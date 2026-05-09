use crate::config::{ArigConfig, ReadyProbe, ServiceConfig, ServiceType};
use crate::dag;
use chrono::Local;
use futures::future::select_all;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
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
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

type LogTail = Arc<Mutex<VecDeque<String>>>;
type LogSink = Arc<Mutex<File>>;
type LastOutput = Arc<Mutex<Instant>>;

struct ManagedChild {
    name: String,
    wave: usize,
    child: tokio::process::Child,
    tail: LogTail,
    last_output: LastOutput,
    io_tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub async fn up(config: ArigConfig) -> anyhow::Result<()> {
    #[cfg(windows)]
    let _job = win::JobGuard::new()?;

    // Install the ctrl-c handler eagerly. tokio::signal::ctrl_c registers its
    // OS handler on first poll, so deferring it until the post-wave select
    // means a ctrl-c during spawn / probe / oneshot waits hits the Windows
    // default handler and kills us with STATUS_CONTROL_C_EXIT before we can
    // run shutdown(). Broadcasting via watch lets every blocking await race
    // against shutdown without re-arming a fresh signal future each time
    // (which would lose any signal that fired between selects).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let _ = signal::ctrl_c().await;
            let _ = tx.send(true);
        });
    }

    let session_dir = create_session_dir()?;
    eprintln!("arig: logs at {}", session_dir.display());

    let waves = dag::toposort(&config)?;
    let mut children: Vec<ManagedChild> = Vec::new();

    for (wave_idx, wave) in waves.iter().enumerate() {
        let mut wave_oneshots: Vec<ManagedChild> = Vec::new();
        let mut wave_probes: Vec<(String, ReadyProbe)> = Vec::new();

        for name in wave {
            let service = &config.services[name];
            let mut child = spawn_service(name, service)?;
            let tail: LogTail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)));
            let log_file = open_log_file(&session_dir, name)?;
            let last_output: LastOutput = Arc::new(Mutex::new(Instant::now()));
            let io_tasks = pipe_output(&mut child, name, &tail, &log_file, &last_output);

            let managed = ManagedChild {
                name: name.clone(),
                wave: wave_idx,
                child,
                tail,
                last_output,
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
            let mut rx = shutdown_rx.clone();
            let timeout = config.services[&managed.name].timeout;
            let wait = wait_oneshot(
                &managed.name,
                &mut managed.child,
                timeout,
                managed.last_output.clone(),
            );
            let outcome = tokio::select! {
                r = wait => r,
                _ = rx.changed() => {
                    eprintln!("\narig: shutting down...");
                    shutdown(&mut children, None).await;
                    eprintln!("arig: all services stopped.");
                    return Ok(());
                }
            };

            match outcome {
                Ok(status) if status.success() => {
                    eprintln!("arig: oneshot '{}' completed", managed.name);
                }
                Ok(status) => {
                    eprintln!("arig: oneshot '{}' failed ({status})", managed.name);
                    drain_io(&mut managed.io_tasks).await;
                    dump_tail(&managed.name, &managed.tail);
                    shutdown(&mut children, None).await;
                    anyhow::bail!("oneshot '{}' failed", managed.name);
                }
                Err(err) => {
                    eprintln!("arig: {err}");
                    drain_io(&mut managed.io_tasks).await;
                    dump_tail(&managed.name, &managed.tail);
                    shutdown(&mut children, None).await;
                    anyhow::bail!("oneshot '{}' failed", managed.name);
                }
            }
        }

        // Block on readiness probes for long-running services in this wave
        for (name, probe) in wave_probes {
            let mut rx = shutdown_rx.clone();
            let result = tokio::select! {
                r = wait_ready(&name, &probe) => r,
                _ = rx.changed() => {
                    eprintln!("\narig: shutting down...");
                    shutdown(&mut children, None).await;
                    eprintln!("arig: all services stopped.");
                    return Ok(());
                }
            };

            if let Err(err) = result {
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

    let mut rx = shutdown_rx.clone();
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
            _ = rx.changed() => None,
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
    log_file: &LogSink,
    last_output: &LastOutput,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    if let Some(stdout) = managed.stdout.take() {
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
    if let Some(stderr) = managed.stderr.take() {
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

fn mark_output(last_output: &LastOutput) {
    if let Ok(mut t) = last_output.lock() {
        *t = Instant::now();
    }
}

fn write_log_line(file: &LogSink, line: &str) {
    if let Ok(mut f) = file.lock() {
        let _ = writeln!(*f, "{line}");
    }
}

fn create_session_dir() -> anyhow::Result<PathBuf> {
    let stamp = Local::now().format("%Y%m%d%H%M%S%3f").to_string();
    let dir = PathBuf::from(".logs").join(&stamp);
    std::fs::create_dir_all(&dir)?;

    // Best-effort `.logs/latest` pointer to the freshest run. On Windows this
    // needs Developer Mode (or admin) to create a symlink; on failure we just
    // skip silently — the timestamped dir is the source of truth.
    let latest = PathBuf::from(".logs").join("latest");
    let _ = std::fs::remove_file(&latest);
    let _ = std::fs::remove_dir(&latest);
    let _ = create_dir_link(&dir, &latest);

    Ok(dir)
}

#[cfg(windows)]
fn create_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    // Use a relative target so the link survives if .logs/ is moved.
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

fn open_log_file(session_dir: &Path, name: &str) -> anyhow::Result<LogSink> {
    let path = session_dir.join(format!("{name}.log"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(Arc::new(Mutex::new(file)))
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

async fn wait_oneshot(
    name: &str,
    child: &mut tokio::process::Child,
    timeout: Option<Duration>,
    last_output: LastOutput,
) -> anyhow::Result<std::process::ExitStatus> {
    let heartbeat = tokio::spawn(oneshot_heartbeat(name.to_string(), last_output));
    let result = wait_oneshot_inner(name, child, timeout).await;
    heartbeat.abort();
    result
}

async fn wait_oneshot_inner(
    name: &str,
    child: &mut tokio::process::Child,
    timeout: Option<Duration>,
) -> anyhow::Result<std::process::ExitStatus> {
    let Some(limit) = timeout else {
        return Ok(child.wait().await?);
    };

    match tokio::time::timeout(limit, child.wait()).await {
        Ok(status) => Ok(status?),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!(
                "oneshot '{name}' timed out after {}",
                humantime::format_duration(limit)
            );
        }
    }
}

async fn oneshot_heartbeat(name: String, last_output: LastOutput) {
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    interval.tick().await;
    loop {
        interval.tick().await;
        let last = match last_output.lock() {
            Ok(g) => *g,
            Err(_) => return,
        };
        let silent = last.elapsed();
        if silent >= HEARTBEAT_INTERVAL {
            eprintln!(
                "arig: '{name}' still running (no output for {})",
                humantime::format_duration(Duration::from_secs(silent.as_secs())),
            );
        }
    }
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
    let mut last_heartbeat = Instant::now();
    loop {
        let last_err: String =
            match tokio::time::timeout(PROBE_CONNECT_TIMEOUT, TcpStream::connect(tcp_addr)).await {
                Ok(Ok(_)) => {
                    eprintln!("arig: '{name}' is ready");
                    return Ok(());
                }
                Ok(Err(e)) => e.to_string(),
                Err(_) => "connect timed out".into(),
            };

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            eprintln!("arig: still waiting for '{name}' tcp {tcp_addr} (last error: {last_err})");
            last_heartbeat = Instant::now();
        }

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
    // Walk waves in reverse: dependents first, then their dependencies. Within
    // each wave, signal everyone, then wait for the whole wave to settle before
    // moving on. This stops api logging "nats disconnected" while we're still
    // taking nats down.
    let max_wave = children.iter().map(|c| c.wave).max().unwrap_or(0);

    for wave_idx in (0..=max_wave).rev() {
        let wave_indices: Vec<usize> = (0..children.len())
            .filter(|i| children[*i].wave == wave_idx && Some(*i) != skip_idx)
            .collect();

        if wave_indices.is_empty() {
            continue;
        }

        for &i in &wave_indices {
            send_shutdown_signal(&children[i].child);
        }

        for &i in &wave_indices {
            let managed = &mut children[i];
            let graceful =
                tokio::time::timeout(std::time::Duration::from_secs(5), managed.child.wait()).await;

            match graceful {
                Ok(Ok(status)) => {
                    eprintln!("arig: {} stopped ({status})", managed.name);
                }
                _ => {
                    eprintln!("arig: {} did not stop in time, force killing", managed.name);
                    let _ = managed.child.kill().await;
                }
            }

            drain_io(&mut managed.io_tasks).await;
        }
    }
}

fn send_shutdown_signal(child: &tokio::process::Child) {
    let Some(pid) = child.id() else {
        return;
    };

    #[cfg(windows)]
    win::send_ctrl_break(pid);

    #[cfg(unix)]
    unix::send_sigterm(pid);
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
// Windows: job objects + GenerateConsoleCtrlEvent
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, GetCurrentProcess};

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

    pub fn configure_child(cmd: &mut tokio::process::Command) {
        // Children inherit the parent's job object (kill-on-close safety net).
        // CREATE_NEW_PROCESS_GROUP makes each child the leader of its own
        // group, so we can target it individually with GenerateConsoleCtrlEvent.
        // It also detaches the child from the parent's Ctrl+C — we drive
        // shutdown explicitly via send_ctrl_break.
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
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
}
