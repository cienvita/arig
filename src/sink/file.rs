//! A directory per session: `_arig.log` for arig's own lines, and
//! `<service>.log` for each service's output.

use super::LogSink;
use crate::event::{Bus, Event};
use chrono::Local;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The session-wide log, named to sort above the per-service ones.
const SESSION_LOG: &str = "_arig";

pub fn create_session_dir(base: &Path) -> anyhow::Result<PathBuf> {
    let stamp = Local::now().format("%Y%m%d%H%M%S%3f").to_string();
    let dir = base.join(&stamp);
    std::fs::create_dir_all(&dir)?;

    // Best-effort `latest` pointer to the freshest run. On Windows this
    // needs Developer Mode (or admin) to create a symlink; on failure we just
    // skip silently - the timestamped dir is the source of truth.
    let latest = base.join("latest");
    let _ = std::fs::remove_file(&latest);
    let _ = std::fs::remove_dir(&latest);
    let _ = create_dir_link(&dir, &latest);

    Ok(dir)
}

#[cfg(windows)]
fn create_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    // Use a relative target so the link survives if the logs dir is moved.
    let rel = target
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| target.to_path_buf());
    std::os::windows::fs::symlink_dir(rel, link)
}

#[cfg(unix)]
fn create_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    let rel = target
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| target.to_path_buf());
    std::os::unix::fs::symlink(rel, link)
}

pub struct FileSink {
    dir: PathBuf,
    bus: Bus,
    /// One open file per service, plus the session log. `None` is one that
    /// could not be opened and has already been complained about.
    files: HashMap<String, Option<File>>,
}

impl FileSink {
    pub fn new(dir: PathBuf, bus: Bus) -> Self {
        Self {
            dir,
            bus,
            files: HashMap::new(),
        }
    }

    fn write_line(&mut self, name: &str, line: &str) {
        if let Some(file) = self.file(name) {
            let _ = writeln!(file, "{line}");
        }
    }

    fn file(&mut self, name: &str) -> Option<&mut File> {
        if !self.files.contains_key(name) {
            let opened = open(&self.dir, name);
            let failure = opened.as_ref().err().map(|e| e.to_string());
            // Record the outcome before saying anything: the complaint comes
            // back round as an event, and a second attempt to open the same
            // file would complain about it again.
            self.files.insert(name.to_string(), opened.ok());
            if let Some(err) = failure {
                self.bus.supervisor(format!(
                    "arig: cannot write {name}.log ({err}); output for it is console-only"
                ));
            }
        }
        self.files.get_mut(name)?.as_mut()
    }
}

impl LogSink for FileSink {
    fn write(&mut self, event: &Event) {
        match event {
            Event::Supervisor { line } => self.write_line(SESSION_LOG, line),
            Event::LogLine { service, line, .. } => self.write_line(service, line),
            // Open the file up front so `tail -f` on it works from the moment
            // the service starts, not from its first line of output.
            Event::ServiceStarted { name, .. } => {
                self.file(name);
            }
            _ => {}
        }
    }

    fn dropped(&mut self, count: u64) {
        // Say so in the file rather than leaving a silent gap between two
        // unrelated lines.
        self.write_line(SESSION_LOG, &format!("arig: {count} event(s) dropped"));
    }
}

fn open(dir: &Path, name: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{name}.log")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Cursor, ServiceKind, Stream, event};
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("arig-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(format!("{name}.log"))).expect("read log")
    }

    /// Attach a file sink to `bus` and return the cursor to drain it with.
    fn attach(bus: &Bus, dir: &Path) -> Cursor {
        crate::sink::spawn(
            bus,
            vec![Box::new(FileSink::new(dir.to_path_buf(), bus.clone()))],
        )
    }

    #[tokio::test]
    async fn arig_lines_and_service_output_go_to_separate_files() {
        let dir = temp_dir("file-sink");
        let bus = Bus::new(16);
        let cursor = attach(&bus, &dir);

        event!(bus, "arig: first");
        bus.emit(Event::LogLine {
            service: "api".to_string(),
            stream: Stream::Stdout,
            line: "listening".to_string(),
        });
        bus.emit(Event::LogLine {
            service: "api".to_string(),
            stream: Stream::Stderr,
            line: "warning".to_string(),
        });
        event!(bus, "arig: second");
        bus.drain(&cursor, Duration::from_secs(5)).await;

        assert_eq!(
            read(&dir, SESSION_LOG).lines().collect::<Vec<_>>(),
            ["arig: first", "arig: second"]
        );
        // Both streams interleave in the service's own file, in bus order.
        assert_eq!(
            read(&dir, "api").lines().collect::<Vec<_>>(),
            ["listening", "warning"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_service_gets_a_file_before_it_has_written_anything() {
        let dir = temp_dir("file-sink-empty");
        let bus = Bus::new(16);
        let cursor = attach(&bus, &dir);

        bus.emit(Event::ServiceStarted {
            name: "api".to_string(),
            wave: 0,
            kind: ServiceKind::Service,
            pid: Some(1),
            probed: false,
            depends_on: Vec::new(),
        });
        bus.drain(&cursor, Duration::from_secs(5)).await;

        assert_eq!(read(&dir, "api"), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dropped_events_are_noted_in_the_session_log() {
        let dir = temp_dir("file-sink-lagged");
        let bus = Bus::new(2);
        let cursor = attach(&bus, &dir);

        // Emit faster than the sink task can be scheduled: nothing yields
        // between these, so the two-slot channel overflows.
        for i in 0..6 {
            event!(bus, "arig: {i}");
        }
        bus.drain(&cursor, Duration::from_secs(5)).await;

        let contents = read(&dir, SESSION_LOG);
        assert!(contents.contains("event(s) dropped"), "got: {contents}");
        assert!(contents.contains("arig: 5"), "got: {contents}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
