//! Where events turn into output. A sink sees the whole stream in order and
//! decides what to write; the kernel does not know what is attached to it.

pub mod console;
pub mod file;

use crate::event::{Bus, Cursor, Event};
use tokio::sync::broadcast::error::RecvError;

pub trait LogSink: Send {
    /// One event, in the order it was emitted.
    fn write(&mut self, event: &Event);

    /// Events this sink will never see, because the bus outran it. Sinks
    /// report the gap rather than resuming mid-stream as if nothing happened.
    fn dropped(&mut self, count: u64);
}

/// Feed every sink from one task, so they all see the same events in the same
/// order. The returned cursor is what `Bus::drain` waits on before exit.
pub fn spawn(bus: &Bus, mut sinks: Vec<Box<dyn LogSink>>) -> Cursor {
    let cursor = Cursor::default();
    let mut rx = bus.subscribe();
    let progress = cursor.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    for sink in &mut sinks {
                        sink.write(&event);
                    }
                    progress.advance(1);
                }
                Err(RecvError::Lagged(n)) => {
                    for sink in &mut sinks {
                        sink.dropped(n);
                    }
                    progress.advance(n);
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
    cursor
}
