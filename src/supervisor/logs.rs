use crate::event::{Bus, Event, Stream};
use crate::runtime::SpawnedService;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};

pub const TAIL_LINES: usize = 50;

/// Ring of the most recent lines from one service, dumped when it fails.
pub type LogTail = Arc<Mutex<VecDeque<String>>>;
/// When a service last wrote anything, used by the oneshot heartbeat.
pub type LastOutput = Arc<Mutex<Instant>>;

pub fn new_tail() -> LogTail {
    Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)))
}

pub fn push_tail(tail: &LogTail, line: String) {
    let mut q = tail.lock().expect("tail mutex poisoned");
    if q.len() >= TAIL_LINES {
        q.pop_front();
    }
    q.push_back(line);
}

pub fn mark_output(last_output: &LastOutput) {
    if let Ok(mut t) = last_output.lock() {
        *t = Instant::now();
    }
}

/// Read a service's stdout and stderr line by line onto the bus, where the
/// sinks pick them up. The tail ring and the output clock are filled here
/// rather than by a sink because the kernel reads them directly: one for the
/// dump after a failure, the other for the oneshot heartbeat.
pub fn pipe_output(
    spawned: &mut SpawnedService,
    name: &str,
    tail: &LogTail,
    last_output: &LastOutput,
    bus: &Bus,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    for (stream, source) in [
        (Stream::Stdout, spawned.stdout.take()),
        (Stream::Stderr, spawned.stderr.take()),
    ] {
        let Some(source) = source else { continue };
        let name = name.to_string();
        let tail = tail.clone();
        let last_output = last_output.clone();
        let bus = bus.clone();
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(source).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                push_tail(&tail, line.clone());
                mark_output(&last_output);
                bus.emit(Event::LogLine {
                    service: name.clone(),
                    stream,
                    line,
                });
            }
        }));
    }
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines_only() {
        let tail = new_tail();
        for i in 0..TAIL_LINES + 10 {
            push_tail(&tail, format!("line {i}"));
        }

        let q = tail.lock().unwrap();
        assert_eq!(q.len(), TAIL_LINES);
        assert_eq!(q.front().unwrap(), "line 10");
        assert_eq!(q.back().unwrap(), &format!("line {}", TAIL_LINES + 9));
    }
}
