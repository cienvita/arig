use crate::event::{Bus, Event};
use crate::protocol::ServiceSnapshot;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast::error::RecvError;

/// What `arig ps` reports, derived from the event stream so the bus stays the
/// only record of service state.
#[derive(Clone, Default)]
pub struct StateTracker {
    services: Arc<Mutex<Vec<ServiceSnapshot>>>,
}

impl StateTracker {
    pub fn snapshot(&self) -> Vec<ServiceSnapshot> {
        self.services.lock().expect("state mutex poisoned").clone()
    }

    pub fn apply(&self, event: &Event) {
        let mut services = self.services.lock().expect("state mutex poisoned");
        match event {
            Event::ServiceStarted {
                name,
                wave,
                kind,
                pid,
            } => services.push(ServiceSnapshot {
                name: name.clone(),
                kind: kind.as_str().to_string(),
                wave: *wave,
                pid: *pid,
                status: "running".to_string(),
            }),
            // A failed oneshot keeps its row: the supervisor is about to shut
            // everything down and the row is what `arig ps` shows meanwhile.
            Event::OneshotCompleted { name, success } if *success => {
                services.retain(|s| &s.name != name)
            }
            Event::ServiceExited { name, status } => {
                if let Some(service) = services.iter_mut().find(|s| &s.name == name) {
                    service.status = status.clone();
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
