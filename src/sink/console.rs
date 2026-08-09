//! The terminal: arig's own lines and whatever the services print.

use super::LogSink;
use crate::event::{Event, Stream};
use std::io::Write;

pub struct ConsoleSink;

impl LogSink for ConsoleSink {
    fn write(&mut self, event: &Event) {
        match event {
            // arig's own lines go to stderr, so a service's stdout is still
            // the only thing on arig's stdout and stays pipeable.
            Event::Supervisor { line } => err(line),
            Event::LogLine {
                service,
                stream,
                line,
            } => {
                let line = format!("[{service}] {line}");
                match stream {
                    Stream::Stdout => out(&line),
                    Stream::Stderr => err(&line),
                }
            }
            _ => {}
        }
    }

    fn dropped(&mut self, count: u64) {
        err(&format!("arig: {count} event(s) dropped"));
    }
}

// `println!` panics if the write fails, which for `arig up | head` would take
// the whole sink task down and with it the log files. There is nowhere left to
// report a console write that failed, so drop it and keep going.
fn out(line: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}

fn err(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}
