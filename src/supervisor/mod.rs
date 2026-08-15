mod logs;
mod platform;

use crate::config::{ArigConfig, ServiceType};
use crate::dag;
use crate::event::{Bus, Event, ServiceKind, event};
use crate::ipc;
use crate::probe::ReadyCheck;
use crate::protocol;
use crate::registry::{BoundProbe, Registry};
use crate::runtime::{Exit, RunningService, StopOutcome};
use crate::sink;
use crate::state::{self, StateTracker};
use anyhow::Context;
use futures::future::select_all;
use logs::{LastOutput, LogTail};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::signal;

const IO_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// A service the kernel is responsible for: the runtime's handle on it, plus
/// the log plumbing the kernel set up around it.
struct ManagedChild {
    name: String,
    wave: usize,
    handle: Box<dyn RunningService>,
    tail: LogTail,
    last_output: LastOutput,
    io_tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// A readiness check the kernel must see pass before the next wave starts.
struct PendingProbe {
    probe: BoundProbe,
    /// How long to keep retrying before the wave fails.
    timeout: Duration,
}

struct IpcCleanup(ipc::Endpoint);

impl Drop for IpcCleanup {
    fn drop(&mut self) {
        ipc::cleanup(&self.0);
    }
}

/// Owns everything the supervisor needs to drive services: the config, the DAG
/// it came from, the bus every observer hangs off, and the runtimes that do
/// the actual spawning.
struct Kernel {
    config: ArigConfig,
    bus: Bus,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    registry: Registry,
    /// Whether this supervisor was spawned by `up --detach`. Only affects what
    /// it tells the reader to do to stop it: there is no tty to ctrl-c.
    detached: bool,
}

pub async fn up(config: ArigConfig, detached: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    let _job = platform::win::JobGuard::new()?;

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

    let session_dir = sink::file::create_session_dir(&config.dirs.logs)?;
    let mut registry = Registry::with_builtins(&bus, &session_dir);
    // Attach the sinks before the first line is emitted; anything emitted
    // earlier is gone by the time they subscribe.
    let sinks = sink::spawn(&bus, registry.take_sinks());
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
        shutdown_rx,
        registry,
        detached,
    };
    let result = kernel.run().await;

    // The sinks run in a task of their own, so give them a moment to write the
    // lines emitted just before we return.
    bus.drain(&sinks, IO_DRAIN_TIMEOUT).await;
    result
}

impl Kernel {
    /// Bind every readiness check before anything spawns, so a `ready:` block
    /// no probe can serve fails while there is still nothing to clean up.
    /// Oneshots are skipped: dependents already wait for them to exit, so a
    /// ready block on one has never meant anything.
    fn resolve_probes(&self) -> anyhow::Result<HashMap<&str, PendingProbe>> {
        let mut probes = HashMap::new();
        for (name, service) in &self.config.services {
            if service.service_type == ServiceType::Oneshot {
                continue;
            }
            let Some(spec) = &service.ready else { continue };
            let bound = self
                .registry
                .ready_check(spec)
                .with_context(|| format!("service '{name}'"))?;
            if let Some(probe) = bound {
                probes.insert(
                    name.as_str(),
                    PendingProbe {
                        probe,
                        timeout: spec.timeout,
                    },
                );
            }
        }
        Ok(probes)
    }

    /// Check every service against the runtime it selected, before anything
    /// spawns. Same reasoning as the probes: a service block naming a runtime
    /// that does not exist, or keys that runtime cannot use, should fail while
    /// there is still nothing to stop.
    fn validate_services(&self) -> anyhow::Result<()> {
        for (name, service) in &self.config.services {
            self.registry
                .runtime(&service.runtime)
                .and_then(|runtime| runtime.validate(name, service))
                .with_context(|| format!("service '{name}'"))?;
        }
        Ok(())
    }

    async fn run(&self) -> anyhow::Result<()> {
        let waves = dag::toposort(&self.config)?;
        self.validate_services()?;
        let mut probes = self.resolve_probes()?;
        let mut children: Vec<ManagedChild> = Vec::new();

        for (wave_idx, wave) in waves.iter().enumerate() {
            let mut wave_oneshots: Vec<ManagedChild> = Vec::new();
            let mut wave_probes: Vec<(String, PendingProbe)> = Vec::new();

            for name in wave {
                let service = &self.config.services[name];
                let runtime = self.registry.runtime(&service.runtime)?;
                let mut spawned = runtime.spawn(name, service).await?;
                let pid = spawned.handle.pid();
                match pid {
                    Some(pid) => event!(self.bus, "arig: started {name} (PID {pid})"),
                    None => event!(self.bus, "arig: started {name}"),
                }

                let tail = logs::new_tail();
                let last_output: LastOutput = Arc::new(Mutex::new(Instant::now()));
                let io_tasks =
                    logs::pipe_output(&mut spawned, name, &tail, &last_output, &self.bus);

                let managed = ManagedChild {
                    name: name.clone(),
                    wave: wave_idx,
                    handle: spawned.handle,
                    tail,
                    last_output,
                    io_tasks,
                };

                self.bus.emit(Event::ServiceStarted {
                    name: name.clone(),
                    wave: wave_idx,
                    kind: ServiceKind::from(&service.service_type),
                    pid,
                    probed: probes.contains_key(name.as_str()),
                });

                if service.service_type == ServiceType::Oneshot {
                    wave_oneshots.push(managed);
                } else {
                    if let Some(pending) = probes.remove(name.as_str()) {
                        wave_probes.push((name.clone(), pending));
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
                    managed.handle.as_mut(),
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
            for (name, pending) in wave_probes {
                let mut rx = self.shutdown_rx.clone();
                let result = tokio::select! {
                    r = wait_ready(&self.bus, &name, &pending) => r,
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

        // Every wave is up and every probe has passed. Emitted before the
        // console line so a waiter is released by the state change rather than
        // by whatever a sink does with the line.
        self.bus.emit(Event::StartupComplete);

        if children.is_empty() {
            event!(self.bus, "arig: all tasks completed.");
            return Ok(());
        }

        event!(
            self.bus,
            "arig: {} service(s) running. {}",
            children.len(),
            // Detached, ctrl-c reaches nothing: the supervisor called setsid
            // and has no controlling terminal.
            if self.detached {
                "Run `arig down` to stop."
            } else {
                "Press Ctrl+C to stop."
            }
        );

        let mut rx = self.shutdown_rx.clone();
        let exit = {
            let waits: Vec<_> = children
                .iter_mut()
                .enumerate()
                .map(|(i, c)| {
                    Box::pin(async move {
                        let status = c.handle.wait().await;
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

    let child = platform::spawn_detached(&mut cmd)?;
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
        protocol::Request::Wait => {
            // No deadline here: the client owns the timeout, and a supervisor
            // that gives up on startup exits, which the client sees as EOF.
            let mut startup = state.startup();
            while !*startup.borrow_and_update() {
                if startup.changed().await.is_err() {
                    return;
                }
            }
            let _ = protocol::write_response(&mut wr, &protocol::Response::ok()).await;
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
    handle: &mut dyn RunningService,
    timeout: Option<Duration>,
    last_output: LastOutput,
) -> anyhow::Result<Exit> {
    let heartbeat = tokio::spawn(oneshot_heartbeat(
        bus.clone(),
        name.to_string(),
        last_output,
    ));
    let result = wait_oneshot_inner(name, handle, timeout).await;
    heartbeat.abort();
    result
}

async fn wait_oneshot_inner(
    name: &str,
    handle: &mut dyn RunningService,
    timeout: Option<Duration>,
) -> anyhow::Result<Exit> {
    let Some(limit) = timeout else {
        return handle.wait().await;
    };

    match tokio::time::timeout(limit, handle.wait()).await {
        Ok(status) => status,
        Err(_) => {
            handle.kill().await;
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

async fn wait_ready(bus: &Bus, name: &str, pending: &PendingProbe) -> anyhow::Result<()> {
    let PendingProbe { probe, timeout } = pending;
    let kind = probe.kind;
    let check: &dyn ReadyCheck = probe.check.as_ref();
    let target = check.target();

    event!(
        bus,
        "arig: waiting for '{name}' {kind} probe on {target} (timeout {})",
        humantime::format_duration(*timeout),
    );

    let deadline = Instant::now() + *timeout;
    let mut last_heartbeat = Instant::now();
    loop {
        let last_err = match check.check().await {
            Ok(()) => {
                event!(bus, "arig: '{name}' is ready");
                bus.emit(Event::ServiceReady {
                    name: name.to_string(),
                });
                return Ok(());
            }
            Err(e) => e,
        };

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            event!(
                bus,
                "arig: still waiting for '{name}' {kind} {target} (last error: {last_err})"
            );
            last_heartbeat = Instant::now();
        }

        if Instant::now() >= deadline {
            anyhow::bail!(
                "'{name}' {kind} probe '{target}' did not become ready within {}: last error: {last_err}",
                humantime::format_duration(*timeout),
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

        // Set the whole wave on its way out before waiting on any one of them,
        // so services that stop on a signal do so while we are still waiting
        // for the ones that need a shutdown hook.
        for &i in &wave_indices {
            children[i].handle.begin_stop();
        }

        for &i in &wave_indices {
            let managed = &mut children[i];
            let name = managed.name.clone();

            match managed.handle.finish_stop().await {
                StopOutcome::Exited(status) => stopped(bus, &name, &status.to_string()),
                StopOutcome::Killed => {
                    bus.emit(Event::ServiceExited {
                        name,
                        status: "killed".to_string(),
                    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DirsConfig, ReadyProbe, ServiceConfig};
    use crate::probe::Probe;
    use crate::registry::DEFAULT_RUNTIME;
    use crate::runtime::{Runtime, SpawnedService};
    use async_trait::async_trait;
    use tokio::sync::broadcast::error::RecvError;

    /// What the kernel asked of the services, in the order it asked.
    #[derive(Clone, Default)]
    struct StopLog(Arc<Mutex<Vec<String>>>);

    impl StopLog {
        fn record(&self, entry: String) {
            self.0.lock().expect("stop log mutex poisoned").push(entry);
        }

        fn entries(&self) -> Vec<String> {
            self.0.lock().expect("stop log mutex poisoned").clone()
        }
    }

    /// Stands in for a real runtime so the wave and shutdown ordering can be
    /// tested without spawning anything.
    struct FakeRuntime {
        log: StopLog,
        force_kill: bool,
    }

    #[async_trait]
    impl Runtime for FakeRuntime {
        // Registered under the default name, since that is what services
        // resolve to.
        fn name(&self) -> &'static str {
            DEFAULT_RUNTIME
        }

        async fn spawn(&self, name: &str, _spec: &ServiceConfig) -> anyhow::Result<SpawnedService> {
            Ok(SpawnedService {
                handle: Box::new(FakeService {
                    name: name.to_string(),
                    log: self.log.clone(),
                    force_kill: self.force_kill,
                }),
                stdout: None,
                stderr: None,
            })
        }
    }

    struct FakeService {
        name: String,
        log: StopLog,
        force_kill: bool,
    }

    #[async_trait]
    impl RunningService for FakeService {
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }

        async fn wait(&mut self) -> anyhow::Result<Exit> {
            // Long-running: nothing but shutdown ends this.
            std::future::pending().await
        }

        fn begin_stop(&mut self) {
            self.log.record(format!("begin {}", self.name));
        }

        async fn finish_stop(&mut self) -> StopOutcome {
            self.log.record(format!("finish {}", self.name));
            if self.force_kill {
                StopOutcome::Killed
            } else {
                StopOutcome::Exited(Exit::from_code(0))
            }
        }

        async fn kill(&mut self) {
            self.log.record(format!("kill {}", self.name));
        }
    }

    /// Stands in for a real probe so readiness can be driven without anything
    /// to connect to.
    struct FakeProbe {
        passes: bool,
    }

    impl Probe for FakeProbe {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn claims(&self, spec: &ReadyProbe) -> bool {
            spec.tcp.is_some()
        }

        fn prepare(&self, spec: &ReadyProbe) -> anyhow::Result<Box<dyn ReadyCheck>> {
            Ok(Box::new(FakeCheck {
                target: spec.tcp.clone().unwrap_or_default(),
                passes: self.passes,
            }))
        }
    }

    struct FakeCheck {
        target: String,
        passes: bool,
    }

    #[async_trait]
    impl ReadyCheck for FakeCheck {
        fn target(&self) -> &str {
            &self.target
        }

        async fn check(&self) -> Result<(), String> {
            if self.passes {
                Ok(())
            } else {
                Err("nothing there".to_string())
            }
        }
    }

    fn service(depends_on: &[&str], ready: Option<ReadyProbe>) -> ServiceConfig {
        ServiceConfig {
            runtime: DEFAULT_RUNTIME.to_string(),
            command: Some("never run: the fake runtime ignores it".to_string()),
            image: None,
            ports: Vec::new(),
            service_type: ServiceType::Service,
            working_dir: None,
            env: HashMap::new(),
            depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
            ready,
            timeout: None,
            shutdown: None,
        }
    }

    /// A block the fake probe claims. Zero timeout so a probe that never
    /// passes gives up on its first attempt instead of retrying for a minute.
    fn ready_block() -> ReadyProbe {
        ReadyProbe {
            tcp: Some("wherever:1".to_string()),
            timeout: Duration::ZERO,
        }
    }

    /// Run the kernel over `services`, ask it to shut down once they are all
    /// up, and report what the runtime saw and what reached the bus. With a
    /// `probe`, every service gets a readiness block for it to answer.
    async fn run_to_shutdown(
        services: &[(&str, &[&str])],
        force_kill: bool,
        probe: Option<Arc<dyn Probe>>,
    ) -> (Vec<String>, Vec<Event>) {
        run_to_shutdown_detached(services, force_kill, probe, false).await
    }

    async fn run_to_shutdown_detached(
        services: &[(&str, &[&str])],
        force_kill: bool,
        probe: Option<Arc<dyn Probe>>,
        detached: bool,
    ) -> (Vec<String>, Vec<Event>) {
        let bus = Bus::new(64);
        let mut collected = bus.subscribe();
        let mut starts = bus.subscribe();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let log = StopLog::default();

        let mut registry = Registry::default();
        registry.register(Arc::new(FakeRuntime {
            log: log.clone(),
            force_kill,
        }));
        let ready = probe.is_some();
        if let Some(probe) = probe {
            registry.register_probe(probe);
        }

        let kernel = Kernel {
            config: ArigConfig {
                dirs: DirsConfig::default(),
                services: services
                    .iter()
                    .map(|(name, deps)| (name.to_string(), service(deps, ready.then(ready_block))))
                    .collect(),
            },
            bus: bus.clone(),
            shutdown_rx,
            registry,
            detached,
        };

        let expected = services.len();
        let trigger = tokio::spawn(async move {
            let mut seen = 0;
            while seen < expected {
                match starts.recv().await {
                    Ok(Event::ServiceStarted { .. }) => seen += 1,
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                }
            }
            let _ = shutdown_tx.send(true);
        });

        kernel.run().await.expect("kernel run");
        trigger.await.expect("trigger task");

        let mut events = Vec::new();
        while let Ok(event) = collected.try_recv() {
            events.push(event);
        }
        (log.entries(), events)
    }

    #[tokio::test]
    async fn services_stop_in_reverse_wave_order() {
        let (stops, _) = run_to_shutdown(&[("db", &[]), ("api", &["db"])], false, None).await;

        assert_eq!(stops, ["begin api", "finish api", "begin db", "finish db"]);
    }

    #[tokio::test]
    async fn a_whole_wave_is_signalled_before_any_of_it_is_waited_on() {
        let (stops, _) = run_to_shutdown(&[("a", &[]), ("b", &[])], false, None).await;

        // Which of the two goes first follows config iteration order, so the
        // claim is about the split between signalling and waiting.
        assert_eq!(stops.len(), 4, "got: {stops:?}");
        assert!(
            stops[..2].iter().all(|s| s.starts_with("begin")),
            "got: {stops:?}"
        );
        assert!(
            stops[2..].iter().all(|s| s.starts_with("finish")),
            "got: {stops:?}"
        );
    }

    #[tokio::test]
    async fn a_service_the_runtime_had_to_kill_is_reported_as_killed() {
        let (_, events) = run_to_shutdown(&[("a", &[])], true, None).await;

        let exits: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::ServiceExited { name, status } => Some((name.as_str(), status.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(exits, [("a", "killed")]);
    }

    #[tokio::test]
    async fn a_wave_waits_on_the_probe_the_registry_resolved() {
        let (_, events) = run_to_shutdown(
            &[("db", &[]), ("api", &["db"])],
            false,
            Some(Arc::new(FakeProbe { passes: true })),
        )
        .await;

        // 'api' is in the second wave, so it only started because the probe
        // for 'db' was consulted and passed first.
        let lines: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Supervisor { line } => Some(line.as_str()),
                _ => None,
            })
            .collect();
        assert!(lines.contains(&"arig: 'db' is ready"), "got: {lines:#?}");
    }

    #[tokio::test]
    async fn a_passing_probe_reports_the_service_ready_on_the_bus() {
        let (_, events) = run_to_shutdown(
            &[("db", &[])],
            false,
            Some(Arc::new(FakeProbe { passes: true })),
        )
        .await;

        // The log line above is for a reader; this is what `arig ps` and
        // `arig wait` are derived from.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ServiceStarted { probed: true, .. })),
            "got: {events:#?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ServiceReady { name } if name == "db")),
            "got: {events:#?}"
        );
    }

    #[tokio::test]
    async fn startup_is_reported_complete_once_every_wave_is_up() {
        let (_, events) = run_to_shutdown(&[("db", &[]), ("api", &["db"])], false, None).await;

        let started = events
            .iter()
            .filter(|e| matches!(e, Event::ServiceStarted { .. }))
            .count();
        let complete = events
            .iter()
            .position(|e| matches!(e, Event::StartupComplete))
            .expect("startup must be reported complete");
        assert_eq!(started, 2);
        // Nothing may claim the stack is up before the last wave has spawned.
        let last_start = events
            .iter()
            .rposition(|e| matches!(e, Event::ServiceStarted { .. }))
            .expect("services started");
        assert!(complete > last_start, "got: {events:#?}");
    }

    #[tokio::test]
    async fn a_detached_supervisor_does_not_advise_ctrl_c() {
        let (_, events) = run_to_shutdown_detached(&[("a", &[])], false, None, true).await;

        let running = events
            .iter()
            .filter_map(|e| match e {
                Event::Supervisor { line } if line.contains("service(s) running") => Some(line),
                _ => None,
            })
            .next()
            .expect("the running line must be emitted");
        assert!(running.contains("arig down"), "got: {running}");
        assert!(!running.contains("Ctrl+C"), "got: {running}");
    }

    #[tokio::test]
    async fn a_probe_that_never_passes_fails_the_wave() {
        let bus = Bus::new(64);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let log = StopLog::default();

        let mut registry = Registry::default();
        registry.register(Arc::new(FakeRuntime {
            log: log.clone(),
            force_kill: false,
        }));
        registry.register_probe(Arc::new(FakeProbe { passes: false }));

        let kernel = Kernel {
            config: ArigConfig {
                dirs: DirsConfig::default(),
                services: [("api".to_string(), service(&[], Some(ready_block())))].into(),
            },
            bus,
            shutdown_rx,
            registry,
            detached: false,
        };

        let err = kernel
            .run()
            .await
            .expect_err("a service that never becomes ready must fail the wave");
        assert!(
            err.to_string().contains("readiness probe failed for 'api'"),
            "got: {err}"
        );
        // The service that never came up is still stopped on the way out.
        assert_eq!(log.entries(), ["begin api", "finish api"]);
    }
}
