//! The built-in runtime: services are commands run through the system shell.

use super::{OutputStream, RunningService, Runtime, SpawnedService, StopOutcome};
use crate::config::{ServiceConfig, ServiceType};
use crate::event::{Bus, event};
use async_trait::async_trait;
use std::collections::HashMap;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::Command;

/// How long a service gets to exit after being signalled before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

pub struct ProcessRuntime {
    bus: Bus,
}

impl ProcessRuntime {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }
}

#[async_trait]
impl Runtime for ProcessRuntime {
    async fn spawn(&self, name: &str, spec: &ServiceConfig) -> anyhow::Result<SpawnedService> {
        let mut child = spawn_child(spec)?;
        let stdout = child.stdout.take().map(|s| Box::new(s) as OutputStream);
        let stderr = child.stderr.take().map(|s| Box::new(s) as OutputStream);
        let hook = spec.shutdown.as_ref().map(|sd| ResolvedShutdown {
            command: sd.command.clone(),
            timeout: sd.timeout,
            working_dir: spec.working_dir.clone(),
            env: spec.env.clone(),
        });

        Ok(SpawnedService {
            handle: Box::new(ProcessChild {
                name: name.to_string(),
                child,
                hook,
                bus: self.bus.clone(),
            }),
            stdout,
            stderr,
        })
    }
}

/// A shutdown hook with the service context it inherits, resolved at spawn
/// time so stopping does not need the config back.
struct ResolvedShutdown {
    command: String,
    timeout: Duration,
    working_dir: Option<String>,
    env: HashMap<String, String>,
}

pub struct ProcessChild {
    name: String,
    child: tokio::process::Child,
    hook: Option<ResolvedShutdown>,
    bus: Bus,
}

#[async_trait]
impl RunningService for ProcessChild {
    fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    async fn wait(&mut self) -> anyhow::Result<ExitStatus> {
        Ok(self.child.wait().await?)
    }

    fn begin_stop(&mut self) {
        // A hooked service is stopped by running its hook, which happens in
        // finish_stop; signalling it here would race that.
        if self.hook.is_none() {
            send_shutdown_signal(&self.child);
        }
    }

    async fn finish_stop(&mut self) -> StopOutcome {
        let Some(hook) = self.hook.take() else {
            // begin_stop already signalled it, so this is just the wait.
            return self.wait_or_kill(STOP_GRACE).await;
        };

        let mut cmd = Command::new(shell_program());
        cmd.args(shell_args(&hook.command))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(ref dir) = hook.working_dir {
            cmd.current_dir(dir);
        }
        cmd.envs(&hook.env);
        configure_child(&mut cmd);

        event!(self.bus, "arig: running shutdown hook for '{}'", self.name);
        let mut hook_child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                event!(
                    self.bus,
                    "arig: shutdown hook for '{}' failed to spawn ({e}), signalling",
                    self.name
                );
                send_shutdown_signal(&self.child);
                // Use the configured hook timeout, not the default grace
                // period, since the operator set it expecting the service to
                // need that long to stop.
                return self.wait_or_kill(hook.timeout).await;
            }
        };

        let stopped = tokio::time::timeout(hook.timeout, self.child.wait()).await;
        // Kill the hook's whole process group, then reap it so it doesn't
        // become a zombie and its subprocesses don't become orphans. On
        // Windows the job object owns the tree, so the kill below covers it.
        if let Some(pid) = hook_child.id() {
            kill_process_group(pid);
        }
        let _ = hook_child.kill().await;
        let _ = hook_child.wait().await;

        if let Ok(Ok(status)) = stopped {
            return StopOutcome::Exited(status);
        }

        // The hook ran but the service is still up. Signal it and give it the
        // grace period before force-killing.
        event!(
            self.bus,
            "arig: {} did not stop after shutdown hook, signalling",
            self.name
        );
        send_shutdown_signal(&self.child);
        self.wait_or_kill(STOP_GRACE).await
    }

    async fn kill(&mut self) {
        // tokio's kill reaps the child as well as signalling it.
        let _ = self.child.kill().await;
    }
}

impl ProcessChild {
    /// Wait out `grace` and kill whatever is left.
    async fn wait_or_kill(&mut self, grace: Duration) -> StopOutcome {
        match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(Ok(status)) => StopOutcome::Exited(status),
            _ => {
                event!(
                    self.bus,
                    "arig: {} did not stop in time, force killing",
                    self.name
                );
                let _ = self.child.kill().await;
                StopOutcome::Killed
            }
        }
    }
}

fn spawn_child(service: &ServiceConfig) -> anyhow::Result<tokio::process::Child> {
    let mut cmd = Command::new(shell_program());
    cmd.args(shell_args(&service.command))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Oneshots get an empty stdin. A command that prompts sees EOF and takes
    // its non-interactive path (or fails loudly) instead of blocking on a read
    // that never resolves and stalling the wave. Long-running services keep the
    // inherited stdin, since the foreground service may legitimately want it.
    if service.service_type == ServiceType::Oneshot {
        cmd.stdin(Stdio::null());
    }

    if let Some(dir) = &service.working_dir {
        cmd.current_dir(dir);
    }
    cmd.envs(&service.env);
    configure_child(&mut cmd);

    Ok(cmd.spawn()?)
}

/// Ask a child to stop: CTRL_BREAK on Windows, SIGTERM to its process group on
/// unix. Both rely on `configure_child` having given it its own group.
fn send_shutdown_signal(child: &tokio::process::Child) {
    let Some(pid) = child.id() else {
        return;
    };

    #[cfg(windows)]
    win::send_ctrl_break(pid);

    #[cfg(unix)]
    unix::send_sigterm(pid);
}

/// Kill a helper process (a shutdown hook) and everything it started. On
/// Windows the job object already owns the tree, so this is a no-op there.
fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    unix::send_sigkill(pid);

    #[cfg(windows)]
    let _ = pid;
}

fn configure_child(cmd: &mut Command) {
    #[cfg(windows)]
    win::configure_child(cmd);

    #[cfg(unix)]
    unix::configure_child(cmd);
}

fn shell_program() -> &'static str {
    if cfg!(windows) { "cmd" } else { "sh" }
}

fn shell_args(command: &str) -> Vec<&str> {
    if cfg!(windows) {
        vec!["/C", command]
    } else {
        vec!["-c", command]
    }
}

// ---------------------------------------------------------------------------
// Windows: GenerateConsoleCtrlEvent
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod win {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetConsoleWindow,
    };
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    fn has_console() -> bool {
        unsafe { !GetConsoleWindow().is_null() }
    }

    pub fn configure_child(cmd: &mut tokio::process::Command) {
        // CREATE_NEW_PROCESS_GROUP: each child leads its own group so we can
        // target it individually with GenerateConsoleCtrlEvent. It also
        // detaches the child from the parent's Ctrl+C; we drive shutdown
        // explicitly via send_ctrl_break.
        // CREATE_NO_WINDOW: a detached supervisor has no console, and without
        // this flag Windows allocates a fresh console window for every cmd.exe
        // child. We only add it when we're console-less so the foreground
        // case keeps sharing a console (CTRL_BREAK_EVENT needs that).
        let mut flags = CREATE_NEW_PROCESS_GROUP;
        if !has_console() {
            flags |= CREATE_NO_WINDOW;
        }
        cmd.creation_flags(flags);
    }

    /// Send CTRL_BREAK_EVENT to a single child's process group.
    /// CTRL_C_EVENT cannot be addressed to a non-zero group on Windows;
    /// CTRL_BREAK_EVENT can, and shutdown handlers in .NET / NATS / docker-CLI
    /// treat it equivalently.
    pub fn send_ctrl_break(pid: u32) {
        unsafe {
            GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
        }
    }
}

// ---------------------------------------------------------------------------
// Unix: process groups + SIGTERM/SIGKILL
// ---------------------------------------------------------------------------
#[cfg(unix)]
mod unix {
    pub fn configure_child(cmd: &mut tokio::process::Command) {
        unsafe {
            cmd.pre_exec(|| {
                // Put child in its own process group so we can signal it
                libc::setpgid(0, 0);
                Ok(())
            });
        }
    }

    pub fn send_sigterm(pid: u32) {
        unsafe {
            // Signal the whole process group
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }

    pub fn send_sigkill(pid: u32) {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(command: &str, service_type: ServiceType) -> ServiceConfig {
        ServiceConfig {
            command: command.to_string(),
            service_type,
            working_dir: None,
            env: HashMap::new(),
            depends_on: Vec::new(),
            ready: None,
            timeout: None,
            shutdown: None,
        }
    }

    // A command that blocks on stdin until it sees EOF. `cat` copies stdin to
    // stdout; `set /p` is a cmd builtin that reads one line, chosen over `more`
    // because the pager waits on the console even when stdin is redirected.
    fn stdin_reader() -> &'static str {
        if cfg!(windows) { "set /p X=" } else { "cat" }
    }

    #[tokio::test]
    async fn oneshot_stdin_is_closed() {
        let mut child =
            spawn_child(&svc(stdin_reader(), ServiceType::Oneshot)).expect("spawn oneshot");

        // The regression is a hang, so the assertion is that it terminates at
        // all. Exit status is up to the command's own EOF handling.
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("oneshot stdin should be at EOF, not waiting on the terminal")
            .expect("wait on oneshot");
    }
}
