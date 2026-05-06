use crate::config::{ArigConfig, ReadyProbe, ServiceConfig, ServiceType};
use crate::dag;
use futures::future::select_all;
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::signal;

const TAIL_LINES: usize = 50;
const IO_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

type LogTail = Arc<Mutex<VecDeque<String>>>;

struct ManagedChild {
    name: String,
    child: tokio::process::Child,
    tail: LogTail,
    io_tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub async fn up(config: ArigConfig) -> anyhow::Result<()> {
    #[cfg(windows)]
    let _job = win::JobGuard::new()?;

    let waves = dag::toposort(&config)?;
    let mut children: Vec<ManagedChild> = Vec::new();

    for wave in &waves {
        let mut wave_oneshots: Vec<ManagedChild> = Vec::new();
        let mut wave_probes: Vec<(String, ReadyProbe)> = Vec::new();

        for name in wave {
            let service = &config.services[name];
            let mut child = spawn_service(name, service)?;
            let tail: LogTail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)));
            let io_tasks = pipe_output(&mut child, name, &tail);

            let managed = ManagedChild {
                name: name.clone(),
                child,
                tail,
                io_tasks,
            };

            if service.service_type == ServiceType::Oneshot {
                wave_oneshots.push(managed);
            } else {
                if let Some(probe) = &service.ready {
                    wave_probes.push((name.clone(), probe.clone()));
                }
                children.push(managed);
            }
        }

        // Wait for all oneshots in this wave to finish before next wave
        for mut managed in wave_oneshots {
            let status = managed.child.wait().await?;
            if !status.success() {
                eprintln!("arig: oneshot '{}' failed ({status})", managed.name);
                drain_io(&mut managed.io_tasks).await;
                dump_tail(&managed.name, &managed.tail);
                shutdown(&mut children, None).await;
                anyhow::bail!("oneshot '{}' failed", managed.name);
            }
            eprintln!("arig: oneshot '{}' completed", managed.name);
        }

        // Block on readiness probes for long-running services in this wave
        for (name, probe) in wave_probes {
            if let Err(err) = wait_ready(&name, &probe).await {
                eprintln!("arig: {err}");
                if let Some(idx) = children.iter().position(|c| c.name == name) {
                    drain_io(&mut children[idx].io_tasks).await;
                    let n = children[idx].name.clone();
                    dump_tail(&n, &children[idx].tail);
                }
                shutdown(&mut children, None).await;
                anyhow::bail!("readiness probe failed for '{name}'");
            }
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

    let exit = {
        let waits: Vec<_> = children
            .iter_mut()
            .enumerate()
            .map(|(i, c)| {
                Box::pin(async move {
                    let status = c.child.wait().await;
                    (i, status)
                })
            })
            .collect();

        tokio::select! {
            _ = signal::ctrl_c() => None,
            ((idx, status), _, _) = select_all(waits) => Some((idx, status)),
        }
    };

    let skip_idx = exit.as_ref().map(|(idx, _)| *idx);
    let bail = match exit {
        None => {
            eprintln!("\narig: shutting down...");
            false
        }
        Some((idx, Ok(status))) => {
            eprintln!(
                "arig: service '{}' exited (status {status}); long-running services aren't expected to exit, shutting down the rest",
                children[idx].name
            );
            drain_io(&mut children[idx].io_tasks).await;
            let name = children[idx].name.clone();
            dump_tail(&name, &children[idx].tail);
            true
        }
        Some((idx, Err(err))) => {
            eprintln!(
                "arig: service '{}' wait failed ({err}); shutting down the rest",
                children[idx].name
            );
            drain_io(&mut children[idx].io_tasks).await;
            let name = children[idx].name.clone();
            dump_tail(&name, &children[idx].tail);
            true
        }
    };

    shutdown(&mut children, skip_idx).await;

    for managed in children.iter_mut() {
        drain_io(&mut managed.io_tasks).await;
    }

    eprintln!("arig: all services stopped.");
    if bail {
        anyhow::bail!("a service exited unexpectedly");
    }
    Ok(())
}

fn pipe_output(
    managed: &mut tokio::process::Child,
    name: &str,
    tail: &LogTail,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    if let Some(stdout) = managed.stdout.take() {
        let n = name.to_string();
        let t = tail.clone();
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("[{n}] {line}");
                push_tail(&t, line);
            }
        }));
    }
    if let Some(stderr) = managed.stderr.take() {
        let n = name.to_string();
        let t = tail.clone();
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{n}] {line}");
                push_tail(&t, line);
            }
        }));
    }
    tasks
}

fn push_tail(tail: &LogTail, line: String) {
    let mut q = tail.lock().expect("tail mutex poisoned");
    if q.len() >= TAIL_LINES {
        q.pop_front();
    }
    q.push_back(line);
}

async fn drain_io(tasks: &mut Vec<tokio::task::JoinHandle<()>>) {
    for t in tasks.drain(..) {
        let _ = tokio::time::timeout(IO_DRAIN_TIMEOUT, t).await;
    }
}

fn dump_tail(name: &str, tail: &LogTail) {
    let q = tail.lock().expect("tail mutex poisoned");
    if q.is_empty() {
        return;
    }
    eprintln!("arig: --- last {} line(s) from '{}' ---", q.len(), name);
    for line in q.iter() {
        eprintln!("[{name}] {line}");
    }
    eprintln!("arig: --- end '{name}' tail ---");
}

async fn wait_ready(name: &str, probe: &ReadyProbe) -> anyhow::Result<()> {
    let Some(tcp_addr) = probe.tcp.as_deref() else {
        return Ok(());
    };

    eprintln!(
        "arig: waiting for '{name}' tcp probe on {tcp_addr} (timeout {})",
        humantime::format_duration(probe.timeout),
    );

    let deadline = Instant::now() + probe.timeout;
    loop {
        let last_err: String = match tokio::time::timeout(
            PROBE_CONNECT_TIMEOUT,
            TcpStream::connect(tcp_addr),
        )
        .await
        {
            Ok(Ok(_)) => {
                eprintln!("arig: '{name}' is ready");
                return Ok(());
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => "connect timed out".into(),
        };

        if Instant::now() >= deadline {
            anyhow::bail!(
                "'{name}' tcp probe '{tcp_addr}' did not become ready within {}: last error: {last_err}",
                humantime::format_duration(probe.timeout),
            );
        }

        tokio::time::sleep(PROBE_INTERVAL).await;
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

async fn shutdown(children: &mut [ManagedChild], skip_idx: Option<usize>) {
    #[cfg(windows)]
    win::send_ctrl_c();

    #[cfg(unix)]
    for (i, managed) in children.iter().enumerate() {
        if Some(i) == skip_idx {
            continue;
        }
        unix::send_sigterm(&managed.child);
    }

    // Wait up to 5 seconds per process for graceful exit, then force kill
    for (i, managed) in children.iter_mut().enumerate() {
        if Some(i) == skip_idx {
            continue;
        }

        let graceful = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            managed.child.wait(),
        )
        .await;

        match graceful {
            Ok(Ok(status)) => {
                eprintln!("arig: {} stopped ({status})", managed.name);
            }
            _ => {
                eprintln!("arig: {} did not stop in time, force killing", managed.name);
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
