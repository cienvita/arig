use crate::config::{ArigConfig, ServiceConfig, ServiceType};
use crate::dag;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::signal;

struct ManagedChild {
    name: String,
    child: tokio::process::Child,
}

pub async fn up(config: ArigConfig) -> anyhow::Result<()> {
    #[cfg(windows)]
    let _job = win::JobGuard::new()?;

    let waves = dag::toposort(&config)?;
    let mut children: Vec<ManagedChild> = Vec::new();
    let mut io_tasks = Vec::new();

    for wave in &waves {
        let mut wave_oneshots: Vec<ManagedChild> = Vec::new();

        for name in wave {
            let service = &config.services[name];
            let mut child = spawn_service(name, service)?;
            pipe_output(&mut child, name, &mut io_tasks);

            if service.service_type == ServiceType::Oneshot {
                wave_oneshots.push(ManagedChild {
                    name: name.clone(),
                    child,
                });
            } else {
                children.push(ManagedChild {
                    name: name.clone(),
                    child,
                });
            }
        }

        // Wait for all oneshots in this wave to finish before next wave
        for mut managed in wave_oneshots {
            let status = managed.child.wait().await?;
            if !status.success() {
                eprintln!("arig: oneshot '{}' failed ({status})", managed.name);
                shutdown(&mut children).await;
                anyhow::bail!("oneshot '{}' failed", managed.name);
            }
            eprintln!("arig: oneshot '{}' completed", managed.name);
        }
    }

    if children.is_empty() {
        eprintln!("arig: all tasks completed.");
        return Ok(());
    }

    eprintln!(
        "arig: {} service(s) running. Press Ctrl+C to stop.",
        children.len()
    );

    signal::ctrl_c().await?;
    eprintln!("\narig: shutting down...");

    shutdown(&mut children).await;

    for task in io_tasks {
        let _ = task.await;
    }

    eprintln!("arig: all services stopped.");
    Ok(())
}

fn pipe_output(
    managed: &mut tokio::process::Child,
    name: &str,
    io_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    if let Some(stdout) = managed.stdout.take() {
        let n = name.to_string();
        io_tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("[{n}] {line}");
            }
        }));
    }
    if let Some(stderr) = managed.stderr.take() {
        let n = name.to_string();
        io_tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{n}] {line}");
            }
        }));
    }
}

fn spawn_service(name: &str, service: &ServiceConfig) -> anyhow::Result<tokio::process::Child> {
    let mut cmd = Command::new(shell_program());
    cmd.args(shell_args(&service.command))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = &service.working_dir {
        cmd.current_dir(dir);
    }
    cmd.envs(&service.env);

    #[cfg(windows)]
    win::configure_child(&mut cmd);

    #[cfg(unix)]
    unix::configure_child(&mut cmd);

    let child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    eprintln!("arig: started {name} (PID {pid})");
    Ok(child)
}

async fn shutdown(children: &mut [ManagedChild]) {
    #[cfg(windows)]
    win::send_ctrl_c();

    #[cfg(unix)]
    for managed in children.iter() {
        unix::send_sigterm(&managed.child);
    }

    // Wait up to 5 seconds per process for graceful exit, then force kill
    for managed in children.iter_mut() {
        let graceful = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            managed.child.wait(),
        )
        .await;

        match graceful {
            Ok(Ok(status)) => {
                eprintln!("arig: {} exited gracefully ({status})", managed.name);
            }
            _ => {
                eprintln!("arig: force killing {}", managed.name);
                let _ = managed.child.kill().await;
            }
        }
    }

    #[cfg(windows)]
    win::restore_ctrl_c();
}

fn shell_program() -> &'static str {
    if cfg!(windows) {
        "cmd"
    } else {
        "sh"
    }
}

fn shell_args(command: &str) -> Vec<&str> {
    if cfg!(windows) {
        vec!["/C", command]
    } else {
        vec!["-c", command]
    }
}

// ---------------------------------------------------------------------------
// Windows: job objects + GenerateConsoleCtrlEvent
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Console::{
        GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_C_EVENT,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// RAII guard that holds the job object handle. Children assigned to this
    /// job are killed when the handle is closed (including on parent crash).
    pub struct JobGuard {
        handle: HANDLE,
    }

    impl JobGuard {
        pub fn new() -> anyhow::Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    anyhow::bail!("CreateJobObjectW failed");
                }

                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(handle);
                    anyhow::bail!("SetInformationJobObject failed");
                }

                // Assign ourselves so children inherit the job
                AssignProcessToJobObject(handle, GetCurrentProcess());

                Ok(Self { handle })
            }
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    pub fn configure_child(_cmd: &mut tokio::process::Command) {
        // Children inherit the job object from the parent process.
        // No extra flags needed since we assigned the parent to the job.
    }

    /// Ignore Ctrl+C in our process, then broadcast CTRL_C_EVENT to the
    /// console process group (which includes our children).
    pub fn send_ctrl_c() {
        unsafe {
            // Ignore Ctrl+C in parent while we send it to children
            SetConsoleCtrlHandler(None, 1); // TRUE = add ignore
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
        }
    }

    /// Re-enable default Ctrl+C handling.
    pub fn restore_ctrl_c() {
        unsafe {
            SetConsoleCtrlHandler(None, 0); // FALSE = remove ignore
        }
    }
}

// ---------------------------------------------------------------------------
// Unix: process groups + SIGTERM/SIGKILL
// ---------------------------------------------------------------------------
#[cfg(unix)]
mod unix {
    use std::os::unix::process::CommandExt;

    pub fn configure_child(cmd: &mut tokio::process::Command) {
        unsafe {
            cmd.pre_exec(|| {
                // Put child in its own process group so we can signal it
                libc::setpgid(0, 0);
                Ok(())
            });
        }
    }

    pub fn send_sigterm(child: &tokio::process::Child) {
        if let Some(pid) = child.id() {
            unsafe {
                // Signal the whole process group
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
    }
}
