use crate::event::{Bus, Event};
use crate::protocol::{self, Readiness, ServiceSnapshot};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

/// How startup ended. `arig wait` blocks until this settles, so the failed
/// case carries the reason: the client has no terminal output to read, and an
/// exiting supervisor would otherwise leave it with a closed connection and
/// nothing to report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Startup {
    Pending,
    Ready,
    Failed(String),
}

/// Where a service is in its lifecycle. Rendered into the wire `status`
/// string, which older clients read as free-form text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceState {
    /// Spawning, or waiting on whatever the runtime has to do first.
    Starting,
    Running,
    Stopping,
    /// Stopped because it was asked to be.
    Stopped,
    /// Being rebuilt, with nothing of it running. A build alongside a running
    /// instance leaves the service running, since it is.
    Building,
    /// Gone on its own, worded by the runtime that ran it.
    Exited(String),
}

impl ServiceState {
    pub fn as_str(&self) -> &str {
        match self {
            ServiceState::Starting => "starting",
            ServiceState::Running => protocol::RUNNING,
            ServiceState::Stopping => "stopping",
            ServiceState::Stopped => "stopped",
            ServiceState::Building => "building",
            ServiceState::Exited(status) => status,
        }
    }
}

/// What the operator asked for, as opposed to what is true. Without it a
/// service that was stopped on purpose and one that died look the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desired {
    Up,
    Stopped,
}

impl Desired {
    pub fn as_str(self) -> &'static str {
        match self {
            Desired::Up => "up",
            Desired::Stopped => "stopped",
        }
    }
}

/// One tracked service. `arig ps` renders this, but the row holds a little
/// more than the wire carries: uptime is an instant here and seconds there.
struct Row {
    name: String,
    kind: String,
    wave: usize,
    pid: Option<u32>,
    state: ServiceState,
    desired: Desired,
    ready: Readiness,
    restarts: u64,
    /// When the current instance started, and `None` once it is gone.
    started_at: Option<Instant>,
    depends_on: Vec<String>,
}

/// What `arig ps` reports, derived from the event stream so the bus stays the
/// only record of service state.
#[derive(Clone)]
pub struct StateTracker {
    services: Arc<Mutex<Vec<Row>>>,
    /// Settles once the kernel knows how startup ended. Set by the kernel
    /// rather than derived from the bus: a waiter must not be answered before
    /// the exits that decide the verdict have been applied, and events reach
    /// the tracker on a task of their own.
    startup: Arc<watch::Sender<Startup>>,
}

impl Default for StateTracker {
    fn default() -> Self {
        Self {
            services: Arc::default(),
            startup: Arc::new(watch::channel(Startup::Pending).0),
        }
    }
}

impl StateTracker {
    pub fn snapshot(&self) -> Vec<ServiceSnapshot> {
        self.services
            .lock()
            .expect("state mutex poisoned")
            .iter()
            .map(|row| ServiceSnapshot {
                name: row.name.clone(),
                kind: row.kind.clone(),
                wave: row.wave,
                pid: row.pid,
                status: row.state.as_str().to_string(),
                desired: Some(row.desired.as_str().to_string()),
                ready: row.ready,
                restarts: row.restarts,
                uptime_secs: row.started_at.map(|t| t.elapsed().as_secs()),
                depends_on: row.depends_on.clone(),
            })
            .collect()
    }

    /// Record how startup ended. The first verdict wins: a service that dies
    /// after the stack came up is a running stack that failed, not a startup
    /// that did, and a waiter has already been told it was ready.
    pub fn finish_startup(&self, outcome: Startup) {
        self.startup.send_if_modified(|current| {
            if *current != Startup::Pending {
                return false;
            }
            *current = outcome;
            true
        });
    }

    /// How startup ended so far, without blocking. A lifecycle command needs
    /// it: the kernel only drains its command channel once it is past startup,
    /// so a command sent before that would hang rather than be refused.
    pub fn startup(&self) -> Startup {
        self.startup.borrow().clone()
    }

    /// Block until startup has settled. Returns immediately for a caller that
    /// arrives after the fact, so a late `arig wait` still gets the verdict.
    pub async fn wait_startup(&self) -> Startup {
        let mut rx = self.startup.subscribe();
        loop {
            {
                let current = rx.borrow_and_update();
                if *current != Startup::Pending {
                    return current.clone();
                }
            }
            if rx.changed().await.is_err() {
                return Startup::Failed("supervisor stopped".to_string());
            }
        }
    }

    pub fn apply(&self, event: &Event) {
        let mut services = self.services.lock().expect("state mutex poisoned");
        match event {
            Event::ServiceStarted {
                name,
                wave,
                kind,
                pid,
                probed,
                depends_on,
            } => {
                let ready = if *probed {
                    Readiness::Pending
                } else {
                    Readiness::Unchecked
                };
                // Upsert: a service that was stopped and started again keeps
                // its row, or `ps` would list it twice.
                match services.iter_mut().find(|s| &s.name == name) {
                    Some(row) => {
                        row.restarts += 1;
                        row.kind = kind.as_str().to_string();
                        row.wave = *wave;
                        row.pid = *pid;
                        row.state = ServiceState::Running;
                        row.desired = Desired::Up;
                        row.ready = ready;
                        row.started_at = Some(Instant::now());
                        row.depends_on = depends_on.clone();
                    }
                    None => services.push(Row {
                        name: name.clone(),
                        kind: kind.as_str().to_string(),
                        wave: *wave,
                        pid: *pid,
                        state: ServiceState::Running,
                        desired: Desired::Up,
                        ready,
                        restarts: 0,
                        started_at: Some(Instant::now()),
                        depends_on: depends_on.clone(),
                    }),
                }
            }
            Event::StartRequested { name } => {
                if let Some(row) = services.iter_mut().find(|s| &s.name == name) {
                    row.state = ServiceState::Starting;
                    row.desired = Desired::Up;
                }
            }
            Event::StopRequested { name } => {
                if let Some(row) = services.iter_mut().find(|s| &s.name == name) {
                    row.state = ServiceState::Stopping;
                    row.desired = Desired::Stopped;
                }
            }
            Event::ServiceStopped { name } => {
                if let Some(row) = services.iter_mut().find(|s| &s.name == name) {
                    row.state = ServiceState::Stopped;
                    row.pid = None;
                    row.started_at = None;
                    // Whatever the probe last said is about an instance that
                    // is gone.
                    row.ready = Readiness::Unchecked;
                }
            }
            Event::ServiceReady { name } => {
                if let Some(service) = services.iter_mut().find(|s| &s.name == name) {
                    service.ready = Readiness::Ready;
                }
            }
            // A failed oneshot keeps its row: the supervisor is about to shut
            // everything down and the row is what `arig ps` shows meanwhile.
            Event::OneshotCompleted { name, success } if *success => {
                services.retain(|s| &s.name != name)
            }
            // Only a service with nothing running has anything to show for a
            // build; one that is up stays up while it builds.
            Event::BuildStarted { name } => {
                if let Some(row) = services.iter_mut().find(|s| &s.name == name)
                    && row.state == ServiceState::Stopped
                {
                    row.state = ServiceState::Building;
                }
            }
            Event::BuildFinished { name } => {
                if let Some(row) = services.iter_mut().find(|s| &s.name == name)
                    && row.state == ServiceState::Building
                {
                    row.state = ServiceState::Stopped;
                }
            }
            Event::ServiceExited { name, status } => {
                if let Some(service) = services.iter_mut().find(|s| &s.name == name) {
                    service.state = ServiceState::Exited(status.clone());
                    service.started_at = None;
                }
            }
            _ => {}
        }
    }
}

/// Subscribe a tracker to the bus for the life of the supervisor.
pub fn spawn(bus: &Bus) -> StateTracker {
    let tracker = StateTracker::default();
    let mut rx = bus.subscribe();
    let sink = tracker.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => sink.apply(&event),
                // Missed events cannot be recovered, so `ps` may show a stale
                // row until the next event for that service arrives. Applying
                // an event is cheap enough that this needs a log flood to
                // happen at all.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
    tracker
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ServiceKind;

    fn started(name: &str, wave: usize, kind: ServiceKind, pid: u32) -> Event {
        Event::ServiceStarted {
            name: name.to_string(),
            wave,
            kind,
            pid: Some(pid),
            probed: false,
            depends_on: Vec::new(),
        }
    }

    fn probed(name: &str) -> Event {
        Event::ServiceStarted {
            name: name.to_string(),
            wave: 0,
            kind: ServiceKind::Service,
            pid: Some(33),
            probed: true,
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn snapshots_are_derived_from_the_event_sequence() {
        let tracker = StateTracker::default();

        tracker.apply(&started("migrate", 0, ServiceKind::Oneshot, 11));
        tracker.apply(&started("api", 1, ServiceKind::Service, 22));
        tracker.apply(&Event::OneshotCompleted {
            name: "migrate".to_string(),
            success: true,
        });

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].name, "api");
        assert_eq!(snapshot[0].kind, "service");
        assert_eq!(snapshot[0].wave, 1);
        assert_eq!(snapshot[0].pid, Some(22));
        assert_eq!(snapshot[0].status, "running");
        assert_eq!(snapshot[0].ready, Readiness::Unchecked);
    }

    #[test]
    fn a_probed_service_is_pending_until_its_probe_passes() {
        let tracker = StateTracker::default();

        tracker.apply(&probed("db"));
        assert_eq!(tracker.snapshot()[0].ready, Readiness::Pending);

        tracker.apply(&Event::ServiceReady {
            name: "db".to_string(),
        });
        assert_eq!(tracker.snapshot()[0].ready, Readiness::Ready);
    }

    #[tokio::test]
    async fn a_waiter_blocks_until_the_kernel_records_a_verdict() {
        let tracker = StateTracker::default();
        let waiter = tokio::spawn({
            let tracker = tracker.clone();
            async move { tracker.wait_startup().await }
        });

        // Service state moving on its own is not a verdict.
        tracker.apply(&probed("db"));
        tracker.apply(&Event::ServiceReady {
            name: "db".to_string(),
        });
        assert!(!waiter.is_finished());

        tracker.finish_startup(Startup::Ready);
        assert_eq!(waiter.await.expect("waiter task"), Startup::Ready);
    }

    #[tokio::test]
    async fn a_late_waiter_still_gets_the_verdict() {
        let tracker = StateTracker::default();
        // Nothing is subscribed at this point, which is the ordinary case:
        // `arig wait` usually connects after the supervisor has settled.
        tracker.finish_startup(Startup::Ready);

        assert_eq!(tracker.wait_startup().await, Startup::Ready);
    }

    #[tokio::test]
    async fn a_failed_startup_carries_its_reason() {
        let tracker = StateTracker::default();
        tracker.finish_startup(Startup::Failed("probe gave up".to_string()));

        assert_eq!(
            tracker.wait_startup().await,
            Startup::Failed("probe gave up".to_string())
        );
    }

    #[tokio::test]
    async fn a_service_dying_after_startup_does_not_rewrite_the_verdict() {
        let tracker = StateTracker::default();
        tracker.finish_startup(Startup::Ready);
        // The stack came up and then broke. `arig wait` has already been
        // answered, and startup is not retroactively a failure.
        tracker.finish_startup(Startup::Failed("too late".to_string()));

        assert_eq!(tracker.wait_startup().await, Startup::Ready);
    }

    #[test]
    fn a_failed_oneshot_keeps_its_row() {
        let tracker = StateTracker::default();

        tracker.apply(&started("migrate", 0, ServiceKind::Oneshot, 11));
        tracker.apply(&Event::OneshotCompleted {
            name: "migrate".to_string(),
            success: false,
        });

        assert_eq!(tracker.snapshot().len(), 1);
    }

    /// A service stopped on purpose and one that died have to look different;
    /// that is what desired state is for.
    #[test]
    fn a_deliberate_stop_is_told_apart_from_a_service_that_died() {
        let tracker = StateTracker::default();
        tracker.apply(&started("api", 0, ServiceKind::Service, 22));
        tracker.apply(&started("db", 0, ServiceKind::Service, 23));

        tracker.apply(&Event::StopRequested {
            name: "api".to_string(),
        });
        assert_eq!(tracker.snapshot()[0].status, "stopping");
        tracker.apply(&Event::ServiceStopped {
            name: "api".to_string(),
        });
        tracker.apply(&Event::ServiceExited {
            name: "db".to_string(),
            status: "exit code 1".to_string(),
        });

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot[0].status, "stopped");
        assert_eq!(snapshot[0].desired.as_deref(), Some("stopped"));
        assert_eq!(snapshot[0].uptime_secs, None);
        assert_eq!(snapshot[1].status, "exit code 1");
        assert_eq!(snapshot[1].desired.as_deref(), Some("up"));
    }

    /// A restarted service is the same service. A second row would show up in
    /// `ps` as a second service that never goes away.
    #[test]
    fn starting_a_service_again_reuses_its_row() {
        let tracker = StateTracker::default();

        tracker.apply(&started("api", 0, ServiceKind::Service, 22));
        tracker.apply(&Event::ServiceExited {
            name: "api".to_string(),
            status: "stopped".to_string(),
        });
        tracker.apply(&started("api", 0, ServiceKind::Service, 23));

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].pid, Some(23));
        assert_eq!(snapshot[0].status, "running");
        assert_eq!(snapshot[0].restarts, 1);
    }

    #[test]
    fn a_running_service_reports_an_uptime_and_a_stopped_one_does_not() {
        let tracker = StateTracker::default();

        tracker.apply(&started("api", 0, ServiceKind::Service, 22));
        assert!(tracker.snapshot()[0].uptime_secs.is_some());

        tracker.apply(&Event::ServiceExited {
            name: "api".to_string(),
            status: "exit code 0".to_string(),
        });
        assert_eq!(tracker.snapshot()[0].uptime_secs, None);
    }

    #[test]
    fn exit_updates_the_status_in_place() {
        let tracker = StateTracker::default();

        tracker.apply(&started("api", 0, ServiceKind::Service, 22));
        tracker.apply(&Event::ServiceExited {
            name: "api".to_string(),
            status: "exit status: 0".to_string(),
        });

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].status, "exit status: 0");
    }

    #[tokio::test]
    async fn tracker_follows_the_bus() {
        let bus = Bus::new(16);
        let tracker = spawn(&bus);

        bus.emit(started("api", 0, ServiceKind::Service, 22));

        for _ in 0..200 {
            if !tracker.snapshot().is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("tracker never observed the event");
    }
}
