use crate::event::{Bus, Cursor, Event};
use chrono::Local;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::broadcast::error::RecvError;

pub const TAIL_LINES: usize = 50;

/// Ring of the most recent lines from one service, dumped when it fails.
pub type LogTail = Arc<Mutex<VecDeque<String>>>;
pub type LogFile = Arc<Mutex<File>>;
/// When a service last wrote anything, used by the oneshot heartbeat.
pub type LastOutput = Arc<Mutex<Instant>>;

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

pub fn open_log_file(session_dir: &Path, name: &str) -> anyhow::Result<LogFile> {
    let path = session_dir.join(format!("{name}.log"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(Arc::new(Mutex::new(file)))
}

pub fn new_tail() -> LogTail {
    Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)))
}

pub fn write_log_line(file: &LogFile, line: &str) {
    if let Ok(mut f) = file.lock() {
        let _ = writeln!(*f, "{line}");
    }
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

/// Session-wide `_arig.log`: a subscriber that writes every `arig: ...` line
/// the supervisor emits.
pub struct SessionLog {
    cursor: Cursor,
}

impl SessionLog {
    pub fn spawn(bus: &Bus, file: LogFile) -> Self {
        let cursor = Cursor::default();
        let mut rx = bus.subscribe();
        let sink = cursor.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(Event::Supervisor { line }) => {
                        write_log_line(&file, &line);
                        sink.advance(1);
                    }
                    Ok(_) => sink.advance(1),
                    Err(RecvError::Lagged(n)) => {
                        // Say so in the file rather than leaving a silent gap
                        // between two unrelated lines.
                        write_log_line(&file, &format!("arig: {n} event(s) dropped"));
                        sink.advance(n);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        Self { cursor }
    }

    /// Cursor to hand to `Bus::drain` before the process exits.
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("arig-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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

    #[tokio::test]
    async fn session_log_records_supervisor_lines() {
        let dir = temp_dir("session-log");
        let bus = Bus::new(16);
        let log = SessionLog::spawn(&bus, open_log_file(&dir, "_arig").unwrap());

        bus.supervisor("arig: first".to_string());
        bus.emit(Event::ShutdownRequested);
        bus.supervisor("arig: second".to_string());
        bus.drain(log.cursor(), Duration::from_secs(5)).await;

        let contents = std::fs::read_to_string(dir.join("_arig.log")).unwrap();
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            ["arig: first", "arig: second"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_log_notes_dropped_events() {
        let dir = temp_dir("session-log-lagged");
        let bus = Bus::new(2);
        let log = SessionLog::spawn(&bus, open_log_file(&dir, "_arig").unwrap());

        // Emit faster than the sink task can be scheduled: nothing yields
        // between these, so the two-slot channel overflows.
        for i in 0..6 {
            bus.supervisor(format!("arig: {i}"));
        }
        bus.drain(log.cursor(), Duration::from_secs(5)).await;

        let contents = std::fs::read_to_string(dir.join("_arig.log")).unwrap();
        assert!(contents.contains("event(s) dropped"), "got: {contents}");
        assert!(contents.contains("arig: 5"), "got: {contents}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
