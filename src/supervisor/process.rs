use super::logs::{LastOutput, LogFile, LogTail, mark_output, push_tail, write_log_line};
use crate::config::{ServiceConfig, ServiceType};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub fn spawn_service(service: &ServiceConfig) -> anyhow::Result<tokio::process::Child> {
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

/// Read a child's stdout and stderr line by line, fanning each line out to the
/// console, the service's log file, and its tail ring.
pub fn pipe_output(
    child: &mut tokio::process::Child,
    name: &str,
    tail: &LogTail,
    log_file: &LogFile,
    last_output: &LastOutput,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let n = name.to_string();
        let t = tail.clone();
        let f = log_file.clone();
        let lo = last_output.clone();
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("[{n}] {line}");
                write_log_line(&f, &line);
                push_tail(&t, line);
                mark_output(&lo);
            }
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        let n = name.to_string();
        let t = tail.clone();
        let f = log_file.clone();
        let lo = last_output.clone();
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{n}] {line}");
                write_log_line(&f, &line);
                push_tail(&t, line);
                mark_output(&lo);
            }
        }));
    }
    tasks
}

/// Ask a child to stop: CTRL_BREAK on Windows, SIGTERM to its process group on
/// unix. Both rely on `configure_child` having given it its own group.
pub fn send_shutdown_signal(child: &tokio::process::Child) {
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
pub fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    unix::send_sigkill(pid);

    #[cfg(windows)]
    let _ = pid;
}

pub fn configure_child(cmd: &mut Command) {
    #[cfg(windows)]
    win::configure_child(cmd);

    #[cfg(unix)]
    unix::configure_child(cmd);
}

pub fn shell_program() -> &'static str {
    if cfg!(windows) { "cmd" } else { "sh" }
}

pub fn shell_args(command: &str) -> Vec<&str> {
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
pub mod win {
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
    use std::collections::HashMap;
    use std::time::Duration;

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
            spawn_service(&svc(stdin_reader(), ServiceType::Oneshot)).expect("spawn oneshot");

        // The regression is a hang, so the assertion is that it terminates at
        // all. Exit status is up to the command's own EOF handling.
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("oneshot stdin should be at EOF, not waiting on the terminal")
            .expect("wait on oneshot");
    }
}
