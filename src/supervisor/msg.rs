//! What reaches the kernel loop from outside it: lifecycle commands from IPC
//! clients, and the completions of the phases those commands run.

use tokio::sync::oneshot;

/// A lifecycle command, as the kernel sees it. Kept apart from the wire
/// `Request` so the kernel's vocabulary is not the protocol's, and closed on
/// purpose: a new verb is a new variant.
pub enum LifecycleReq {
    Stop {
        service: String,
    },
    Start {
        service: String,
        no_wait: bool,
    },
    Restart {
        service: String,
        build: bool,
        no_wait: bool,
    },
    Build {
        service: String,
    },
}

impl LifecycleReq {
    pub fn service(&self) -> &str {
        match self {
            LifecycleReq::Stop { service }
            | LifecycleReq::Start { service, .. }
            | LifecycleReq::Restart { service, .. }
            | LifecycleReq::Build { service } => service,
        }
    }

    /// Whether the client wants an answer as soon as the service has spawned
    /// rather than once its readiness probe has passed.
    pub fn no_wait(&self) -> bool {
        match self {
            LifecycleReq::Stop { .. } | LifecycleReq::Build { .. } => false,
            LifecycleReq::Start { no_wait, .. } | LifecycleReq::Restart { no_wait, .. } => *no_wait,
        }
    }
}

/// One step of a lifecycle command. A command is a sequence of these, so
/// restart is build, stop and start rather than a case of its own.
#[derive(PartialEq, Eq)]
pub enum Phase {
    Build,
    Stop,
    Start,
}

/// Which command a completion belongs to. A phase runs outside the kernel
/// loop, so its message can arrive after the command that started it is over;
/// the kernel drops one that no longer matches rather than advancing whatever
/// command holds the service now.
pub type Seq = u64;

/// What the kernel handles between service exits. Everything that has to
/// reach the loop arrives as one of these, since the loop owns the services
/// and nothing else can touch them.
pub enum KernelMsg {
    Lifecycle {
        req: LifecycleReq,
        /// Answered exactly once, on every path out of the command. A client
        /// whose reply never comes waits for its own timeout instead.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// A service the kernel handed to a stop task is gone.
    StopFinished { name: String, seq: Seq },
    /// A readiness probe the kernel started has passed or given up.
    ProbeSettled {
        name: String,
        seq: Seq,
        result: Result<(), String>,
    },
    /// A build the kernel started has ended.
    BuildFinished {
        name: String,
        seq: Seq,
        result: Result<(), String>,
    },
}
