//! Where the kernel hands a service over to something that can actually run
//! it. The kernel keeps the scheduling policy - what starts in which wave,
//! how long a oneshot may take, what order things stop in - and a runtime
//! owns the mechanics of one service: spawning it, waiting on it, and getting
//! it to stop.

pub mod process;

use crate::config::ServiceConfig;
use async_trait::async_trait;
use std::process::ExitStatus;
use tokio::io::AsyncRead;

/// A service's output, handed to the kernel to pipe into the logs.
pub type OutputStream = Box<dyn AsyncRead + Send + Unpin>;

pub struct SpawnedService {
    pub handle: Box<dyn RunningService>,
    pub stdout: Option<OutputStream>,
    pub stderr: Option<OutputStream>,
}

/// How a service ended once it was asked to stop.
pub enum StopOutcome {
    /// It exited on its own, after whatever the runtime does to ask nicely.
    Exited(ExitStatus),
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

    async fn wait(&mut self) -> anyhow::Result<ExitStatus>;

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
