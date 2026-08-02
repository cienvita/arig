//! Readiness checks. The kernel keeps the polling policy - how often to retry,
//! how long to keep at it, when to say something while it waits - and a probe
//! owns one attempt against one service.

pub mod tcp;

use crate::config::ReadyProbe;
use async_trait::async_trait;

/// A probe bound to one service's `ready:` block, ready to be polled.
#[async_trait]
pub trait ReadyCheck: Send + Sync {
    /// What is being waited on, for the log lines. e.g. "127.0.0.1:5432".
    fn target(&self) -> &str;

    /// One attempt. `Err` carries why the service is not ready yet, which the
    /// kernel repeats while it waits and in the give-up message.
    async fn check(&self) -> Result<(), String>;
}

pub trait Probe: Send + Sync {
    /// The kind of check this performs, and its key in the registry.
    fn name(&self) -> &'static str;

    /// Whether a `ready:` block asks for this kind of check. Exactly one
    /// registered probe may claim a block.
    fn claims(&self, spec: &ReadyProbe) -> bool;

    /// Bind to a block this probe claims, rejecting one it cannot use.
    fn prepare(&self, spec: &ReadyProbe) -> anyhow::Result<Box<dyn ReadyCheck>>;
}
