mod logs;
mod process;

use crate::config::{ArigConfig, ReadyProbe, ServiceType};
use crate::dag;
use crate::event::{Bus, Event, ServiceKind, event};
use crate::ipc;
use crate::protocol;
use crate::state::{self, StateTracker};
use anyhow::Context;
use futures::future::select_all;
use logs::{LastOutput, LogTail};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::signal;

const IO_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

struct ResolvedShutdown {
    command: String,
    timeout: Duration,
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

/// Owns everything the supervisor needs to drive services: the config, the DAG
/// it came from, and the bus every observer hangs off.
struct Kernel {
    config: ArigConfig,
    bus: Bus,
    session_dir: PathBuf,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
}

pub async fn up(config: ArigConfig) -> anyhow::Result<()> {
    #[cfg(windows)]
    let _job = process::win::JobGuard::new()?;

    let bus = Bus::new(crate::event::CAPACITY);

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
        let bus = bus.clone();
        tokio::spawn(async move {
            let _ = signal::ctrl_c().await;
            bus.emit(Event::ShutdownRequested);
            let _ = tx.send(true);
        });
    }

    let session_dir = logs::create_session_dir(&config.dirs.logs)?;
    // Subscribe the sinks before the first line is emitted; anything emitted
    // earlier is gone by the time they attach.
    let session_log = logs::SessionLog::spawn(&bus, logs::open_log_file(&session_dir, "_arig")?);
    let state = state::spawn(&bus);
    event!(bus, "arig: logs at {}", session_dir.display());

    let workspace = std::env::current_dir().context("read current dir")?;
    let endpoint = ipc::Endpoint::for_workspace(&workspace)?;
    let listener = ipc::bind(&endpoint)?;
    let _ipc_cleanup = IpcCleanup(endpoint.clone());
    let acceptor = ipc::Acceptor::new(listener, endpoint.address.clone());
    let _ipc_task = tokio::spawn(ipc_accept_loop(
        acceptor,
        state,
        shutdown_tx.clone(),
        bus.clone(),
    ));
    event!(bus, "arig: ipc bound at {}", endpoint.address);

    let kernel = Kernel {
        config,
        bus: bus.clone(),
        session_dir,
        shutdown_rx,
    };
    let result = kernel.run().await;

    // The session log is a task of its own, so give it a moment to write the
    // lines emitted just before we return.
    bus.drain(session_log.cursor(), IO_DRAIN_TIMEOUT).await;
    result
}

impl Kernel {
    async fn run(&self) -> anyhow::Result<()> {
        let waves = dag::toposort(&self.config)?;
        let mut children: Vec<ManagedChild> = Vec::new();

        for (wave_idx, wave) in waves.iter().enumerate() {
            let mut wave_oneshots: Vec<ManagedChild> = Vec::new();
            let mut wave_probes: Vec<(String, ReadyProbe)> = Vec::new();

            for name in wave {
                let service = &self.config.services[name];
                let mut child = process::spawn_service(service)?;
                let pid = child.id().unwrap_or(0);
                event!(self.bus, "arig: started {name} (PID {pid})");

                let tail = logs::new_tail();
                let log_file = logs::open_log_file(&self.session_dir, name)?;
                let last_output: LastOutput = Arc::new(Mutex::new(Instant::now()));
                let io_tasks =
                    process::pipe_output(&mut child, name, &tail, &log_file, &last_output);

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

                self.bus.emit(Event::ServiceStarted {
                    name: name.clone(),
                    wave: wave_idx,
                    kind: ServiceKind::from(&service.service_type),
                    pid,
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
                let mut rx = self.shutdown_rx.clone();
                let timeout = self.config.services[&managed.name].timeout;
                let wait = wait_oneshot(
                    &self.bus,
                    &managed.name,
                    &mut managed.child,
                    timeout,
                    managed.last_output.clone(),
                );
                let outcome = tokio::select! {
                    r = wait => r,
                    _ = rx.changed() => {
                        event!(self.bus, "\narig: shutting down...");
                        shutdown(&self.bus, &mut children, None).await;
                        event!(self.bus, "arig: all services stopped.");
                        return Ok(());
                    }
                };

                match outcome {
                    Ok(status) if status.success() => {
                        event!(self.bus, "arig: oneshot '{}' completed", managed.name);
                        self.bus.emit(Event::OneshotCompleted {
                            name: managed.name.clone(),
                            success: true,
                        });
                    }
                    Ok(status) => {
                        event!(
                            self.bus,
                            "arig: oneshot '{}' failed ({status})",
                            managed.name
                        );
                        self.bus.emit(Event::OneshotCompleted {
                            name: managed.name.clone(),
                            success: false,
                        });
                        drain_io(&mut managed.io_tasks).await;
                        dump_tail(&self.bus, &managed.name, &managed.tail);
                        shutdown(&self.bus, &mut children, None).await;
                        anyhow::bail!("oneshot '{}' failed", managed.name);
                    }
                    Err(err) => {
                        event!(self.bus, "arig: {err}");
                        self.bus.emit(Event::OneshotCompleted {
                            name: managed.name.clone(),
                            success: false,
                        });
                        drain_io(&mut managed.io_tasks).await;
                        dump_tail(&self.bus, &managed.name, &managed.tail);
                        shutdown(&self.bus, &mut children, None).await;
                        anyhow::bail!("oneshot '{}' failed", managed.name);
                    }
                }
            }

            // Block on readiness probes for long-running services in this wave
            for (name, probe) in wave_probes {
                let mut rx = self.shutdown_rx.clone();
                let result = tokio::select! {
                    r = wait_ready(&self.bus, &name, &probe) => r,
                    _ = rx.changed() => {
                        event!(self.bus, "\narig: shutting down...");
                        shutdown(&self.bus, &mut children, None).await;
                        event!(self.bus, "arig: all services stopped.");
                        return Ok(());
                    }
                };

                if let Err(err) = result {
                    event!(self.bus, "arig: {err}");
                    if let Some(idx) = children.iter().position(|c| c.name == name) {
                        drain_io(&mut children[idx].io_tasks).await;
                        let n = children[idx].name.clone();
                        dump_tail(&self.bus, &n, &children[idx].tail);
                    }
                    shutdown(&self.bus, &mut children, None).await;
                    anyhow::bail!("readiness probe failed for '{name}'");
                }
            }
        }

        if children.is_empty() {
            event!(self.bus, "arig: all tasks completed.");
            return Ok(());
        }

        event!(
            self.bus,
            "arig: {} service(s) running. Press Ctrl+C to stop.",
            children.len()
        );

        let mut rx = self.shutdown_rx.clone();
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
                event!(self.bus, "\narig: shutting down...");
                false
            }
            Some((idx, Ok(status))) => {
                event!(
                    self.bus,
                    "arig: service '{}' exited (status {status}); long-running services aren't expected to exit, shutting down the rest",
                    children[idx].name
                );
                self.bus.emit(Event::ServiceExited {
                    name: children[idx].name.clone(),
                    status: status.to_string(),
                });
                drain_io(&mut children[idx].io_tasks).await;
                let name = children[idx].name.clone();
                dump_tail(&self.bus, &name, &children[idx].tail);
                true
            }
            Some((idx, Err(err))) => {
                event!(
                    self.bus,
                    "arig: service '{}' wait failed ({err}); shutting down the rest",
                    children[idx].name
                );
                drain_io(&mut children[idx].io_tasks).await;
                let name = children[idx].name.clone();
                dump_tail(&self.bus, &name, &children[idx].tail);
                true
            }
        };

        shutdown(&self.bus, &mut children, skip_idx).await;

        event!(self.bus, "arig: all services stopped.");
        if bail {
            anyhow::bail!("a service exited unexpectedly");
        }
        Ok(())
    }
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

    let child = process::spawn_detached(&mut cmd)?;
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
    state: StateTracker,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    bus: Bus,
) {
    loop {
        let stream = match acceptor.accept().await {
            Ok(s) => s,
            Err(_) => break,
        };
        let st = state.clone();
        let sd = shutdown_tx.clone();
        let bus = bus.clone();
        tokio::spawn(async move {
            handle_client(stream, st, sd, bus).await;
        });
    }
}

async fn handle_client(
    stream: ipc::ServerStream,
    state: StateTracker,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    bus: Bus,
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
            let snap = state.snapshot();
            let _ = protocol::write_response(&mut wr, &protocol::Response::ps(snap)).await;
        }
        protocol::Request::Down => {
            // Flush response before triggering shutdown so the client always
            // sees the ack even if the supervisor exits quickly.
            let _ = protocol::write_response(&mut wr, &protocol::Response::ok()).await;
            bus.emit(Event::ShutdownRequested);
            let _ = shutdown_tx.send(true);
        }
    }
}

async fn drain_io(tasks: &mut Vec<tokio::task::JoinHandle<()>>) {
    for t in tasks.drain(..) {
        let _ = tokio::time::timeout(IO_DRAIN_TIMEOUT, t).await;
    }
}

fn dump_tail(bus: &Bus, name: &str, tail: &LogTail) {
    let q = tail.lock().expect("tail mutex poisoned");
    if q.is_empty() {
        return;
    }
    event!(
        bus,
        "arig: --- last {} line(s) from '{}' ---",
        q.len(),
        name
    );
    for line in q.iter() {
        event!(bus, "[{name}] {line}");
    }
    event!(bus, "arig: --- end '{name}' tail ---");
}

async fn wait_oneshot(
    bus: &Bus,
    name: &str,
    child: &mut tokio::process::Child,
    timeout: Option<Duration>,
    last_output: LastOutput,
) -> anyhow::Result<std::process::ExitStatus> {
    let heartbeat = tokio::spawn(oneshot_heartbeat(
        bus.clone(),
        name.to_string(),
        last_output,
    ));
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

async fn oneshot_heartbeat(bus: Bus, name: String, last_output: LastOutput) {
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
                bus,
                "arig: '{name}' still running (no output for {})",
                humantime::format_duration(Duration::from_secs(silent.as_secs())),
            );
        }
    }
}

async fn wait_ready(bus: &Bus, name: &str, probe: &ReadyProbe) -> anyhow::Result<()> {
    let Some(tcp_addr) = probe.tcp.as_deref() else {
        return Ok(());
    };

    event!(
        bus,
        "arig: waiting for '{name}' tcp probe on {tcp_addr} (timeout {})",
        humantime::format_duration(probe.timeout),
    );

    let deadline = Instant::now() + probe.timeout;
    let mut last_heartbeat = Instant::now();
    loop {
        let last_err: String =
            match tokio::time::timeout(PROBE_CONNECT_TIMEOUT, TcpStream::connect(tcp_addr)).await {
                Ok(Ok(_)) => {
                    event!(bus, "arig: '{name}' is ready");
                    return Ok(());
                }
                Ok(Err(e)) => e.to_string(),
                Err(_) => "connect timed out".into(),
            };

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            event!(
                bus,
                "arig: still waiting for '{name}' tcp {tcp_addr} (last error: {last_err})"
            );
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

async fn shutdown(bus: &Bus, children: &mut [ManagedChild], skip_idx: Option<usize>) {
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
                process::send_shutdown_signal(&children[i].child);
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
                let mut sd_cmd = Command::new(process::shell_program());
                sd_cmd
                    .args(process::shell_args(&command))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(ref dir) = working_dir {
                    sd_cmd.current_dir(dir);
                }
                sd_cmd.envs(&env);
                process::configure_child(&mut sd_cmd);

                event!(bus, "arig: running shutdown hook for '{name}'");
                match sd_cmd.spawn() {
                    Ok(mut sd_child) => {
                        let r = tokio::time::timeout(timeout, managed.child.wait()).await;
                        // Kill the hook's whole process group, then reap it so
                        // it doesn't become a zombie and its subprocesses don't
                        // become orphans. On Windows the job object owns the
                        // tree, so the kill below covers it.
                        if let Some(pid) = sd_child.id() {
                            process::kill_process_group(pid);
                        }
                        let _ = sd_child.kill().await;
                        let _ = sd_child.wait().await;
                        match r {
                            Ok(Ok(status)) => stopped(bus, &name, &status.to_string()),
                            _ => {
                                // Hook ran but the main child did not exit within
                                // the configured timeout. Give it a SIGTERM + 5 s
                                // grace period before force-killing.
                                event!(
                                    bus,
                                    "arig: {name} did not stop after shutdown hook, signalling"
                                );
                                process::send_shutdown_signal(&managed.child);
                                match tokio::time::timeout(
                                    Duration::from_secs(5),
                                    managed.child.wait(),
                                )
                                .await
                                {
                                    Ok(Ok(status)) => stopped(bus, &name, &status.to_string()),
                                    _ => {
                                        force_kill(bus, &name, &mut managed.child).await;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        event!(
                            bus,
                            "arig: shutdown hook for '{name}' failed to spawn ({e}), signalling"
                        );
                        process::send_shutdown_signal(&managed.child);
                        // Use the configured hook timeout, not the hardcoded
                        // default, since the operator set it expecting the service
                        // to need that long to stop.
                        match tokio::time::timeout(timeout, managed.child.wait()).await {
                            Ok(Ok(status)) => stopped(bus, &name, &status.to_string()),
                            _ => force_kill(bus, &name, &mut managed.child).await,
                        }
                    }
                }
            } else {
                match tokio::time::timeout(Duration::from_secs(5), managed.child.wait()).await {
                    Ok(Ok(status)) => stopped(bus, &name, &status.to_string()),
                    _ => force_kill(bus, &name, &mut managed.child).await,
                }
            }

            drain_io(&mut managed.io_tasks).await;
        }
    }
}

fn stopped(bus: &Bus, name: &str, status: &str) {
    event!(bus, "arig: {name} stopped ({status})");
    bus.emit(Event::ServiceExited {
        name: name.to_string(),
        status: status.to_string(),
    });
}

async fn force_kill(bus: &Bus, name: &str, child: &mut tokio::process::Child) {
    event!(bus, "arig: {name} did not stop in time, force killing");
    let _ = child.kill().await;
    bus.emit(Event::ServiceExited {
        name: name.to_string(),
        status: "killed".to_string(),
    });
}
