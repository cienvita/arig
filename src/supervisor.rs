use crate::config::{ArigConfig, ReadyProbe, ServiceConfig, ServiceType};
use crate::dag;
use crate::ipc;
use crate::protocol;
use anyhow::Context;
use chrono::Local;
use futures::future::select_all;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::signal;

type ServiceState = Arc<Mutex<Vec<protocol::ServiceSnapshot>>>;

const TAIL_LINES: usize = 50;
const IO_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

type LogTail = Arc<Mutex<VecDeque<String>>>;
type LogSink = Arc<Mutex<File>>;
type LastOutput = Arc<Mutex<Instant>>;

// Session-wide log file for arig's own messages. Set once in `up` after the
// session dir exists, then read by `event!` on every `arig:` line.
static EVENT_LOG: OnceLock<LogSink> = OnceLock::new();

macro_rules! event {
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        eprintln!("{s}");
        if let Some(f) = $crate::supervisor::EVENT_LOG.get()
            && let Ok(mut g) = f.lock()
        {
            let _ = writeln!(*g, "{s}");
        }
    }};
}

struct ResolvedShutdown {
    command: String,
    timeout: std::time::Duration,
    working_dir: Option<String>,
    env: std::collections::HashMap<String, String>,
}

struct ManagedChild {
    name: String,
    wave: usize,
    child: tokio::process::Child,
    tail: LogTail,
    last_output: LastOutput,
    io_tasks: Vec<tokio::task::JoinHandle<()>>,
    shutdown_hook: Option<ResolvedShutdown>,
}

struct IpcCleanup(ipc::Endpoint);

impl Drop for IpcCleanup {
    fn drop(&mut self) {
        ipc::cleanup(&self.0);
    }
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

    let session_dir = create_session_dir(&config.dirs.logs)?;
    let event_log = open_log_file(&session_dir, "_arig")?;
    let _ = EVENT_LOG.set(event_log);
    event!("arig: logs at {}", session_dir.display());

    let workspace = std::env::current_dir().context("read current dir")?;
    let endpoint = ipc::Endpoint::for_workspace(&workspace)?;
    let listener = ipc::bind(&endpoint)?;
    let _ipc_cleanup = IpcCleanup(endpoint.clone());
    let state: ServiceState = Arc::new(Mutex::new(Vec::new()));
    let acceptor = ipc::Acceptor::new(listener, endpoint.address.clone());
    let _ipc_task = tokio::spawn(ipc_accept_loop(
        acceptor,
        state.clone(),
        shutdown_tx.clone(),
    ));
    event!("arig: ipc bound at {}", endpoint.address);

    let waves = dag::toposort(&config)?;
    let mut children: Vec<ManagedChild> = Vec::new();

    for (wave_idx, wave) in waves.iter().enumerate() {
        let mut wave_oneshots: Vec<ManagedChild> = Vec::new();
        let mut wave_probes: Vec<(String, ReadyProbe)> = Vec::new();

        for name in wave {
            let service = &config.services[name];
            let mut child = spawn_service(name, service)?;
            let pid = child.id().unwrap_or(0);
            let tail: LogTail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)));
            let log_file = open_log_file(&session_dir, name)?;
            let last_output: LastOutput = Arc::new(Mutex::new(Instant::now()));
            let io_tasks = pipe_output(&mut child, name, &tail, &log_file, &last_output);

            let shutdown_hook = service.shutdown.as_ref().map(|sd| ResolvedShutdown {
                command: sd.command.clone(),
                timeout: sd.timeout,
                working_dir: service.working_dir.clone(),
                env: service.env.clone(),
            });
            let managed = ManagedChild {
                name: name.clone(),
                wave: wave_idx,
                child,
                tail,
                last_output,
                io_tasks,
                shutdown_hook,
            };

            state.lock().unwrap().push(protocol::ServiceSnapshot {
                name: name.clone(),
                kind: match service.service_type {
                    ServiceType::Service => "service".into(),
                    ServiceType::Oneshot => "oneshot".into(),
                },
                wave: wave_idx,
                pid,
                status: "running".into(),
            });

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
                    event!("\narig: shutting down...");
                    shutdown(&mut children, None).await;
                    event!("arig: all services stopped.");
                    return Ok(());
                }
            };

            match outcome {
                Ok(status) if status.success() => {
                    event!("arig: oneshot '{}' completed", managed.name);
                    state.lock().unwrap().retain(|s| s.name != managed.name);
                }
                Ok(status) => {
                    event!("arig: oneshot '{}' failed ({status})", managed.name);
                    drain_io(&mut managed.io_tasks).await;
                    dump_tail(&managed.name, &managed.tail);
                    shutdown(&mut children, None).await;
                    anyhow::bail!("oneshot '{}' failed", managed.name);
                }
                Err(err) => {
                    event!("arig: {err}");
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
                    event!("\narig: shutting down...");
                    shutdown(&mut children, None).await;
                    event!("arig: all services stopped.");
                    return Ok(());
                }
            };

            if let Err(err) = result {
                event!("arig: {err}");
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
        event!("arig: all tasks completed.");
        return Ok(());
    }

    event!(
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
            event!("\narig: shutting down...");
            false
        }
        Some((idx, Ok(status))) => {
            event!(
                "arig: service '{}' exited (status {status}); long-running services aren't expected to exit, shutting down the rest",
                children[idx].name
            );
            drain_io(&mut children[idx].io_tasks).await;
            let name = children[idx].name.clone();
            dump_tail(&name, &children[idx].tail);
            true
        }
        Some((idx, Err(err))) => {
            event!(
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

    event!("arig: all services stopped.");
    if bail {
        anyhow::bail!("a service exited unexpectedly");
    }
    Ok(())
}

/// Spawn the current binary as a detached `__supervise` process, wait until it
/// binds its IPC endpoint, then return. The caller process exits normally.
pub async fn detach_and_exit(config_file: &Path) -> anyhow::Result<()> {
    // Pass cwd as-is to the child; Endpoint::for_workspace canonicalizes
    // internally for the hash. Canonicalizing here would yield `\\?\` paths on
    // Windows, which CMD.EXE refuses as a working directory for service
    // commands.
    let workspace = std::env::current_dir().context("read current dir")?;
    let endpoint = ipc::Endpoint::for_workspace(&workspace)?;

    if ipc::probe(&endpoint).await {
        anyhow::bail!(
            "a supervisor is already running for this workspace at {}",
            endpoint.address
        );
    }

    let exe = std::env::current_exe().context("locate current exe")?;
    let var_dir = workspace.join(".arig/var");
    std::fs::create_dir_all(&var_dir).with_context(|| format!("create {}", var_dir.display()))?;
    let log_path = var_dir.join("supervisor.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--file").arg(config_file);
    cmd.arg("__supervise").arg("--workspace").arg(&workspace);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log.try_clone()?));
    cmd.stderr(Stdio::from(log));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // New session detaches us from the spawning shell's controlling
                // tty, so closing the terminal doesn't SIGHUP the supervisor.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = spawn_detached(&mut cmd)?;
    let pid = child.id();
    eprintln!("arig: spawned supervisor (pid {pid})");

    match ipc::wait_ready(&endpoint, Duration::from_secs(10)).await {
        Ok(()) => {
            eprintln!("arig: supervisor ready at {}", endpoint.address);
            eprintln!("arig: log at {}", log_path.display());
            Ok(())
        }
        Err(e) => {
            anyhow::bail!("{e}. check {} for details", log_path.display());
        }
    }
}

async fn ipc_accept_loop(
    mut acceptor: ipc::Acceptor,
    state: ServiceState,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    loop {
        let stream = match acceptor.accept().await {
            Ok(s) => s,
            Err(_) => break,
        };
        let st = state.clone();
        let sd = shutdown_tx.clone();
        tokio::spawn(async move {
            handle_client(stream, st, sd).await;
        });
    }
}

async fn handle_client(
    stream: ipc::ServerStream,
    state: ServiceState,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    let (rd, mut wr) = tokio::io::split(stream);
    let req = match protocol::read_request(rd).await {
        Ok(r) => r,
        Err(e) => {
            let _ =
                protocol::write_response(&mut wr, &protocol::Response::err(e.to_string())).await;
            return;
        }
    };
    match req {
        protocol::Request::Ps => {
            let snap = state.lock().unwrap().clone();
            let _ = protocol::write_response(&mut wr, &protocol::Response::ps(snap)).await;
        }
        protocol::Request::Down => {
            // Flush response before triggering shutdown so the client always
            // sees the ack even if the supervisor exits quickly.
            let _ = protocol::write_response(&mut wr, &protocol::Response::ok()).await;
            let _ = shutdown_tx.send(true);
        }
    }
}

#[cfg(unix)]
fn spawn_detached(cmd: &mut std::process::Command) -> anyhow::Result<std::process::Child> {
    cmd.spawn().context("spawn detached supervisor")
}

#[cfg(windows)]
fn spawn_detached(cmd: &mut std::process::Command) -> anyhow::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS: no console attachment.
    // CREATE_NEW_PROCESS_GROUP: own group so it doesn't share our Ctrl+C.
    // CREATE_BREAKAWAY_FROM_JOB: escape any job we (or our parent) sit in that
    // has KILL_ON_JOB_CLOSE, so the supervisor outlives this CLI.
    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Ok(c) => Ok(c),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            // The outer job (a shell wrapper, a terminal multiplexer, or this
            // session's harness) does not allow CREATE_BREAKAWAY_FROM_JOB.
            // Retry without it: the supervisor will be assigned to that job
            // and may die when it closes if KILL_ON_JOB_CLOSE is set.
            eprintln!(
                "arig: outer job denied breakaway; supervisor will inherit it (closing this shell may kill it)"
            );
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn()
                .context("spawn detached supervisor (no breakaway)")
        }
        Err(e) => Err(anyhow::Error::new(e).context("spawn detached supervisor")),
    }
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

fn create_session_dir(base: &Path) -> anyhow::Result<PathBuf> {
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
    event!("arig: --- last {} line(s) from '{}' ---", q.len(), name);
    for line in q.iter() {
        event!("[{name}] {line}");
    }
    event!("arig: --- end '{name}' tail ---");
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
            event!(
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

    event!(
        "arig: waiting for '{name}' tcp probe on {tcp_addr} (timeout {})",
        humantime::format_duration(probe.timeout),
    );

    let deadline = Instant::now() + probe.timeout;
    let mut last_heartbeat = Instant::now();
    loop {
        let last_err: String =
            match tokio::time::timeout(PROBE_CONNECT_TIMEOUT, TcpStream::connect(tcp_addr)).await {
                Ok(Ok(_)) => {
                    event!("arig: '{name}' is ready");
                    return Ok(());
                }
                Ok(Err(e)) => e.to_string(),
                Err(_) => "connect timed out".into(),
            };

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            event!("arig: still waiting for '{name}' tcp {tcp_addr} (last error: {last_err})");
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

    #[cfg(windows)]
    win::configure_child(&mut cmd);

    #[cfg(unix)]
    unix::configure_child(&mut cmd);

    let child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    event!("arig: started {name} (PID {pid})");
    Ok(child)
}

async fn shutdown(children: &mut [ManagedChild], skip_idx: Option<usize>) {
    // Walk waves in reverse: dependents first, then their dependencies. Within
    // each wave, signal everyone without a shutdown hook first (so they start
    // their graceful exit while we wait on hook-based services), then wait for
    // the whole wave to settle before moving on.
    let max_wave = children.iter().map(|c| c.wave).max().unwrap_or(0);

    for wave_idx in (0..=max_wave).rev() {
        let wave_indices: Vec<usize> = (0..children.len())
            .filter(|i| children[*i].wave == wave_idx && Some(*i) != skip_idx)
            .collect();

        if wave_indices.is_empty() {
            continue;
        }

        // Pre-signal services that don't have a shutdown hook so they can
        // begin exiting while we sequentially handle hook-based ones.
        for &i in &wave_indices {
            if children[i].shutdown_hook.is_none() {
                send_shutdown_signal(&children[i].child);
            }
        }

        for &i in &wave_indices {
            let managed = &mut children[i];
            let name = managed.name.clone();

            // Clone hook data eagerly to avoid holding a shared borrow of
            // `managed` while we also need &mut access to `managed.child`.
            let hook = managed.shutdown_hook.as_ref().map(|h| {
                (
                    h.command.clone(),
                    h.timeout,
                    h.working_dir.clone(),
                    h.env.clone(),
                )
            });

            if let Some((command, timeout, working_dir, env)) = hook {
                let mut sd_cmd = Command::new(shell_program());
                sd_cmd
                    .args(shell_args(&command))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(ref dir) = working_dir {
                    sd_cmd.current_dir(dir);
                }
                sd_cmd.envs(&env);
                #[cfg(windows)]
                win::configure_child(&mut sd_cmd);
                #[cfg(unix)]
                unix::configure_child(&mut sd_cmd);

                event!("arig: running shutdown hook for '{name}'");
                match sd_cmd.spawn() {
                    Ok(mut sd_child) => {
                        let r = tokio::time::timeout(timeout, managed.child.wait()).await;
                        // Kill the hook's whole process group, then reap it so
                        // it doesn't become a zombie and its subprocesses don't
                        // become orphans. On Windows the job object owns the
                        // tree, so the kill below covers it.
                        #[cfg(unix)]
                        if let Some(pid) = sd_child.id() {
                            unix::send_sigkill(pid);
                        }
                        let _ = sd_child.kill().await;
                        let _ = sd_child.wait().await;
                        match r {
                            Ok(Ok(status)) => event!("arig: {name} stopped ({status})"),
                            _ => {
                                // Hook ran but the main child did not exit within
                                // the configured timeout. Give it a SIGTERM + 5 s
                                // grace period before force-killing.
                                event!("arig: {name} did not stop after shutdown hook, signalling");
                                send_shutdown_signal(&managed.child);
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    managed.child.wait(),
                                )
                                .await
                                {
                                    Ok(Ok(status)) => event!("arig: {name} stopped ({status})"),
                                    _ => {
                                        event!("arig: {name} did not stop in time, force killing");
                                        let _ = managed.child.kill().await;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        event!(
                            "arig: shutdown hook for '{name}' failed to spawn ({e}), signalling"
                        );
                        send_shutdown_signal(&managed.child);
                        // Use the configured hook timeout, not the hardcoded
                        // default, since the operator set it expecting the service
                        // to need that long to stop.
                        match tokio::time::timeout(timeout, managed.child.wait()).await {
                            Ok(Ok(status)) => event!("arig: {name} stopped ({status})"),
                            _ => {
                                event!("arig: {name} did not stop in time, force killing");
                                let _ = managed.child.kill().await;
                            }
                        }
                    }
                }
            } else {
                match tokio::time::timeout(std::time::Duration::from_secs(5), managed.child.wait())
                    .await
                {
                    Ok(Ok(status)) => event!("arig: {name} stopped ({status})"),
                    _ => {
                        event!("arig: {name} did not stop in time, force killing");
                        let _ = managed.child.kill().await;
                    }
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
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetConsoleWindow,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, GetCurrentProcess};

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    fn has_console() -> bool {
        unsafe { !GetConsoleWindow().is_null() }
    }

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
        let mut child = spawn_service("reader", &svc(stdin_reader(), ServiceType::Oneshot))
            .expect("spawn oneshot");

        // The regression is a hang, so the assertion is that it terminates at
        // all. Exit status is up to the command's own EOF handling.
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("oneshot stdin should be at EOF, not waiting on the terminal")
            .expect("wait on oneshot");
    }
}
