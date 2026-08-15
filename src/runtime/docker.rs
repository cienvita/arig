//! The docker runtime: a service is a container on the local daemon.
//!
//! Containers are named `arig-<service>` so a session can be inspected with
//! plain `docker` commands, and so a container left behind by a crashed run is
//! found and replaced rather than colliding with the new one.

use super::{Exit, OutputStream, RunningService, Runtime, SpawnedService, StopOutcome};
use crate::config::{PortMapping, ServiceConfig};
use crate::event::{Bus, event};
use anyhow::Context;
use async_trait::async_trait;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, KillContainerOptions,
    LogsOptionsBuilder, RemoveContainerOptionsBuilder, StopContainerOptionsBuilder,
    WaitContainerOptions,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// The name this runtime registers under.
pub const NAME: &str = "docker";

/// How long the daemon is given to stop a container before it is killed. Kept
/// in step with the process runtime's grace period.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Buffer between the log demultiplexer and the kernel's readers. Output is
/// drained continuously, so this only has to absorb a burst.
const PIPE_BUFFER: usize = 64 * 1024;

/// bollard puts a deadline on every request, two minutes by default, and it
/// covers the wait for the response head. The daemon does not answer `/wait`
/// until the container exits, so a service outliving that deadline would look
/// like it had ended. The control calls keep the default; only waiting uses
/// this, and it is long rather than absent because bollard has no way to say
/// "no deadline".
const WAIT_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

pub struct DockerRuntime {
    bus: Bus,
}

impl DockerRuntime {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }
}

/// The container name arig gives a service.
fn container_name(service: &str) -> String {
    format!("arig-{service}")
}

#[async_trait]
impl Runtime for DockerRuntime {
    fn name(&self) -> &'static str {
        NAME
    }

    fn validate(&self, name: &str, spec: &ServiceConfig) -> anyhow::Result<()> {
        if spec.image.is_none() {
            anyhow::bail!("service '{name}' has no image");
        }
        for port in &spec.ports {
            PortMapping::parse(port)?;
        }
        // Docker names are [a-zA-Z0-9][a-zA-Z0-9_.-]*, and arig prefixes its
        // own. A service name outside that would fail at create time, which is
        // after other services have started.
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            anyhow::bail!(
                "service '{name}' cannot be a container name; use letters, digits, '_', '.' or '-'"
            );
        }
        if spec.shutdown.is_some() {
            anyhow::bail!(
                "service '{name}' has a shutdown hook, which the docker runtime does not run; the daemon stops the container"
            );
        }
        Ok(())
    }

    async fn spawn(&self, name: &str, spec: &ServiceConfig) -> anyhow::Result<SpawnedService> {
        let image = spec
            .image
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("service '{name}' has no image"))?;

        let docker =
            Docker::connect_with_local_defaults().context("cannot reach the docker daemon")?;
        let waiter = docker.clone().with_timeout(WAIT_TIMEOUT);

        let container = container_name(name);

        // A container from a previous run holds the name. Replace it rather
        // than failing: the config is the source of truth for what should run.
        if remove_container(&docker, &container, &self.bus, name).await {
            event!(self.bus, "arig: replaced leftover container {container}");
        }

        pull_image(&docker, image, &self.bus, name).await?;

        let mut exposed = Vec::new();
        let mut bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for port in &spec.ports {
            let mapping = PortMapping::parse(port)?;
            let key = format!("{}/tcp", mapping.container);
            exposed.push(key.clone());
            bindings.insert(
                key,
                Some(vec![PortBinding {
                    host_ip: None,
                    host_port: Some(mapping.host.to_string()),
                }]),
            );
        }

        let body = ContainerCreateBody {
            image: Some(image.to_string()),
            // Split on whitespace: the container's command is an argv, not a
            // shell line, and there may be no shell in the image to hand it to.
            cmd: spec
                .command
                .as_deref()
                .map(|c| c.split_whitespace().map(str::to_string).collect()),
            env: Some(
                spec.env
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>(),
            ),
            exposed_ports: Some(exposed),
            // No TTY, so the daemon keeps stdout and stderr apart and the two
            // reach the kernel as separate streams the way a process's do.
            tty: Some(false),
            host_config: Some(HostConfig {
                port_bindings: Some(bindings),
                ..Default::default()
            }),
            ..Default::default()
        };

        let options = CreateContainerOptionsBuilder::default()
            .name(&container)
            .build();
        docker
            .create_container(Some(options), body)
            .await
            .with_context(|| format!("cannot create container for '{name}'"))?;

        docker
            .start_container(&container, None)
            .await
            .with_context(|| format!("cannot start container for '{name}'"))?;

        let (stdout, stderr) = pipe_logs(&docker, &container, &self.bus, name);

        Ok(SpawnedService {
            handle: Box::new(DockerService {
                name: name.to_string(),
                container,
                docker,
                waiter,
                bus: self.bus.clone(),
            }),
            stdout: Some(stdout),
            stderr: Some(stderr),
        })
    }
}

/// Remove a container by name, ignoring one that is not there. Reports whether
/// one was actually removed, which only the spawn path has anything to say
/// about. Any other failure is reported: it usually turns into a create failure
/// next, and this says why.
async fn remove_container(docker: &Docker, container: &str, bus: &Bus, service: &str) -> bool {
    let options = RemoveContainerOptionsBuilder::default().force(true).build();
    match docker.remove_container(container, Some(options)).await {
        Ok(()) => true,
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => false,
        Err(e) => {
            event!(
                bus,
                "arig: could not remove container {container} for '{service}': {e}"
            );
            false
        }
    }
}

/// Pull the image. Always attempted: the daemon serves an image it already has
/// from its own store, so this only costs a round trip on the common path.
async fn pull_image(docker: &Docker, image: &str, bus: &Bus, service: &str) -> anyhow::Result<()> {
    let (repo, tag) = match image.rsplit_once(':') {
        // A colon in the last path segment is a tag; one before a '/' is a
        // registry port, e.g. localhost:5000/img.
        Some((repo, tag)) if !tag.contains('/') => (repo, tag),
        _ => (image, "latest"),
    };

    let options = CreateImageOptionsBuilder::default()
        .from_image(repo)
        .tag(tag)
        .build();

    event!(bus, "arig: pulling {repo}:{tag} for '{service}'");
    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(item) = stream.next().await {
        item.with_context(|| format!("cannot pull {repo}:{tag} for '{service}'"))?;
    }
    Ok(())
}

/// Split the daemon's single log stream back into stdout and stderr, so the
/// kernel pipes them the same way it pipes a process's.
fn pipe_logs(
    docker: &Docker,
    container: &str,
    bus: &Bus,
    service: &str,
) -> (OutputStream, OutputStream) {
    let options = LogsOptionsBuilder::default()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .build();
    let mut stream = docker.logs(container, Some(options));

    let (mut out_w, out_r) = tokio::io::duplex(PIPE_BUFFER);
    let (mut err_w, err_r) = tokio::io::duplex(PIPE_BUFFER);
    let bus = bus.clone();
    let service = service.to_string();

    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            let write = match item {
                Ok(LogOutput::StdErr { message }) => err_w.write_all(&message).await,
                // Console and StdIn only appear on a TTY container, which arig
                // does not create; treat anything else as stdout.
                Ok(other) => out_w.write_all(&other.into_bytes()).await,
                Err(e) => {
                    event!(bus, "arig: lost the log stream for '{service}': {e}");
                    return;
                }
            };
            // The reader is gone once the kernel has drained the service, which
            // is the normal end of this task rather than a failure.
            if write.is_err() {
                return;
            }
        }
    });

    (
        Box::new(out_r) as OutputStream,
        Box::new(err_r) as OutputStream,
    )
}

struct DockerService {
    name: String,
    container: String,
    /// Control calls, on the default request deadline.
    docker: Docker,
    /// The same daemon, on a deadline long enough to outlast the service.
    waiter: Docker,
    bus: Bus,
}

impl DockerService {
    /// Wait for the container to exit. bollard turns a non-zero exit into a
    /// transport error, so it is mapped back: a container that exits 1 is a
    /// service that failed, not a call that went wrong.
    async fn wait_exit(&self) -> anyhow::Result<Exit> {
        let mut stream = self
            .waiter
            .wait_container(&self.container, None::<WaitContainerOptions>);

        match stream.next().await {
            Some(Ok(response)) => Ok(Exit::from_code(response.status_code)),
            Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => {
                Ok(Exit::from_code(code))
            }
            Some(Err(e)) => Err(anyhow::Error::new(e)
                .context(format!("cannot wait on the container for '{}'", self.name))),
            None => anyhow::bail!("the daemon closed the wait stream for '{}'", self.name),
        }
    }
}

#[async_trait]
impl RunningService for DockerService {
    /// A container has no pid on this side of the daemon.
    fn pid(&self) -> Option<u32> {
        None
    }

    async fn wait(&mut self) -> anyhow::Result<Exit> {
        self.wait_exit().await
    }

    async fn try_exit(&mut self) -> Option<Exit> {
        let inspect = match self.docker.inspect_container(&self.container, None).await {
            Ok(inspect) => inspect,
            Err(e) => {
                event!(
                    self.bus,
                    "arig: cannot inspect the container for '{}' ({e})",
                    self.name
                );
                return None;
            }
        };
        // Only an explicit `running: false` counts. A daemon that reported
        // neither is not evidence the container is gone.
        let state = inspect.state?;
        if state.running != Some(false) {
            return None;
        }
        Some(Exit::from_code(state.exit_code.unwrap_or_default()))
    }

    fn begin_stop(&mut self) {
        // The kernel wants the whole wave on its way out before it waits on any
        // one of it, and stopping a container is a call to the daemon, so this
        // hands it off rather than blocking.
        let docker = self.docker.clone();
        let container = self.container.clone();
        let bus = self.bus.clone();
        let name = self.name.clone();
        tokio::spawn(async move {
            let options = StopContainerOptionsBuilder::default()
                .t(STOP_GRACE.as_secs() as i32)
                .build();
            if let Err(e) = docker.stop_container(&container, Some(options)).await {
                event!(bus, "arig: could not stop the container for '{name}': {e}");
            }
        });
    }

    async fn finish_stop(&mut self) -> StopOutcome {
        // begin_stop asked the daemon to stop it, which already escalates to a
        // kill after its own grace period. This waits that out and only then
        // treats it as stuck.
        let outcome = match tokio::time::timeout(STOP_GRACE * 2, self.wait_exit()).await {
            Ok(Ok(exit)) => StopOutcome::Exited(exit),
            Ok(Err(e)) => {
                event!(
                    self.bus,
                    "arig: could not tell how the container for '{}' ended: {e}",
                    self.name
                );
                StopOutcome::Killed
            }
            Err(_) => {
                event!(
                    self.bus,
                    "arig: container for '{}' did not stop in time, killing",
                    self.name
                );
                self.kill().await;
                StopOutcome::Killed
            }
        };

        remove_container(&self.docker, &self.container, &self.bus, &self.name).await;
        outcome
    }

    async fn kill(&mut self) {
        if let Err(e) = self
            .docker
            .kill_container(&self.container, None::<KillContainerOptions>)
            .await
        {
            event!(
                self.bus,
                "arig: could not kill the container for '{}': {e}",
                self.name
            );
        }
        // The kernel treats kill as reaping, so do not return while the
        // container is still on its way down.
        let _ = self.wait_exit().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServiceType;

    fn spec(image: Option<&str>, ports: &[&str]) -> ServiceConfig {
        ServiceConfig {
            runtime: NAME.to_string(),
            command: None,
            image: image.map(str::to_string),
            ports: ports.iter().map(|p| p.to_string()).collect(),
            service_type: ServiceType::Service,
            working_dir: None,
            env: HashMap::new(),
            depends_on: Vec::new(),
            ready: None,
            timeout: None,
            shutdown: None,
        }
    }

    fn runtime() -> DockerRuntime {
        DockerRuntime::new(Bus::new(1))
    }

    #[test]
    fn a_service_with_no_image_is_rejected() {
        let err = runtime()
            .validate("db", &spec(None, &[]))
            .err()
            .expect("a docker service must have an image");
        assert!(err.to_string().contains("db"), "got: {err}");
    }

    #[test]
    fn a_service_with_an_image_and_ports_is_accepted() {
        runtime()
            .validate("db", &spec(Some("postgres:16"), &["5432:5432"]))
            .expect("validate");
    }

    #[test]
    fn a_bad_port_fails_before_anything_starts() {
        assert!(
            runtime()
                .validate("db", &spec(Some("postgres:16"), &["nope:5432"]))
                .is_err(),
            "'nope' is not a port"
        );
    }

    #[test]
    fn a_service_name_docker_cannot_use_is_rejected() {
        let err = runtime()
            .validate("my service", &spec(Some("postgres:16"), &[]))
            .err()
            .expect("a space cannot appear in a container name");
        assert!(err.to_string().contains("container name"), "got: {err}");
    }

    #[test]
    fn a_shutdown_hook_is_rejected_rather_than_ignored() {
        let mut spec = spec(Some("postgres:16"), &[]);
        spec.shutdown = Some(crate::config::ShutdownConfig {
            command: "docker compose down".to_string(),
            timeout: Duration::from_secs(30),
        });

        let err = runtime()
            .validate("db", &spec)
            .err()
            .expect("a hook the runtime will not run must not be accepted silently");
        assert!(err.to_string().contains("shutdown hook"), "got: {err}");
    }

    #[test]
    fn a_container_is_named_after_its_service() {
        assert_eq!(container_name("db"), "arig-db");
    }

    /// Everything below needs a docker daemon, so it is not part of the default
    /// run: two of the three CI platforms have none. The `docker` job runs them
    /// with `cargo test -- --ignored docker`.
    mod daemon {
        use super::*;
        use tokio::io::AsyncReadExt;

        fn alpine(command: &str) -> ServiceConfig {
            let mut spec = spec(Some("alpine:3"), &[]);
            spec.command = Some(command.to_string());
            spec
        }

        /// Read a stream to end, as the kernel's log pipe does.
        async fn drain(mut stream: OutputStream) -> String {
            let mut buf = String::new();
            stream.read_to_string(&mut buf).await.expect("read output");
            buf
        }

        #[tokio::test]
        #[ignore = "needs a docker daemon"]
        async fn a_container_that_exits_zero_is_a_success() {
            let mut spawned = runtime()
                .spawn("arig-test-ok", &alpine("true"))
                .await
                .expect("spawn");

            let exit = spawned.handle.wait().await.expect("wait");
            assert!(exit.success(), "got: {exit}");
            spawned.handle.finish_stop().await;
        }

        #[tokio::test]
        #[ignore = "needs a docker daemon"]
        async fn a_container_that_exits_nonzero_is_a_failure_not_an_error() {
            let mut spawned = runtime()
                .spawn("arig-test-fail", &alpine("false"))
                .await
                .expect("spawn");

            // bollard reports a non-zero exit as a transport error; the runtime
            // has to turn it back into an ordinary failed exit or a oneshot
            // that legitimately fails looks like a broken daemon call.
            let exit = spawned
                .handle
                .wait()
                .await
                .expect("a non-zero exit is not an error");
            assert!(!exit.success(), "got: {exit}");
            spawned.handle.finish_stop().await;
        }

        #[tokio::test]
        #[ignore = "needs a docker daemon"]
        async fn stderr_arrives_on_its_own_stream() {
            // `ls` on a missing path writes to stderr and nothing to stdout,
            // which needs no shell quoting to arrange.
            let mut spawned = runtime()
                .spawn("arig-test-stderr", &alpine("ls /no-such-path"))
                .await
                .expect("spawn");

            let stdout = spawned.stdout.take().expect("stdout stream");
            let stderr = spawned.stderr.take().expect("stderr stream");
            let exit = spawned.handle.wait().await.expect("wait");
            assert!(!exit.success(), "got: {exit}");

            let (out, err) = tokio::join!(drain(stdout), drain(stderr));
            assert!(err.contains("/no-such-path"), "stderr was {err:?}");
            assert!(out.is_empty(), "stdout should be empty, was {out:?}");
            spawned.handle.finish_stop().await;
        }

        #[tokio::test]
        #[ignore = "needs a docker daemon"]
        async fn output_reaches_the_kernel_as_stdout() {
            let mut spawned = runtime()
                .spawn("arig-test-output", &alpine("echo hello-from-arig"))
                .await
                .expect("spawn");

            let stdout = spawned.stdout.take().expect("stdout stream");
            let exit = spawned.handle.wait().await.expect("wait");
            assert!(exit.success(), "got: {exit}");

            let output = drain(stdout).await;
            assert!(
                output.contains("hello-from-arig"),
                "got: {output:?}, expected the container's stdout"
            );
            spawned.handle.finish_stop().await;
        }

        #[tokio::test]
        #[ignore = "needs a docker daemon"]
        async fn a_leftover_container_is_replaced_rather_than_colliding() {
            let name = "arig-test-leftover";
            let mut first = runtime()
                .spawn(name, &alpine("true"))
                .await
                .expect("first spawn");
            let _ = first.handle.wait().await;
            // Deliberately not stopped: the container stays behind holding the
            // name, the way it would after a crashed run.

            let mut second = runtime()
                .spawn(name, &alpine("true"))
                .await
                .expect("a leftover container must not block the next run");
            let exit = second.handle.wait().await.expect("wait");
            assert!(exit.success(), "got: {exit}");
            second.handle.finish_stop().await;
        }

        #[tokio::test]
        #[ignore = "needs a docker daemon"]
        async fn a_long_running_container_is_stopped() {
            let mut spawned = runtime()
                .spawn("arig-test-stop", &alpine("sleep 600"))
                .await
                .expect("spawn");

            spawned.handle.begin_stop();
            match spawned.handle.finish_stop().await {
                StopOutcome::Exited(_) | StopOutcome::Killed => {}
            }
        }
    }
}
