mod logs;
mod msg;
mod platform;

use crate::config::{ArigConfig, ServiceConfig, ServiceType};
use crate::dag;
use crate::event::{Bus, Event, ServiceKind, event};
use crate::ipc;
use crate::probe::ReadyCheck;
use crate::protocol;
use crate::registry::{BoundProbe, Registry};
use crate::runtime::{Exit, RunningService, StopOutcome};
use crate::sink;
use crate::state::{self, Startup, StateTracker};
use anyhow::Context;
use futures::future::select_all;
use logs::{LastOutput, LogTail};
use msg::{KernelMsg, LifecycleReq, Phase, Seq};
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::signal;
use tokio::sync::{mpsc, oneshot};

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

/// What the steady-state loop owns: the services it can still reach, and the
/// lifecycle commands it has under way.
struct Steady {
    children: Vec<ManagedChild>,
    /// Which wave each service started in, so one that is stopped and started
    /// again keeps its place in the shutdown order.
    wave_of: HashMap<String, usize>,
    /// One entry per service with a command in flight. A second command for
    /// the same service is refused rather than queued.
    inflight: HashMap<String, InFlight>,
    /// Handed to each command as it is accepted, so a phase reporting back
    /// late can be told from the one running now.
    next_seq: Seq,
}

/// A lifecycle command the kernel has accepted and not yet answered.
struct InFlight {
    seq: Seq,
    /// The client waiting on it. Taken when the command settles, or when a
    /// `--no-wait` start has got as far as the client asked for.
    reply: Option<oneshot::Sender<Result<(), String>>>,
    no_wait: bool,
    /// Phases left to run.
    plan: VecDeque<Phase>,
    /// The definition a start phase will spawn, read when the command was
    /// accepted rather than when the phase runs: a restart has to fail on a
    /// bad edit before it stops anything.
    spec: Option<ServiceConfig>,
    /// What a build phase will run, read at the same point and for the same
    /// reason.
    build: Option<ServiceConfig>,
    /// The task running the current phase, if the phase has one.
    task: Option<Task>,
}

/// A phase running outside the kernel loop. The two are handled differently
/// on shutdown, which is the reason they are distinguished at all.
enum Task {
    /// Abandoned when the stack goes down: nothing is waiting for a service
    /// to become ready once everything is stopping.
    Probe(tokio::task::JoinHandle<()>),
    /// Waited out when the stack goes down, so the service is actually gone
    /// before the supervisor is.
    Work(tokio::task::JoinHandle<()>),
}

/// How far `start` got before it had to hand off.
enum Started {
    /// Spawned, with nothing left to wait for.
    Spawned,
    /// Spawned, with a readiness probe still to answer.
    Probing,
}

impl Steady {
    /// Finish a command and answer its client. Every path out of a command
    /// ends here, so a client is never left holding a connection that will
    /// not be written to.
    fn settle(&mut self, name: &str, result: Result<(), String>) {
        if let Some(mut entry) = self.inflight.remove(name)
            && let Some(reply) = entry.reply.take()
        {
            let _ = reply.send(result);
        }
    }

    /// Answer a client without finishing the command, for a start that is not
    /// waiting on its probe.
    fn answer(&mut self, name: &str, result: Result<(), String>) {
        if let Some(entry) = self.inflight.get_mut(name)
            && let Some(reply) = entry.reply.take()
        {
            let _ = reply.send(result);
        }
    }

    fn clear_task(&mut self, name: &str) {
        if let Some(entry) = self.inflight.get_mut(name) {
            entry.task = None;
        }
    }

    /// Whether a completion belongs to the command this service is running
    /// now. One from a command that is already over says nothing about it.
    fn is_current(&self, name: &str, seq: Seq) -> bool {
        self.inflight
            .get(name)
            .is_some_and(|entry| entry.seq == seq)
    }

    /// Which command a phase about to be spawned belongs to.
    fn seq_of(&self, name: &str) -> Seq {
        self.inflight.get(name).map(|entry| entry.seq).unwrap_or(0)
    }
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
    /// Where that config was read from, so a start can read it again and pick
    /// up an edit. `None` leaves every start on the definition the stack came
    /// up with, which is what the tests run on.
    config_path: Option<std::path::PathBuf>,
    bus: Bus,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    registry: Registry,
    /// Where the startup verdict is recorded for `arig wait` to read.
    state: StateTracker,
    /// Whether this supervisor was spawned by `up --detach`. Only affects what
    /// it tells the reader to do to stop it: there is no tty to ctrl-c.
    detached: bool,
    /// Handed to everything that has to reach the loop: the IPC clients that
    /// issue lifecycle commands, and the tasks running their phases. Unbounded
    /// on purpose: the kernel waits out a phase task on the way down, and a
    /// bounded queue that filled would leave that task blocked on a send
    /// nobody is draining. The queue is bounded in practice by the number of
    /// connected clients.
    msg_tx: mpsc::UnboundedSender<KernelMsg>,
}

pub async fn up(
    config: ArigConfig,
    config_path: Option<std::path::PathBuf>,
    detached: bool,
) -> anyhow::Result<()> {
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
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let _ipc_task = tokio::spawn(ipc_accept_loop(
        acceptor,
        state.clone(),
        shutdown_tx.clone(),
        bus.clone(),
        msg_tx.clone(),
    ));
    event!(bus, "arig: ipc bound at {}", endpoint.address);

    let kernel = Kernel {
        config,
        config_path,
        bus: bus.clone(),
        shutdown_rx,
        registry,
        state: state.clone(),
        detached,
        msg_tx,
    };
    let result = kernel.run(msg_rx).await;

    // Whatever happened, anything blocked in `arig wait` needs an answer
    // rather than the closed connection it would get when we exit. A startup
    // that already settled keeps its verdict.
    state.finish_startup(match &result {
        Ok(()) => Startup::Failed("supervisor stopped before startup finished".to_string()),
        Err(err) => Startup::Failed(err.to_string()),
    });

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
            if let Some(pending) = self.resolve_probe(name, service)? {
                probes.insert(name.as_str(), pending);
            }
        }
        Ok(probes)
    }

    /// Bind the readiness check one service asks for. Oneshots are skipped:
    /// dependents already wait for them to exit, so a ready block on one has
    /// never meant anything.
    fn resolve_probe(
        &self,
        name: &str,
        service: &ServiceConfig,
    ) -> anyhow::Result<Option<PendingProbe>> {
        if service.service_type == ServiceType::Oneshot {
            return Ok(None);
        }
        let Some(spec) = &service.ready else {
            return Ok(None);
        };
        let bound = self
            .registry
            .ready_check(spec)
            .with_context(|| format!("service '{name}'"))?;
        Ok(bound.map(|probe| PendingProbe {
            probe,
            timeout: spec.timeout,
        }))
    }

    /// Spawn one service and wire its output into the logs. The startup waves
    /// and `arig start` share this, so both leave the same plumbing behind.
    async fn spawn_service(
        &self,
        name: &str,
        service: &ServiceConfig,
        wave: usize,
        probed: bool,
    ) -> anyhow::Result<ManagedChild> {
        let runtime = self.registry.runtime(&service.runtime)?;
        let mut spawned = runtime.spawn(name, service).await?;
        let pid = spawned.handle.pid();
        match pid {
            Some(pid) => event!(self.bus, "arig: started {name} (PID {pid})"),
            None => event!(self.bus, "arig: started {name}"),
        }

        let tail = logs::new_tail();
        let last_output: LastOutput = Arc::new(Mutex::new(Instant::now()));
        let io_tasks = logs::pipe_output(&mut spawned, name, &tail, &last_output, &self.bus);

        self.bus.emit(Event::ServiceStarted {
            name: name.to_string(),
            wave,
            kind: ServiceKind::from(&service.service_type),
            pid,
            probed,
            depends_on: service.depends_on.clone(),
        });

        Ok(ManagedChild {
            name: name.to_string(),
            wave,
            handle: spawned.handle,
            tail,
            last_output,
            io_tasks,
        })
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

    /// Fail startup now, so anything blocked in `arig wait` hears the reason
    /// before the shutdown that follows rather than after it.
    fn fail_startup(&self, reason: impl Into<String>) {
        self.state.finish_startup(Startup::Failed(reason.into()));
    }

    /// Report every service already gone, and answer with the first of them.
    /// A long-running service is not waited on until every wave is up, so one
    /// that died while a later wave was still coming up has gone unnoticed;
    /// without this the stack is called ready and torn down a moment later.
    async fn exited_during_startup(&self, children: &mut [ManagedChild]) -> Option<usize> {
        let mut first = None;
        for (idx, managed) in children.iter_mut().enumerate() {
            let Some(exit) = managed.handle.try_exit().await else {
                continue;
            };
            event!(
                self.bus,
                "arig: service '{}' exited during startup ({exit})",
                managed.name
            );
            self.bus.emit(Event::ServiceExited {
                name: managed.name.clone(),
                status: exit.to_string(),
            });
            first.get_or_insert(idx);
        }
        first
    }

    async fn run(&self, msg_rx: mpsc::UnboundedReceiver<KernelMsg>) -> anyhow::Result<()> {
        let waves = dag::toposort(&self.config)?;
        self.validate_services()?;
        let mut probes = self.resolve_probes()?;
        let mut children: Vec<ManagedChild> = Vec::new();

        for (wave_idx, wave) in waves.iter().enumerate() {
            let mut wave_oneshots: Vec<ManagedChild> = Vec::new();
            let mut wave_probes: Vec<(String, PendingProbe)> = Vec::new();

            for name in wave {
                let service = &self.config.services[name];
                let probed = probes.contains_key(name.as_str());
                let managed = self.spawn_service(name, service, wave_idx, probed).await?;

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
                        self.fail_startup(format!("oneshot '{}' failed", managed.name));
                        drain_io(&mut managed.io_tasks).await;
                        dump_tail(&self.bus, &managed.name, &managed.tail);
                        shutdown(&self.bus, &mut children, None).await;
                        anyhow::bail!("oneshot '{}' failed", managed.name);
                    }
                    Err(err) => {
                        event!(self.bus, "arig: {err}");
                        self.fail_startup(err.to_string());
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
                    self.fail_startup(err.to_string());
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

        // Every wave is up and every probe has passed, but a probe that took
        // a while is time a service in an earlier wave had to die in. Ask
        // before calling the stack ready.
        if let Some(idx) = self.exited_during_startup(&mut children).await {
            let name = children[idx].name.clone();
            self.fail_startup(format!("service '{name}' exited during startup"));
            drain_io(&mut children[idx].io_tasks).await;
            dump_tail(&self.bus, &name, &children[idx].tail);
            shutdown(&self.bus, &mut children, Some(idx)).await;
            event!(self.bus, "arig: all services stopped.");
            anyhow::bail!("service '{name}' exited during startup");
        }

        self.state.finish_startup(Startup::Ready);

        if children.is_empty() {
            // Only reachable for a stack of oneshots: there is no service to
            // command later, so there is nothing to stay up for. A stack whose
            // services are all stopped afterwards does stay up.
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

        let mut steady = Steady {
            children,
            wave_of: waves
                .iter()
                .enumerate()
                .flat_map(|(idx, wave)| wave.iter().map(move |name| (name.clone(), idx)))
                .collect(),
            inflight: HashMap::new(),
            next_seq: 0,
        };
        self.steady_state(&mut steady, msg_rx).await
    }

    /// Everything after the last wave is up. The kernel sits here until it is
    /// told to shut down, running lifecycle commands as they arrive and taking
    /// the stack down if a service exits on its own.
    async fn steady_state(
        &self,
        steady: &mut Steady,
        mut msg_rx: mpsc::UnboundedReceiver<KernelMsg>,
    ) -> anyhow::Result<()> {
        let mut rx = self.shutdown_rx.clone();
        loop {
            let action = {
                // The wait futures borrow the children, so they have to be
                // gone before anything below can touch the collection. Both
                // runtimes answer a repeated wait, so rebuilding them every
                // pass loses nothing.
                let waits: Vec<_> = steady
                    .children
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
                    _ = rx.changed() => Action::Shutdown,
                    Some(msg) = msg_rx.recv() => Action::Message(msg),
                    (idx, status) = first_exit(waits) => Action::Exited(idx, status),
                }
            };

            match action {
                Action::Message(msg) => self.handle(steady, msg).await,
                Action::Shutdown => {
                    event!(self.bus, "\narig: shutting down...");
                    self.settle_commands(steady).await;
                    shutdown(&self.bus, &mut steady.children, None).await;
                    event!(self.bus, "arig: all services stopped.");
                    return Ok(());
                }
                Action::Exited(idx, status) => {
                    self.report_unexpected_exit(steady, idx, status).await;
                    self.settle_commands(steady).await;
                    shutdown(&self.bus, &mut steady.children, Some(idx)).await;
                    event!(self.bus, "arig: all services stopped.");
                    anyhow::bail!("a service exited unexpectedly");
                }
            }
        }
    }

    /// A long-running service ended without being asked to. Report it; the
    /// caller takes the rest of the stack down.
    async fn report_unexpected_exit(
        &self,
        steady: &mut Steady,
        idx: usize,
        status: anyhow::Result<Exit>,
    ) {
        let name = steady.children[idx].name.clone();
        match status {
            Ok(status) => {
                event!(
                    self.bus,
                    "arig: service '{name}' exited (status {status}); long-running services aren't expected to exit, shutting down the rest",
                );
                self.bus.emit(Event::ServiceExited {
                    name: name.clone(),
                    status: status.to_string(),
                });
            }
            Err(err) => event!(
                self.bus,
                "arig: service '{name}' wait failed ({err}); shutting down the rest"
            ),
        }
        drain_io(&mut steady.children[idx].io_tasks).await;
        dump_tail(&self.bus, &name, &steady.children[idx].tail);
    }

    /// `down` wins over anything in flight. A probe nobody is waiting for is
    /// abandoned; a stop already under way is waited out, so its service is
    /// gone before the supervisor is.
    async fn settle_commands(&self, steady: &mut Steady) {
        for (_, mut entry) in steady.inflight.drain() {
            match entry.task.take() {
                Some(Task::Probe(task)) => task.abort(),
                Some(Task::Work(task)) => {
                    let _ = task.await;
                }
                None => {}
            }
            if let Some(reply) = entry.reply.take() {
                let _ = reply.send(Err("the supervisor is shutting down".to_string()));
            }
        }
    }

    async fn handle(&self, steady: &mut Steady, message: KernelMsg) {
        match message {
            KernelMsg::Lifecycle { req, reply } => self.accept(steady, req, reply).await,
            KernelMsg::StopFinished { name, seq } => {
                if !steady.is_current(&name, seq) {
                    return;
                }
                steady.clear_task(&name);
                self.advance(steady, &name).await;
            }
            KernelMsg::ProbeSettled { name, seq, result } => {
                if !steady.is_current(&name, seq) {
                    return;
                }
                steady.clear_task(&name);
                match result {
                    Ok(()) => self.advance(steady, &name).await,
                    // The service stays up. The client asked for a start, and
                    // a probe that gave up is something to report rather than
                    // something to undo.
                    Err(reason) => steady.settle(&name, Err(reason)),
                }
            }
            KernelMsg::BuildFinished { name, seq, result } => {
                if !steady.is_current(&name, seq) {
                    return;
                }
                steady.clear_task(&name);
                match result {
                    Ok(()) => self.advance(steady, &name).await,
                    // Nothing else has happened yet: a restart that was going
                    // to stop the service never gets that far, so a broken
                    // edit leaves a working service running.
                    Err(reason) => steady.settle(&name, Err(reason)),
                }
            }
        }
    }

    /// Take a command from a client, or refuse it.
    async fn accept(
        &self,
        steady: &mut Steady,
        req: LifecycleReq,
        reply: oneshot::Sender<Result<(), String>>,
    ) {
        let name = req.service().to_string();
        // Everything a command needs from the config is read here, while the
        // running instance is still untouched, so a bad edit is refused
        // rather than acted on halfway.
        let prepared = self.plan(steady, &req).and_then(|plan| {
            let spec = match plan.contains(&Phase::Start) {
                true => Some(self.prepare(&name)?),
                false => None,
            };
            let build = match plan.contains(&Phase::Build) {
                true => Some(self.build_of(&name)?),
                false => None,
            };
            Ok((plan, spec, build))
        });
        let (plan, spec, build) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => {
                let _ = reply.send(Err(reason));
                return;
            }
        };

        // A start that already answered its client is only still here to
        // watch its probe; the new command takes the service over.
        if let Some(previous) = steady.inflight.remove(&name)
            && let Some(Task::Probe(task)) = previous.task
        {
            task.abort();
        }
        let seq = steady.next_seq;
        steady.next_seq += 1;
        steady.inflight.insert(
            name.clone(),
            InFlight {
                seq,
                reply: Some(reply),
                no_wait: req.no_wait(),
                plan,
                spec,
                build,
                task: None,
            },
        );
        self.advance(steady, &name).await;
    }

    /// Check a command against the stack and turn it into the phases that
    /// carry it out. Everything that can be refused is refused here, before
    /// anything has been stopped or spawned.
    fn plan(&self, steady: &Steady, req: &LifecycleReq) -> Result<VecDeque<Phase>, String> {
        let name = req.service();
        let Some(service) = self.config.services.get(name) else {
            return Err(format!("no service '{name}' in this stack"));
        };
        if service.service_type == ServiceType::Oneshot {
            return Err(format!(
                "'{name}' is a oneshot; lifecycle commands apply to long-running services"
            ));
        }
        // An entry whose client has been answered and whose phases are done
        // is a probe still being watched, which nothing needs to wait for.
        if steady
            .inflight
            .get(name)
            .is_some_and(|entry| entry.reply.is_some() || !entry.plan.is_empty())
        {
            return Err(format!("operation in progress on '{name}'"));
        }

        let running = steady.children.iter().any(|c| c.name == name);
        match req {
            LifecycleReq::Stop { .. } if !running => Err(format!("'{name}' is already stopped")),
            LifecycleReq::Stop { .. } => Ok(VecDeque::from([Phase::Stop])),
            LifecycleReq::Start { .. } if running => Err(format!("'{name}' is already running")),
            LifecycleReq::Start { .. } => Ok(VecDeque::from([Phase::Start])),
            // Building a service that is up is the point of having the verb:
            // build first, stop second, so the service is down for as little
            // as possible.
            LifecycleReq::Build { .. } => Ok(VecDeque::from([Phase::Build])),
            // A restart of something already stopped is a start: what was
            // asked for is that it ends up running.
            LifecycleReq::Restart { build, .. } => {
                let mut plan = VecDeque::new();
                if *build {
                    plan.push_back(Phase::Build);
                }
                if running {
                    plan.push_back(Phase::Stop);
                }
                plan.push_back(Phase::Start);
                Ok(plan)
            }
        }
    }

    /// The build a command would run, resolved through the service's runtime
    /// so that what a build means stays the runtime's to decide.
    fn build_of(&self, name: &str) -> Result<ServiceConfig, String> {
        let spec = self.reload(name)?;
        let runtime = self
            .registry
            .runtime(&spec.runtime)
            .map_err(|err| err.to_string())?;
        runtime
            .build(&spec)
            .ok_or_else(|| format!("'{name}' has no build: command"))
    }

    /// Read one service's definition again, so a start after an edit runs what
    /// the config file says now rather than what it said at `up` time.
    ///
    /// Structural edits are refused: the waves, and with them the shutdown
    /// order, were computed when the stack came up, and a service that no
    /// longer means the same thing in the graph is not this command's to
    /// adopt.
    fn reload(&self, name: &str) -> Result<ServiceConfig, String> {
        let current = self
            .config
            .services
            .get(name)
            .ok_or_else(|| format!("no service '{name}' in this stack"))?;
        let Some(path) = &self.config_path else {
            return Ok(current.clone());
        };

        let mut config = ArigConfig::load(path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let Some(fresh) = config.services.get(name) else {
            return Err(format!(
                "service '{name}' is not in the config anymore; bounce the stack to apply structural changes"
            ));
        };
        if fresh.service_type != current.service_type {
            return Err(format!(
                "'{name}' changed type; bounce the stack to apply structural changes"
            ));
        }
        let before: std::collections::HashSet<&str> =
            current.depends_on.iter().map(String::as_str).collect();
        let after: std::collections::HashSet<&str> =
            fresh.depends_on.iter().map(String::as_str).collect();
        if before != after {
            return Err(format!(
                "'{name}' changed depends_on; bounce the stack to apply structural changes"
            ));
        }

        Ok(config.services.remove(name).expect("just looked it up"))
    }

    /// The definition a start should run, checked against the runtime that
    /// will run it. Everything here happens before the old instance is
    /// stopped, so a bad edit fails the command with the service still up.
    fn prepare(&self, name: &str) -> Result<ServiceConfig, String> {
        let spec = self.reload(name)?;
        self.registry
            .runtime(&spec.runtime)
            .and_then(|runtime| runtime.validate(name, &spec))
            .map_err(|err| err.to_string())?;
        // A ready block no probe can serve is worth failing on here too.
        self.resolve_probe(name, &spec)
            .map_err(|err| err.to_string())?;
        Ok(spec)
    }

    /// Run the next phase of a command, or answer its client once there are
    /// none left.
    async fn advance(&self, steady: &mut Steady, name: &str) {
        loop {
            let next = match steady.inflight.get_mut(name) {
                // Already settled, so nothing is waiting on it.
                None => return,
                Some(entry) => entry.plan.pop_front(),
            };
            let Some(phase) = next else {
                steady.settle(name, Ok(()));
                return;
            };

            match phase {
                Phase::Build => {
                    self.build_phase(steady, name);
                    return;
                }
                Phase::Stop => {
                    self.stop_phase(steady, name);
                    return;
                }
                Phase::Start => match self.start_phase(steady, name).await {
                    Err(reason) => {
                        steady.settle(name, Err(reason));
                        return;
                    }
                    Ok(Started::Spawned) => continue,
                    Ok(Started::Probing) => {
                        // The probe answers for itself; a client that asked
                        // not to wait for it has got what it asked for.
                        if steady.inflight.get(name).is_some_and(|e| e.no_wait) {
                            steady.answer(name, Ok(()));
                        }
                        return;
                    }
                },
            }
        }
    }

    /// Hand a service's build to a task that runs it to completion. The
    /// service itself is untouched: a build alongside a running instance is
    /// the ordinary case.
    fn build_phase(&self, steady: &mut Steady, name: &str) {
        let plan = match steady
            .inflight
            .get_mut(name)
            .and_then(|entry| entry.build.take())
        {
            Some(plan) => plan,
            None => match self.build_of(name) {
                Ok(plan) => plan,
                Err(reason) => {
                    steady.settle(name, Err(reason));
                    return;
                }
            },
        };
        let runtime = match self.registry.runtime(&plan.runtime) {
            Ok(runtime) => runtime.clone(),
            Err(err) => {
                steady.settle(name, Err(err.to_string()));
                return;
            }
        };

        self.bus.emit(Event::BuildStarted {
            name: name.to_string(),
        });
        event!(self.bus, "arig: building '{name}'");

        let seq = steady.seq_of(name);
        let bus = self.bus.clone();
        let msg_tx = self.msg_tx.clone();
        let shutdown = self.shutdown_rx.clone();
        let building = name.to_string();
        let task = tokio::spawn(async move {
            let result = run_build(&bus, runtime.as_ref(), &building, &plan, shutdown).await;
            bus.emit(Event::BuildFinished {
                name: building.clone(),
            });
            let _ = msg_tx.send(KernelMsg::BuildFinished {
                name: building,
                seq,
                result,
            });
        });

        if let Some(entry) = steady.inflight.get_mut(name) {
            entry.task = Some(Task::Work(task));
        }
    }

    /// Hand one service to a task that stops it. It leaves the children on the
    /// way out, so the loop's teardown-on-exit never sees the exit that is
    /// about to happen.
    fn stop_phase(&self, steady: &mut Steady, name: &str) {
        let Some(idx) = steady.children.iter().position(|c| c.name == name) else {
            steady.settle(name, Err(format!("'{name}' is not running")));
            return;
        };
        let mut managed = steady.children.remove(idx);
        self.bus.emit(Event::StopRequested {
            name: name.to_string(),
        });
        event!(self.bus, "arig: stopping '{name}'");

        let seq = steady.seq_of(name);
        let bus = self.bus.clone();
        let msg_tx = self.msg_tx.clone();
        let stopping = name.to_string();
        let task = tokio::spawn(async move {
            managed.handle.begin_stop();
            let status = match managed.handle.finish_stop().await {
                StopOutcome::Exited(status) => status.to_string(),
                StopOutcome::Killed => "killed".to_string(),
            };
            event!(bus, "arig: {stopping} stopped ({status})");
            drain_io(&mut managed.io_tasks).await;
            bus.emit(Event::ServiceStopped {
                name: stopping.clone(),
            });
            let _ = msg_tx.send(KernelMsg::StopFinished {
                name: stopping,
                seq,
            });
        });

        if let Some(entry) = steady.inflight.get_mut(name) {
            entry.task = Some(Task::Work(task));
        }
    }

    /// Spawn one service again, from the definition read when the command was
    /// accepted.
    async fn start_phase(&self, steady: &mut Steady, name: &str) -> Result<Started, String> {
        let service = match steady
            .inflight
            .get_mut(name)
            .and_then(|entry| entry.spec.take())
        {
            Some(spec) => spec,
            None => self.prepare(name)?,
        };
        let probe = self
            .resolve_probe(name, &service)
            .map_err(|err| err.to_string())?;

        self.bus.emit(Event::StartRequested {
            name: name.to_string(),
        });
        let wave = steady.wave_of.get(name).copied().unwrap_or(0);
        let managed = match self
            .spawn_service(name, &service, wave, probe.is_some())
            .await
        {
            Ok(managed) => managed,
            Err(err) => {
                // The row is Starting from the event above, and nothing is
                // coming to move it on.
                self.bus.emit(Event::ServiceStopped {
                    name: name.to_string(),
                });
                return Err(err.to_string());
            }
        };
        steady.children.push(managed);

        let Some(pending) = probe else {
            return Ok(Started::Spawned);
        };

        let seq = steady.seq_of(name);
        let bus = self.bus.clone();
        let msg_tx = self.msg_tx.clone();
        let probing = name.to_string();
        let task = tokio::spawn(async move {
            let result = wait_ready(&bus, &probing, &pending)
                .await
                .map_err(|err| err.to_string());
            let _ = msg_tx.send(KernelMsg::ProbeSettled {
                name: probing,
                seq,
                result,
            });
        });

        if let Some(entry) = steady.inflight.get_mut(name) {
            entry.task = Some(Task::Probe(task));
        }
        Ok(Started::Probing)
    }
}

/// Wait for the first of the children to exit. `select_all` panics on an empty
/// iterator, and a stack whose services have all been stopped is empty, so
/// that case waits for something else to happen instead. The guard belongs
/// here rather than in a `select!` precondition: those disable a branch from
/// being polled, but the expression behind it is evaluated either way.
async fn first_exit<F>(waits: Vec<F>) -> (usize, anyhow::Result<Exit>)
where
    F: std::future::Future<Output = (usize, anyhow::Result<Exit>)> + Unpin,
{
    if waits.is_empty() {
        return std::future::pending().await;
    }
    select_all(waits).await.0
}

/// What the steady-state loop woke up for.
enum Action {
    Message(KernelMsg),
    Shutdown,
    Exited(usize, anyhow::Result<Exit>),
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
    msg_tx: mpsc::UnboundedSender<KernelMsg>,
) {
    loop {
        let stream = match acceptor.accept().await {
            Ok(s) => s,
            Err(_) => break,
        };
        let st = state.clone();
        let sd = shutdown_tx.clone();
        let bus = bus.clone();
        let msg_tx = msg_tx.clone();
        tokio::spawn(async move {
            handle_client(stream, st, sd, bus, msg_tx).await;
        });
    }
}

async fn handle_client(
    stream: ipc::ServerStream,
    state: StateTracker,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    bus: Bus,
    msg_tx: mpsc::UnboundedSender<KernelMsg>,
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
            // No deadline here: the client owns the timeout. A startup that
            // fails answers with the reason rather than leaving the client to
            // infer one from a closed connection.
            let resp = match state.wait_startup().await {
                Startup::Ready => protocol::Response::ok(),
                Startup::Failed(reason) => protocol::Response::err(reason),
                Startup::Pending => protocol::Response::err("startup state unknown"),
            };
            let _ = protocol::write_response(&mut wr, &resp).await;
        }
        protocol::Request::Down => {
            // Flush response before triggering shutdown so the client always
            // sees the ack even if the supervisor exits quickly.
            let _ = protocol::write_response(&mut wr, &protocol::Response::ok()).await;
            bus.emit(Event::ShutdownRequested);
            let _ = shutdown_tx.send(true);
        }
        protocol::Request::Stop { service } => {
            let req = LifecycleReq::Stop { service };
            let _ = protocol::write_response(&mut wr, &lifecycle(&state, &msg_tx, req).await).await;
        }
        protocol::Request::Start { service, no_wait } => {
            let req = LifecycleReq::Start { service, no_wait };
            let _ = protocol::write_response(&mut wr, &lifecycle(&state, &msg_tx, req).await).await;
        }
        protocol::Request::Restart {
            service,
            build,
            no_wait,
        } => {
            let req = LifecycleReq::Restart {
                service,
                build,
                no_wait,
            };
            let _ = protocol::write_response(&mut wr, &lifecycle(&state, &msg_tx, req).await).await;
        }
        protocol::Request::Build { service } => {
            let req = LifecycleReq::Build { service };
            let _ = protocol::write_response(&mut wr, &lifecycle(&state, &msg_tx, req).await).await;
        }
    }
}

/// Hand a lifecycle command to the kernel and hold the connection until it
/// answers, the way `wait` does. The client owns the timeout.
async fn lifecycle(
    state: &StateTracker,
    msg_tx: &mpsc::UnboundedSender<KernelMsg>,
    req: LifecycleReq,
) -> protocol::Response {
    // The kernel only drains its command channel once it is past startup, so
    // a command sent before that would wait on an answer nobody is coming to
    // give.
    if state.startup() == Startup::Pending {
        return protocol::Response::err("the stack is still starting");
    }

    let (reply, answer) = oneshot::channel();
    if msg_tx.send(KernelMsg::Lifecycle { req, reply }).is_err() {
        return protocol::Response::err("the supervisor is not accepting commands");
    }
    match answer.await {
        Ok(Ok(())) => protocol::Response::ok(),
        Ok(Err(reason)) => protocol::Response::err(reason),
        Err(_) => protocol::Response::err("the supervisor stopped before the command finished"),
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

/// Run one build to completion. It gets the log plumbing and the heartbeat a
/// oneshot gets, since it is one: a long quiet build must not read as hung,
/// and its output belongs in the logs under the service's own name.
async fn run_build(
    bus: &Bus,
    runtime: &dyn crate::runtime::Runtime,
    name: &str,
    plan: &ServiceConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let mut spawned = runtime
        .spawn(name, plan)
        .await
        .map_err(|err| format!("cannot start the build for '{name}': {err}"))?;

    let tail = logs::new_tail();
    let last_output: LastOutput = Arc::new(Mutex::new(Instant::now()));
    let mut io_tasks = logs::pipe_output(&mut spawned, name, &tail, &last_output, bus);

    // A build has no timeout unless the operator set one, and the kernel waits
    // this task out before it stops anything, so a build that outlives the
    // shutdown request would hold the whole stack up. Kill it instead: nothing
    // is going to use what it produces.
    let settled = tokio::select! {
        outcome = wait_oneshot(
            bus,
            name,
            spawned.handle.as_mut(),
            plan.timeout,
            last_output.clone(),
        ) => Some(outcome),
        _ = shutdown.changed() => None,
    };
    let Some(outcome) = settled else {
        spawned.handle.kill().await;
        drain_io(&mut io_tasks).await;
        return Err(format!(
            "the build for '{name}' was stopped by the supervisor shutting down"
        ));
    };
    drain_io(&mut io_tasks).await;

    match outcome {
        Ok(exit) if exit.success() => {
            event!(bus, "arig: build for '{name}' finished");
            Ok(())
        }
        Ok(exit) => {
            dump_tail(bus, name, &tail);
            Err(format!("build for '{name}' failed ({exit})"))
        }
        Err(err) => {
            dump_tail(bus, name, &tail);
            Err(err.to_string())
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::broadcast::error::RecvError;

    /// What the kernel asked of the services, in the order it asked.
    #[derive(Clone, Default)]
    struct Journal(Arc<Mutex<Vec<String>>>);

    impl Journal {
        fn record(&self, entry: String) {
            self.0.lock().expect("journal mutex poisoned").push(entry);
        }

        fn entries(&self) -> Vec<String> {
            self.0.lock().expect("journal mutex poisoned").clone()
        }
    }

    /// Stands in for a real runtime so the wave and shutdown ordering can be
    /// tested without spawning anything.
    struct FakeRuntime {
        log: Journal,
        /// Every spawn, with the command it was given, so a test can tell
        /// which definition a start ran.
        spawns: Journal,
        force_kill: bool,
        /// Name of a service that is to report itself already gone the first
        /// time the kernel asks.
        dies_during_startup: Option<String>,
        /// Name of a service whose wait answers straight away, standing in for
        /// one that dies once the stack is up.
        crashes: Option<String>,
        /// Parks every stop until a test releases it, so a command can be
        /// caught in flight.
        hold: Option<Arc<tokio::sync::Notify>>,
        /// What every oneshot, builds included, exits with.
        oneshot_exit: i64,
        /// Once set, every spawn fails, standing in for a command that cannot
        /// be run or an image that will not pull.
        spawn_fails: Option<Arc<AtomicBool>>,
    }

    #[async_trait]
    impl Runtime for FakeRuntime {
        // Registered under the default name, since that is what services
        // resolve to.
        fn name(&self) -> &'static str {
            DEFAULT_RUNTIME
        }

        async fn spawn(&self, name: &str, spec: &ServiceConfig) -> anyhow::Result<SpawnedService> {
            if let Some(fails) = &self.spawn_fails
                && fails.load(Ordering::SeqCst)
            {
                anyhow::bail!("nothing to run '{name}' with");
            }
            self.spawns
                .record(format!("{name} {}", spec.command.as_deref().unwrap_or("-")));
            // A oneshot has to end for the wave to move on, and a crasher has
            // to end for the steady loop to notice it. Builds arrive here as
            // oneshots too, which is what oneshot_exit is for.
            let exits = (spec.service_type == ServiceType::Oneshot)
                .then(|| Exit::from_code(self.oneshot_exit))
                .or_else(|| (self.crashes.as_deref() == Some(name)).then(|| Exit::from_code(1)));
            Ok(SpawnedService {
                handle: Box::new(FakeService {
                    name: name.to_string(),
                    log: self.log.clone(),
                    force_kill: self.force_kill,
                    already_exited: (self.dies_during_startup.as_deref() == Some(name))
                        .then(|| Exit::from_code(3)),
                    exits,
                    hold: self.hold.clone(),
                }),
                stdout: None,
                stderr: None,
            })
        }
    }

    struct FakeService {
        name: String,
        log: Journal,
        force_kill: bool,
        /// What `try_exit` answers, standing in for a service that died while
        /// a later wave was still coming up.
        already_exited: Option<Exit>,
        /// What `wait` answers. `None` is the long-running case: nothing but
        /// a stop ends it.
        exits: Option<Exit>,
        hold: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait]
    impl RunningService for FakeService {
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }

        async fn wait(&mut self) -> anyhow::Result<Exit> {
            match &self.exits {
                Some(exit) => Ok(exit.clone()),
                None => std::future::pending().await,
            }
        }

        async fn try_exit(&mut self) -> Option<Exit> {
            self.already_exited.clone()
        }

        fn begin_stop(&mut self) {
            self.log.record(format!("begin {}", self.name));
        }

        async fn finish_stop(&mut self) -> StopOutcome {
            self.log.record(format!("finish {}", self.name));
            if let Some(hold) = &self.hold {
                hold.notified().await;
            }
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
    /// to connect to. What it answers is shared and settable, so a test can
    /// have a service come up and then stop being ready.
    struct FakeProbe {
        passes: Arc<AtomicBool>,
    }

    impl FakeProbe {
        fn new(passes: bool) -> Self {
            Self {
                passes: Arc::new(AtomicBool::new(passes)),
            }
        }

        /// A handle on the answer, for a test that changes it mid-run.
        fn switch(&self) -> Arc<AtomicBool> {
            self.passes.clone()
        }
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
                passes: self.passes.clone(),
            }))
        }
    }

    struct FakeCheck {
        target: String,
        passes: Arc<AtomicBool>,
    }

    #[async_trait]
    impl ReadyCheck for FakeCheck {
        fn target(&self) -> &str {
            &self.target
        }

        async fn check(&self) -> Result<(), String> {
            if self.passes.load(Ordering::SeqCst) {
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
            build: None,
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

    /// A block the fake probe claims. The timeout is zero by default, so a
    /// probe that never passes gives up on its first attempt instead of
    /// retrying for a minute.
    fn ready_block(timeout: Duration) -> ReadyProbe {
        ReadyProbe {
            tcp: Some("wherever:1".to_string()),
            timeout,
        }
    }

    /// How a run is set up. The defaults are the ordinary case: services that
    /// stay up, no probes, attached to a terminal.
    #[derive(Default)]
    struct Setup {
        force_kill: bool,
        probe: Option<Arc<dyn Probe>>,
        detached: bool,
        /// Service that reports itself already gone the first time the kernel
        /// asks, as one that died while a later wave was still coming up.
        dies_during_startup: Option<String>,
        /// Service that ends on its own once the stack is up.
        crashes: Option<String>,
        /// Services to declare as oneshots rather than long-running.
        oneshots: Vec<String>,
        /// Config file to run from, for the tests that edit one mid-run.
        config_file: Option<std::path::PathBuf>,
        /// What every oneshot, builds included, exits with.
        oneshot_exit: i64,
        /// Parks every stop until the test releases it.
        hold: Option<Arc<tokio::sync::Notify>>,
        /// How long a readiness probe keeps retrying. Zero gives up on the
        /// first attempt, which is what most tests want.
        probe_timeout: Duration,
        /// Flipped on by a test to make every spawn from then on fail.
        spawn_fails: Option<Arc<AtomicBool>>,
    }

    /// What a run left behind.
    struct Ran {
        stops: Vec<String>,
        events: Vec<Event>,
        startup: Startup,
        /// How the kernel itself ended.
        result: anyhow::Result<()>,
    }

    impl Ran {
        /// The `arig: ...` lines, in the order they were emitted.
        fn lines(&self) -> Vec<&str> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    Event::Supervisor { line } => Some(line.as_str()),
                    _ => None,
                })
                .collect()
        }
    }

    /// A kernel, with everything a test needs to drive it and to see what it
    /// did. With a probe, every service gets a readiness block for it to
    /// answer.
    struct Stack {
        kernel: Option<Kernel>,
        msg_rx: Option<mpsc::UnboundedReceiver<KernelMsg>>,
        msg_tx: mpsc::UnboundedSender<KernelMsg>,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
        bus: Bus,
        collected: tokio::sync::broadcast::Receiver<Event>,
        state: StateTracker,
        log: Journal,
        spawns: Journal,
        /// Fed everything the kernel emits, so a test can read the rows `ps`
        /// would show without waiting on the tracker's own task.
        tracker: StateTracker,
        events: Vec<Event>,
        running: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    }

    /// Build a kernel over `services`, or over the config file `setup` names,
    /// in which case `services` is not used: the file is the definition, and
    /// re-reading it is the point of those tests.
    fn build(services: &[(&str, &[&str])], setup: Setup) -> Stack {
        let bus = Bus::new(256);
        let collected = bus.subscribe();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let log = Journal::default();
        let spawns = Journal::default();
        let state = StateTracker::default();

        let mut registry = Registry::default();
        registry.register(Arc::new(FakeRuntime {
            log: log.clone(),
            spawns: spawns.clone(),
            force_kill: setup.force_kill,
            dies_during_startup: setup.dies_during_startup,
            crashes: setup.crashes,
            hold: setup.hold,
            oneshot_exit: setup.oneshot_exit,
            spawn_fails: setup.spawn_fails,
        }));
        let ready = setup.probe.is_some();
        if let Some(probe) = setup.probe {
            registry.register_probe(probe);
        }

        let config = match &setup.config_file {
            Some(path) => ArigConfig::load(path).expect("the test config must parse"),
            None => {
                let mut config: HashMap<String, ServiceConfig> = services
                    .iter()
                    .map(|(name, deps)| {
                        let block = ready.then(|| ready_block(setup.probe_timeout));
                        (name.to_string(), service(deps, block))
                    })
                    .collect();
                for name in &setup.oneshots {
                    config
                        .get_mut(name)
                        .expect("a oneshot must be one of the services")
                        .service_type = ServiceType::Oneshot;
                }
                ArigConfig {
                    dirs: DirsConfig::default(),
                    services: config,
                }
            }
        };

        let kernel = Kernel {
            config,
            config_path: setup.config_file,
            bus: bus.clone(),
            shutdown_rx,
            registry,
            state: state.clone(),
            detached: setup.detached,
            msg_tx: msg_tx.clone(),
        };

        Stack {
            kernel: Some(kernel),
            msg_rx: Some(msg_rx),
            msg_tx,
            shutdown_tx,
            bus,
            collected,
            state,
            log,
            spawns,
            tracker: StateTracker::default(),
            events: Vec::new(),
            running: None,
        }
    }

    /// Run `body` against a started stack, then shut it down and report what
    /// it left behind. The kernel runs on this task's local set rather than a
    /// task of its own: it holds references to its services across awaits, so
    /// it cannot be moved to another thread.
    async fn with_stack<F>(services: &[(&str, &[&str])], setup: Setup, body: F) -> Ran
    where
        F: AsyncFnOnce(&mut Stack),
    {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let mut stack = build(services, setup);
                let kernel = stack.kernel.take().expect("kernel");
                let msg_rx = stack.msg_rx.take().expect("command channel");
                stack.running = Some(tokio::task::spawn_local(
                    async move { kernel.run(msg_rx).await },
                ));
                // Commands are only answered once the kernel is past startup.
                stack.state.wait_startup().await;
                body(&mut stack).await;
                stack.down().await
            })
            .await
    }

    impl Stack {
        /// Issue a command the way a client does, and wait for its answer.
        async fn command(&self, req: LifecycleReq) -> Result<(), String> {
            self.send(req)
                .await
                .expect("the kernel answers every command")
        }

        /// Issue a command without waiting for its answer.
        fn send(&self, req: LifecycleReq) -> oneshot::Receiver<Result<(), String>> {
            let (reply, answer) = oneshot::channel();
            self.msg_tx
                .send(KernelMsg::Lifecycle { req, reply })
                .map_err(|_| "the kernel takes commands")
                .expect("send");
            answer
        }

        /// Apply everything emitted so far, so both the rows and the event
        /// log below are up to date.
        fn pump(&mut self) {
            use tokio::sync::broadcast::error::TryRecvError;
            loop {
                match self.collected.try_recv() {
                    Ok(event) => {
                        self.tracker.apply(&event);
                        self.events.push(event);
                    }
                    Err(TryRecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
        }

        /// The rows `arig ps` would show.
        fn rows(&mut self) -> Vec<crate::protocol::ServiceSnapshot> {
            self.pump();
            self.tracker.snapshot()
        }

        fn row(&mut self, name: &str) -> crate::protocol::ServiceSnapshot {
            self.rows()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("no row for '{name}'"))
        }

        fn stops(&self) -> Vec<String> {
            self.log.entries()
        }

        /// Every spawn so far, as "<service> <command>".
        fn spawns(&self) -> Vec<String> {
            self.spawns.entries()
        }

        fn is_running(&self) -> bool {
            !self.running.as_ref().expect("kernel task").is_finished()
        }

        /// Ask the stack to shut down without waiting for it, the way ctrl-c
        /// does mid-command.
        fn interrupt(&self) {
            let _ = self.shutdown_tx.send(true);
        }

        /// Give the kernel and its tasks a chance to reach `condition`. Bounded
        /// so a regression fails the test rather than hanging it.
        async fn until(&mut self, what: &str, mut condition: impl FnMut(&mut Stack) -> bool) {
            for _ in 0..1000 {
                if condition(self) {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("the stack never reached: {what}");
        }

        /// Shut the stack down and report what it left behind.
        async fn down(mut self) -> Ran {
            let _ = self.shutdown_tx.send(true);
            let result = self
                .running
                .take()
                .expect("kernel task")
                .await
                .expect("kernel task");
            self.pump();
            Ran {
                stops: self.log.entries(),
                events: self.events,
                startup: self.state.wait_startup().await,
                result,
            }
        }
    }

    fn stop(service: &str) -> LifecycleReq {
        LifecycleReq::Stop {
            service: service.to_string(),
        }
    }

    fn start(service: &str) -> LifecycleReq {
        LifecycleReq::Start {
            service: service.to_string(),
            no_wait: false,
        }
    }

    fn restart(service: &str) -> LifecycleReq {
        LifecycleReq::Restart {
            service: service.to_string(),
            build: false,
            no_wait: false,
        }
    }

    fn rebuild_and_restart(service: &str) -> LifecycleReq {
        LifecycleReq::Restart {
            service: service.to_string(),
            build: true,
            no_wait: false,
        }
    }

    fn build_only(service: &str) -> LifecycleReq {
        LifecycleReq::Build {
            service: service.to_string(),
        }
    }

    /// Run the kernel over `services`, ask it to shut down once they are all
    /// up, and report what the runtime saw, what reached the bus, and how
    /// startup ended.
    async fn run_to_shutdown(services: &[(&str, &[&str])], setup: Setup) -> Ran {
        let mut stack = build(services, setup);
        let kernel = stack.kernel.take().expect("kernel");
        let msg_rx = stack.msg_rx.take().expect("command channel");
        let mut starts = stack.bus.subscribe();
        let shutdown_tx = stack.shutdown_tx.clone();

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

        // Not asserted on here: a run that fails is a case under test, and the
        // verdict below says so more precisely than the error does.
        let result = kernel.run(msg_rx).await;
        stack.state.finish_startup(match &result {
            Ok(()) => Startup::Failed("supervisor stopped before startup finished".to_string()),
            Err(err) => Startup::Failed(err.to_string()),
        });
        trigger.await.expect("trigger task");

        stack.pump();
        Ran {
            stops: stack.log.entries(),
            events: stack.events,
            startup: stack.state.wait_startup().await,
            result,
        }
    }

    #[tokio::test]
    async fn services_stop_in_reverse_wave_order() {
        let ran = run_to_shutdown(&[("db", &[]), ("api", &["db"])], Setup::default()).await;

        assert_eq!(
            ran.stops,
            ["begin api", "finish api", "begin db", "finish db"]
        );
    }

    #[tokio::test]
    async fn a_whole_wave_is_signalled_before_any_of_it_is_waited_on() {
        let ran = run_to_shutdown(&[("a", &[]), ("b", &[])], Setup::default()).await;

        // Which of the two goes first follows config iteration order, so the
        // claim is about the split between signalling and waiting.
        let stops = ran.stops;
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
        let ran = run_to_shutdown(
            &[("a", &[])],
            Setup {
                force_kill: true,
                ..Setup::default()
            },
        )
        .await;

        let exits: Vec<_> = ran
            .events
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
        let ran = run_to_shutdown(
            &[("db", &[]), ("api", &["db"])],
            Setup {
                probe: Some(Arc::new(FakeProbe::new(true))),
                ..Setup::default()
            },
        )
        .await;

        // 'api' is in the second wave, so it only started because the probe
        // for 'db' was consulted and passed first.
        let lines = ran.lines();
        assert!(lines.contains(&"arig: 'db' is ready"), "got: {lines:#?}");
    }

    #[tokio::test]
    async fn a_passing_probe_reports_the_service_ready_on_the_bus() {
        let ran = run_to_shutdown(
            &[("db", &[])],
            Setup {
                probe: Some(Arc::new(FakeProbe::new(true))),
                ..Setup::default()
            },
        )
        .await;

        // The log line above is for a reader; this is what `arig ps` and
        // `arig wait` are derived from.
        let events = &ran.events;
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
    async fn startup_is_ready_once_every_wave_is_up() {
        let ran = run_to_shutdown(&[("db", &[]), ("api", &["db"])], Setup::default()).await;

        let started = ran
            .events
            .iter()
            .filter(|e| matches!(e, Event::ServiceStarted { .. }))
            .count();
        assert_eq!(started, 2);
        assert_eq!(ran.startup, Startup::Ready);
    }

    /// The gap this closes: a service in an earlier wave dies while a later
    /// wave is still coming up, and nothing waits on it until every wave is
    /// done, so the stack would be called ready and torn down a moment later.
    #[tokio::test]
    async fn a_service_that_died_during_startup_fails_the_startup() {
        let ran = run_to_shutdown(
            &[("crasher", &[]), ("api", &["crasher"])],
            Setup {
                dies_during_startup: Some("crasher".to_string()),
                ..Setup::default()
            },
        )
        .await;

        assert_eq!(
            ran.startup,
            Startup::Failed("service 'crasher' exited during startup".to_string()),
        );
        // And `ps` is told, so the row stops claiming it is running.
        assert!(
            ran.events
                .iter()
                .any(|e| matches!(e, Event::ServiceExited { name, .. } if name == "crasher")),
            "got: {:#?}",
            ran.events
        );
    }

    #[tokio::test]
    async fn a_probe_that_never_passes_fails_the_startup_with_its_reason() {
        let ran = run_to_shutdown(
            &[("api", &[])],
            Setup {
                probe: Some(Arc::new(FakeProbe::new(false))),
                ..Setup::default()
            },
        )
        .await;

        // The reason reaches `arig wait`, which has no other way to learn it.
        let Startup::Failed(reason) = &ran.startup else {
            panic!(
                "a probe that never passes must fail startup, got: {:?}",
                ran.startup
            );
        };
        assert!(reason.contains("nothing there"), "got: {reason}");
    }

    #[tokio::test]
    async fn a_detached_supervisor_does_not_advise_ctrl_c() {
        let ran = run_to_shutdown(
            &[("a", &[])],
            Setup {
                detached: true,
                ..Setup::default()
            },
        )
        .await;

        let running = ran
            .lines()
            .into_iter()
            .find(|line| line.contains("service(s) running"))
            .expect("the running line must be emitted");
        assert!(running.contains("arig down"), "got: {running}");
        assert!(!running.contains("Ctrl+C"), "got: {running}");
    }

    #[tokio::test]
    async fn a_probe_that_never_passes_fails_the_wave() {
        let mut stack = build(
            &[("api", &[])],
            Setup {
                probe: Some(Arc::new(FakeProbe::new(false))),
                ..Setup::default()
            },
        );
        let kernel = stack.kernel.take().expect("kernel");
        let msg_rx = stack.msg_rx.take().expect("command channel");

        let err = kernel
            .run(msg_rx)
            .await
            .expect_err("a service that never becomes ready must fail the wave");
        assert!(
            err.to_string().contains("readiness probe failed for 'api'"),
            "got: {err}"
        );
        // The service that never came up is still stopped on the way out.
        assert_eq!(stack.stops(), ["begin api", "finish api"]);
    }

    #[tokio::test]
    async fn stopping_one_service_leaves_the_rest_running() {
        let ran = with_stack(
            &[("db", &[]), ("api", &["db"])],
            Setup::default(),
            async |stack| {
                stack.command(stop("api")).await.expect("stop 'api'");

                assert_eq!(stack.stops(), ["begin api", "finish api"]);
                assert_eq!(stack.row("api").status, "stopped");
                assert_eq!(stack.row("api").desired.as_deref(), Some("stopped"));
                assert_eq!(stack.row("db").status, "running");
                assert!(stack.is_running(), "a stop is not a reason to exit");
            },
        )
        .await;

        ran.result.expect("a deliberate stop is not a failure");
        // 'api' is stopped once, by the command, and is not reached again on
        // the way out.
        assert_eq!(
            ran.stops,
            ["begin api", "finish api", "begin db", "finish db"]
        );
    }

    /// The supervisor is what a later `start` talks to, so it has to outlive
    /// the last service it was running.
    #[tokio::test]
    async fn the_supervisor_stays_up_with_every_service_stopped() {
        let ran = with_stack(&[("api", &[])], Setup::default(), async |stack| {
            stack.command(stop("api")).await.expect("stop 'api'");
            assert!(stack.is_running(), "the last stop must not end the run");

            stack.command(start("api")).await.expect("start 'api'");
            assert_eq!(stack.row("api").status, "running");
            assert_eq!(stack.row("api").desired.as_deref(), Some("up"));
        })
        .await;

        ran.result.expect("clean shutdown");
        assert_eq!(
            ran.stops,
            ["begin api", "finish api", "begin api", "finish api"]
        );
    }

    #[tokio::test]
    async fn a_restart_counts_and_runs_the_probe_again() {
        let ran = with_stack(
            &[("api", &[])],
            Setup {
                probe: Some(Arc::new(FakeProbe::new(true))),
                ..Setup::default()
            },
            async |stack| {
                stack.command(restart("api")).await.expect("restart 'api'");

                let row = stack.row("api");
                assert_eq!(row.status, "running");
                assert_eq!(row.restarts, 1);
                assert_eq!(row.ready, crate::protocol::Readiness::Ready);
            },
        )
        .await;

        ran.result.expect("clean shutdown");
    }

    #[tokio::test]
    async fn a_second_command_for_the_same_service_is_refused() {
        let hold = Arc::new(tokio::sync::Notify::new());
        let released = hold.clone();
        let ran = with_stack(
            &[("api", &[])],
            Setup {
                hold: Some(hold),
                ..Setup::default()
            },
            async |stack| {
                // Parked in finish_stop until the notify below, so the second
                // command arrives while the first is still under way.
                let first = stack.send(stop("api"));
                let refused = stack
                    .command(stop("api"))
                    .await
                    .expect_err("a service already stopping takes no second command");
                assert!(
                    refused.contains("operation in progress on 'api'"),
                    "got: {refused}"
                );

                released.notify_one();
                first
                    .await
                    .expect("the kernel answers every command")
                    .expect("the first stop finishes");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        assert_eq!(ran.stops, ["begin api", "finish api"]);
    }

    #[tokio::test]
    async fn a_oneshot_is_not_a_lifecycle_target() {
        with_stack(
            &[("migrate", &[]), ("api", &["migrate"])],
            Setup {
                oneshots: vec!["migrate".to_string()],
                ..Setup::default()
            },
            async |stack| {
                let refused = stack
                    .command(stop("migrate"))
                    .await
                    .expect_err("a oneshot has no lifecycle to command");
                assert!(refused.contains("oneshot"), "got: {refused}");
            },
        )
        .await
        .result
        .expect("clean shutdown");
    }

    #[tokio::test]
    async fn a_service_that_is_not_in_the_stack_is_refused() {
        with_stack(&[("api", &[])], Setup::default(), async |stack| {
            let refused = stack
                .command(stop("nope"))
                .await
                .expect_err("an unknown service must not be accepted");
            assert!(refused.contains("no service 'nope'"), "got: {refused}");
        })
        .await
        .result
        .expect("clean shutdown");
    }

    #[tokio::test]
    async fn starting_a_running_service_is_refused() {
        with_stack(&[("api", &[])], Setup::default(), async |stack| {
            let refused = stack
                .command(start("api"))
                .await
                .expect_err("'api' is already running");
            assert!(refused.contains("already running"), "got: {refused}");
        })
        .await
        .result
        .expect("clean shutdown");
    }

    /// Shutting down while a stop is under way has to wait that stop out: the
    /// service is in a task rather than in the children, so the teardown below
    /// would otherwise walk straight past it and leave it running.
    #[tokio::test]
    async fn a_shutdown_waits_out_a_stop_already_under_way() {
        let hold = Arc::new(tokio::sync::Notify::new());
        let released = hold.clone();
        let ran = with_stack(
            &[("api", &[])],
            Setup {
                hold: Some(hold),
                ..Setup::default()
            },
            async |stack| {
                let answer = stack.send(stop("api"));
                stack
                    .until("a stop parked mid-flight", |s| {
                        s.stops().contains(&"finish api".to_string())
                    })
                    .await;

                stack.interrupt();
                released.notify_one();

                let refused = answer
                    .await
                    .expect("the kernel answers every command")
                    .expect_err("a command caught by a shutdown is not a success");
                assert!(refused.contains("shutting down"), "got: {refused}");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        // Stopped once, by the command, and not left behind for the teardown
        // that cannot see it.
        assert_eq!(ran.stops, ["begin api", "finish api"]);
    }

    /// A start whose spawn fails has already been reported as starting, and
    /// nothing else is coming to move the row on.
    #[tokio::test]
    async fn a_start_that_cannot_spawn_does_not_leave_the_row_starting() {
        let spawn_fails = Arc::new(AtomicBool::new(false));
        let broken = spawn_fails.clone();
        let ran = with_stack(
            &[("api", &[])],
            Setup {
                spawn_fails: Some(spawn_fails),
                ..Setup::default()
            },
            async |stack| {
                stack.command(stop("api")).await.expect("stop 'api'");
                broken.store(true, Ordering::SeqCst);

                let failed = stack
                    .command(start("api"))
                    .await
                    .expect_err("a spawn that fails fails the command");
                assert!(failed.contains("nothing to run"), "got: {failed}");
                assert_eq!(stack.row("api").status, "stopped");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        assert!(
            ran.events
                .iter()
                .any(|e| matches!(e, Event::StartRequested { name } if name == "api")),
            "the start was reported before it failed",
        );
    }

    /// A probe that gives up is worth reporting, but the process it was
    /// probing is up and stopping it is not this command's business.
    #[tokio::test]
    async fn a_probe_that_gives_up_fails_the_command_and_leaves_it_running() {
        let probe = Arc::new(FakeProbe::new(true));
        let switch = probe.switch();
        let ran = with_stack(
            &[("api", &[])],
            Setup {
                probe: Some(probe),
                ..Setup::default()
            },
            async |stack| {
                switch.store(false, Ordering::SeqCst);

                let failed = stack
                    .command(restart("api"))
                    .await
                    .expect_err("a probe that never passes fails the command");
                assert!(failed.contains("nothing there"), "got: {failed}");

                assert_eq!(stack.row("api").status, "running");
                assert_eq!(stack.row("api").ready, crate::protocol::Readiness::Pending);
            },
        )
        .await;

        ran.result.expect("clean shutdown");
    }

    /// `--no-wait` answers as soon as the service has spawned. The probe it
    /// left running is nobody's business but the log's, so it must not hold
    /// the service against the next command.
    #[tokio::test]
    async fn a_no_wait_start_does_not_hold_the_service() {
        let probe = Arc::new(FakeProbe::new(true));
        let switch = probe.switch();
        let ran = with_stack(
            &[("api", &[])],
            Setup {
                probe: Some(probe),
                // Long enough that the probe is still retrying when the next
                // command arrives.
                probe_timeout: Duration::from_secs(60),
                ..Setup::default()
            },
            async |stack| {
                stack.command(stop("api")).await.expect("stop 'api'");
                switch.store(false, Ordering::SeqCst);

                let req = LifecycleReq::Start {
                    service: "api".to_string(),
                    no_wait: true,
                };
                stack
                    .command(req)
                    .await
                    .expect("start 'api' without waiting");

                stack
                    .command(stop("api"))
                    .await
                    .expect("a probe nobody is waiting for must not refuse the next command");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
    }

    /// A config file only this test writes to, so the edits below are visible
    /// to a restart the way an operator's would be.
    fn temp_config(tag: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("arig-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("arig.yaml");
        std::fs::write(&path, contents).expect("write config");
        path
    }

    #[tokio::test]
    async fn a_restart_runs_the_definition_the_config_holds_now() {
        let path = temp_config("edit", "services:\n  api:\n    command: first\n");
        let edited = path.clone();
        let ran = with_stack(
            &[],
            Setup {
                config_file: Some(path.clone()),
                ..Setup::default()
            },
            async |stack| {
                std::fs::write(&edited, "services:\n  api:\n    command: second\n")
                    .expect("edit the config");
                stack.command(restart("api")).await.expect("restart 'api'");

                assert_eq!(stack.spawns(), ["api first", "api second"]);
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        let _ = std::fs::remove_dir_all(path.parent().expect("config dir"));
    }

    /// The waves, and with them the shutdown order, were computed when the
    /// stack came up; a restart is not where the graph changes.
    #[tokio::test]
    async fn a_restart_after_a_structural_edit_is_refused() {
        let path = temp_config(
            "structural",
            "services:\n  db:\n    command: db\n  api:\n    command: api\n    depends_on: [db]\n",
        );
        let edited = path.clone();
        let ran = with_stack(
            &[],
            Setup {
                config_file: Some(path.clone()),
                ..Setup::default()
            },
            async |stack| {
                std::fs::write(
                    &edited,
                    "services:\n  db:\n    command: db\n  api:\n    command: api\n",
                )
                .expect("edit the config");

                let refused = stack
                    .command(restart("api"))
                    .await
                    .expect_err("a changed depends_on is not this command's to adopt");
                assert!(refused.contains("bounce the stack"), "got: {refused}");
                assert!(stack.stops().is_empty(), "nothing may have been stopped");
                assert_eq!(stack.row("api").status, "running");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        let _ = std::fs::remove_dir_all(path.parent().expect("config dir"));
    }

    #[tokio::test]
    async fn a_config_that_no_longer_parses_leaves_the_service_running() {
        let path = temp_config("unreadable", "services:\n  api:\n    command: first\n");
        let edited = path.clone();
        let ran = with_stack(
            &[],
            Setup {
                config_file: Some(path.clone()),
                ..Setup::default()
            },
            async |stack| {
                std::fs::write(&edited, "services: [not a mapping\n").expect("edit the config");

                let refused = stack
                    .command(restart("api"))
                    .await
                    .expect_err("a config that cannot be read cannot be started from");
                assert!(refused.contains("cannot read"), "got: {refused}");
                assert!(stack.stops().is_empty(), "nothing may have been stopped");
                assert_eq!(stack.row("api").status, "running");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        let _ = std::fs::remove_dir_all(path.parent().expect("config dir"));
    }

    #[tokio::test]
    async fn a_build_runs_the_build_command_and_leaves_the_service_alone() {
        let path = temp_config(
            "build",
            "services:\n  api:\n    command: run\n    build: make\n",
        );
        let ran = with_stack(
            &[],
            Setup {
                config_file: Some(path.clone()),
                ..Setup::default()
            },
            async |stack| {
                stack.command(build_only("api")).await.expect("build 'api'");

                assert_eq!(stack.spawns(), ["api run", "api make"]);
                assert!(stack.stops().is_empty(), "a build stops nothing");
                assert_eq!(stack.row("api").status, "running");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        let _ = std::fs::remove_dir_all(path.parent().expect("config dir"));
    }

    /// The reason to build before stopping: a broken edit must not take a
    /// working service down.
    #[tokio::test]
    async fn a_failed_build_leaves_the_running_instance_untouched() {
        let path = temp_config(
            "build-fails",
            "services:\n  api:\n    command: run\n    build: make\n",
        );
        let ran = with_stack(
            &[],
            Setup {
                config_file: Some(path.clone()),
                oneshot_exit: 1,
                ..Setup::default()
            },
            async |stack| {
                let failed = stack
                    .command(rebuild_and_restart("api"))
                    .await
                    .expect_err("a build that fails fails the command");
                assert!(failed.contains("build for 'api' failed"), "got: {failed}");

                assert!(stack.stops().is_empty(), "nothing may have been stopped");
                assert_eq!(stack.spawns(), ["api run", "api make"]);
                assert_eq!(stack.row("api").status, "running");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        let _ = std::fs::remove_dir_all(path.parent().expect("config dir"));
    }

    #[tokio::test]
    async fn a_service_with_no_build_command_cannot_be_built() {
        let path = temp_config("no-build", "services:\n  api:\n    command: run\n");
        let ran = with_stack(
            &[],
            Setup {
                config_file: Some(path.clone()),
                ..Setup::default()
            },
            async |stack| {
                let refused = stack
                    .command(build_only("api"))
                    .await
                    .expect_err("there is nothing to build");
                assert!(refused.contains("has no build"), "got: {refused}");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        let _ = std::fs::remove_dir_all(path.parent().expect("config dir"));
    }

    #[tokio::test]
    async fn a_restart_with_build_builds_before_it_stops() {
        let path = temp_config(
            "build-restart",
            "services:\n  api:\n    command: run\n    build: make\n",
        );
        let ran = with_stack(
            &[],
            Setup {
                config_file: Some(path.clone()),
                ..Setup::default()
            },
            async |stack| {
                stack
                    .command(rebuild_and_restart("api"))
                    .await
                    .expect("restart 'api' after building it");

                assert_eq!(stack.spawns(), ["api run", "api make", "api run"]);
                assert_eq!(stack.stops(), ["begin api", "finish api"]);
                assert_eq!(stack.row("api").status, "running");
            },
        )
        .await;

        ran.result.expect("clean shutdown");
        let _ = std::fs::remove_dir_all(path.parent().expect("config dir"));
    }

    /// Deliberate stops are the exception; a service ending on its own still
    /// takes the stack with it.
    #[tokio::test]
    async fn an_unexpected_exit_still_takes_the_stack_down() {
        let ran = with_stack(
            &[("db", &[]), ("api", &["db"])],
            Setup {
                crashes: Some("api".to_string()),
                ..Setup::default()
            },
            async |stack| {
                for _ in 0..200 {
                    if !stack.is_running() {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
                panic!("a service that exited must take the stack down");
            },
        )
        .await;

        let err = ran
            .result
            .expect_err("a service exiting on its own is a failure");
        assert!(
            err.to_string().contains("exited unexpectedly"),
            "got: {err}"
        );
        // The rest of the stack is stopped rather than left behind.
        assert_eq!(ran.stops, ["begin db", "finish db"]);
    }
}
