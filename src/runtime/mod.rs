//! Where the kernel hands a service over to something that can actually run
//! it. The kernel keeps the scheduling policy - what starts in which wave,
//! how long a oneshot may take, what order things stop in - and a runtime
//! owns the mechanics of one service: spawning it, waiting on it, and getting
//! it to stop.

pub mod process;

use crate::config::ServiceConfig;
use async_trait::async_trait;
use std::fmt;
use std::process::ExitStatus;
use tokio::io::AsyncRead;

/// A service's output, handed to the kernel to pipe into the logs.
pub type OutputStream = Box<dyn AsyncRead + Send + Unpin>;

pub struct SpawnedService {
    pub handle: Box<dyn RunningService>,
    pub stdout: Option<OutputStream>,
    pub stderr: Option<OutputStream>,
}

/// How a service ended. The kernel asks only whether it succeeded and how to
/// name it in a log line, so a runtime whose services are not local processes
/// has no `ExitStatus` to invent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    success: bool,
    /// How the runtime words this exit. Repeated verbatim in log lines, so it
    /// keeps whatever detail the runtime has: a signal name, a docker status.
    description: String,
}

impl Exit {
    /// An exit the runtime describes by a status code alone.
    // Used by the tests until the docker runtime, the first runtime that gets
    // a bare code back, lands in the next commit.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn from_code(code: i64) -> Self {
        Self {
            success: code == 0,
            description: format!("exit code {code}"),
        }
    }

    pub fn success(&self) -> bool {
        self.success
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.description)
    }
}

impl From<ExitStatus> for Exit {
    fn from(status: ExitStatus) -> Self {
        Self {
            success: status.success(),
            // Keeps the platform wording, including the signal on unix.
            description: status.to_string(),
        }
    }
}

/// How a service ended once it was asked to stop.
pub enum StopOutcome {
    /// It exited on its own, after whatever the runtime does to ask nicely.
    Exited(Exit),
    /// It outlasted every graceful path and was killed.
    Killed,
}

#[async_trait]
pub trait Runtime: Send + Sync {
    /// The name services select this runtime by, and its key in the registry.
    fn name(&self) -> &'static str;

    async fn spawn(&self, name: &str, spec: &ServiceConfig) -> anyhow::Result<SpawnedService>;
}

#[async_trait]
pub trait RunningService: Send {
    fn pid(&self) -> Option<u32>;

    async fn wait(&mut self) -> anyhow::Result<Exit>;

    /// Set the service on its way out without blocking. The kernel calls this
    /// for every service in a wave before it waits on any of them, so a wave
    /// stops concurrently rather than one service at a time.
    fn begin_stop(&mut self);

    /// Wait for the service to actually be gone, escalating as far as a kill
    /// if it will not go. Always preceded by `begin_stop`.
    async fn finish_stop(&mut self) -> StopOutcome;

    /// Kill it outright, no grace period. Used when a oneshot overruns its
    /// timeout; reaps the process before returning.
    async fn kill(&mut self);
}
