use crate::config::ServiceType;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

/// Log lines share the bus with lifecycle events, so capacity is sized for a
/// burst of service output rather than a handful of state changes.
pub const CAPACITY: usize = 4096;

/// How often `Bus::drain` re-checks a subscriber's cursor.
const DRAIN_POLL: Duration = Duration::from_millis(2);

/// Lifecycle shape of a service. Mirrors `ServiceType` so subscribers don't
/// have to depend on the config types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    Service,
    Oneshot,
}

impl ServiceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceKind::Service => "service",
            ServiceKind::Oneshot => "oneshot",
        }
    }
}

impl From<&ServiceType> for ServiceKind {
    fn from(service_type: &ServiceType) -> Self {
        match service_type {
            ServiceType::Service => ServiceKind::Service,
            ServiceType::Oneshot => ServiceKind::Oneshot,
        }
    }
}

/// What the supervisor observes. Variants land here as their consumers do:
/// service output (`LogLine`) and readiness follow with the log sinks, since a
/// variant nothing reads is dead code under `-D warnings`.
#[derive(Debug, Clone)]
pub enum Event {
    ServiceStarted {
        name: String,
        wave: usize,
        kind: ServiceKind,
        pid: u32,
    },
    OneshotCompleted {
        name: String,
        success: bool,
    },
    ServiceExited {
        name: String,
        status: String,
    },
    /// One of arig's own `arig: ...` lines.
    Supervisor {
        line: String,
    },
    ShutdownRequested,
}

/// Fan-out point for everything the supervisor observes. Subscribers get their
/// own queue and are expected to keep up; one that falls behind sees
/// `RecvError::Lagged` and decides for itself how to report the gap.
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
    emitted: Arc<AtomicU64>,
}

impl Bus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self {
            tx,
            emitted: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn emit(&self, event: Event) {
        self.emitted.fetch_add(1, Ordering::SeqCst);
        // `send` only fails when every receiver has been dropped, which is not
        // an error: the supervisor still runs without any sink attached.
        let _ = self.tx.send(event);
    }

    /// Emit one of arig's own lines. The console write stays direct here and
    /// becomes a sink later; the bus copy is what reaches the session log.
    pub fn supervisor(&self, line: String) {
        eprintln!("{line}");
        self.emit(Event::Supervisor { line });
    }

    /// Total events emitted so far. Compared against a subscriber's cursor to
    /// tell whether it has caught up.
    pub fn emitted(&self) -> u64 {
        self.emitted.load(Ordering::SeqCst)
    }

    /// Wait until `cursor` has consumed everything emitted so far, or until
    /// `timeout` elapses. Subscribers run in their own tasks, so without this
    /// the process can exit with its last lines still queued.
    pub async fn drain(&self, cursor: &Cursor, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        let target = self.emitted();
        while cursor.get() < target {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(DRAIN_POLL).await;
        }
    }
}

/// How many events a subscriber has consumed, counting the ones it missed
/// after lagging.
#[derive(Clone, Default)]
pub struct Cursor(Arc<AtomicU64>);

impl Cursor {
    pub fn advance(&self, n: u64) {
        self.0.fetch_add(n, Ordering::SeqCst);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

macro_rules! event {
    ($bus:expr, $($arg:tt)*) => {
        $bus.supervisor(format!($($arg)*))
    };
}

pub(crate) use event;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::RecvError;

    fn line(event: &Event) -> String {
        match event {
            Event::Supervisor { line } => line.clone(),
            other => panic!("expected a supervisor line, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscriber_receives_emitted_events() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();

        bus.emit(Event::ShutdownRequested);
        event!(bus, "arig: hello {}", 1);

        assert!(matches!(rx.recv().await, Ok(Event::ShutdownRequested)));
        assert_eq!(line(&rx.recv().await.unwrap()), "arig: hello 1");
        assert_eq!(bus.emitted(), 2);
    }

    #[tokio::test]
    async fn emitting_without_subscribers_is_not_an_error() {
        let bus = Bus::new(4);
        bus.emit(Event::ShutdownRequested);
        assert_eq!(bus.emitted(), 1);
    }

    #[tokio::test]
    async fn slow_subscriber_is_told_how_many_events_it_missed() {
        let bus = Bus::new(2);
        let mut rx = bus.subscribe();

        for i in 0..4 {
            event!(bus, "arig: {i}");
        }

        // The two oldest are gone; the receiver learns the count rather than
        // silently resuming mid-stream.
        assert!(matches!(rx.recv().await, Err(RecvError::Lagged(2))));
        assert_eq!(line(&rx.recv().await.unwrap()), "arig: 2");
        assert_eq!(line(&rx.recv().await.unwrap()), "arig: 3");
    }

    #[tokio::test]
    async fn drain_waits_for_the_subscriber_to_catch_up() {
        let bus = Bus::new(16);
        let cursor = Cursor::default();
        let mut rx = bus.subscribe();
        let c = cursor.clone();
        tokio::spawn(async move {
            while rx.recv().await.is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
                c.advance(1);
            }
        });

        for i in 0..3 {
            event!(bus, "arig: {i}");
        }
        bus.drain(&cursor, Duration::from_secs(5)).await;

        assert_eq!(cursor.get(), 3);
    }

    #[tokio::test]
    async fn drain_gives_up_at_the_timeout() {
        let bus = Bus::new(16);
        let _rx = bus.subscribe();
        bus.emit(Event::ShutdownRequested);

        // Nothing advances the cursor, so this must return on the deadline
        // rather than hang the shutdown path.
        bus.drain(&Cursor::default(), Duration::from_millis(20))
            .await;
    }
}
