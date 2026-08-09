use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Top-level arig config. Lives at `arig.yaml` by default.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArigConfig {
    /// Directories arig writes to. Defaults under `.arig/var/`.
    #[serde(default)]
    pub dirs: DirsConfig,
    /// Services to supervise, keyed by service name.
    pub services: HashMap<String, ServiceConfig>,
}

/// Filesystem locations arig manages.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DirsConfig {
    /// Base directory for per-session log folders.
    #[serde(default = "default_logs_dir")]
    pub logs: PathBuf,
    /// Scratch directory for temporary files.
    #[serde(default = "default_tmp_dir")]
    #[allow(dead_code)]
    pub tmp: PathBuf,
}

impl Default for DirsConfig {
    fn default() -> Self {
        Self {
            logs: default_logs_dir(),
            tmp: default_tmp_dir(),
        }
    }
}

fn default_logs_dir() -> PathBuf {
    PathBuf::from(".arig/var/logs")
}

fn default_tmp_dir() -> PathBuf {
    PathBuf::from(".arig/var/tmp")
}

/// How arig should treat a service for shutdown and exit semantics.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    /// Long-running process; arig keeps it alive and stops it on shutdown.
    #[default]
    Service,
    /// Runs to completion once; arig waits for exit before dependents start.
    Oneshot,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServiceConfig {
    /// Which runtime runs this service. `process` runs it via the system
    /// shell; `docker` runs it as a container.
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Command line to execute. Required by the `process` runtime. On
    /// `docker` it overrides the image's command, and may be omitted.
    pub command: Option<String>,
    /// Container image. Required by the `docker` runtime, ignored otherwise.
    pub image: Option<String>,
    /// Ports to publish, as "host:container" or a bare port for both.
    /// `docker` only.
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(rename = "type", default)]
    pub service_type: ServiceType,
    /// Working directory for the command. Relative to the config file's directory.
    pub working_dir: Option<String>,
    /// Extra environment variables to set for the process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Other service names that must be ready before this one starts.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Optional readiness probe. Dependents wait until this passes.
    pub ready: Option<ReadyProbe>,
    /// Maximum time a oneshot may run before it's killed and the wave fails.
    /// Ignored for long-running services. e.g. "5m", "30s". No default: opt-in.
    #[serde(default, with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub timeout: Option<Duration>,
    /// Optional shutdown hook. When present, this command is run instead of
    /// signalling the process directly. Useful for thin CLI wrappers that
    /// delegate to a daemon (e.g. `docker compose up`).
    pub shutdown: Option<ShutdownConfig>,
}

/// Shutdown hook configuration for a service.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShutdownConfig {
    /// Command to run to stop the service.
    pub command: String,
    /// How long to wait for the main process to exit after the command runs.
    /// Falls back to signal/kill if the process is still alive. e.g. "30s", "2m".
    #[serde(default = "default_shutdown_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
}

fn default_shutdown_timeout() -> Duration {
    Duration::from_secs(30)
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadyProbe {
    /// TCP host:port to connect to. Probe passes when connect() succeeds.
    pub tcp: Option<String>,
    /// Total time to keep retrying before giving up. e.g. "30s", "1m 30s".
    #[serde(default = "default_probe_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
}

fn default_probe_timeout() -> Duration {
    Duration::from_secs(60)
}

fn default_runtime() -> String {
    crate::registry::DEFAULT_RUNTIME.to_string()
}

/// A published port, as the docker runtime needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortMapping {
    pub host: u16,
    pub container: u16,
}

impl PortMapping {
    /// Parse a `ports:` entry: "8080:80", or "80" for the same port on both
    /// sides. Rejected here rather than at spawn time so a typo fails before
    /// anything starts.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let (host, container) = match spec.split_once(':') {
            Some((h, c)) => (h, c),
            None => (spec, spec),
        };
        let port = |s: &str, side| {
            s.trim()
                .parse::<u16>()
                .map_err(|e| anyhow::anyhow!("{side} port '{s}' in '{spec}' is not a port: {e}"))
        };
        Ok(Self {
            host: port(host, "host")?,
            container: port(container, "container")?,
        })
    }
}

impl ArigConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let mut config: ArigConfig = serde_yaml::from_str(&contents)?;
        config.rebase_working_dirs(path.parent().unwrap_or(Path::new("")));
        Ok(config)
    }

    /// `working_dir` is documented as relative to the config file's directory,
    /// which is not necessarily where the supervisor runs: `--file` can name a
    /// config in another directory. Rebase once here so every consumer, the
    /// spawn path and the shutdown hook alike, sees a resolved value.
    fn rebase_working_dirs(&mut self, base: &Path) {
        // An empty base means the config sits in the working directory, so a
        // relative working_dir already resolves correctly.
        if base.as_os_str().is_empty() {
            return;
        }
        for service in self.services.values_mut() {
            let Some(dir) = &service.working_dir else {
                continue;
            };
            let dir = Path::new(dir);
            if dir.is_relative() {
                // Collecting the components drops the interior `.` a joined
                // `./inner` would otherwise leave in the middle of the path,
                // which only shows up as noise in logs and errors. `..` is
                // left alone; resolving it here would be wrong across symlinks.
                let joined: PathBuf = base.join(dir).components().collect();
                service.working_dir = Some(joined.to_string_lossy().into_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_that_names_no_runtime_gets_the_process_one() {
        let config: ArigConfig =
            serde_yaml::from_str("services:\n  a:\n    command: echo hi\n").expect("parse");

        assert_eq!(config.services["a"].runtime, "process");
        assert_eq!(config.services["a"].command.as_deref(), Some("echo hi"));
    }

    #[test]
    fn a_docker_service_needs_no_command() {
        let config: ArigConfig = serde_yaml::from_str(
            "services:\n  db:\n    runtime: docker\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
        )
        .expect("parse");

        let db = &config.services["db"];
        assert_eq!(db.runtime, "docker");
        assert_eq!(db.image.as_deref(), Some("postgres:16"));
        assert!(db.command.is_none());
        assert_eq!(db.ports, ["5432:5432"]);
    }

    #[test]
    fn working_dir_resolves_against_the_config_directory() {
        // Regression: `--file` naming a config elsewhere used to leave
        // working_dir resolving against the invocation directory instead.
        let dir = std::env::temp_dir().join(format!("arig-test-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("arig.yaml");
        std::fs::write(
            &path,
            "services:\n  a:\n    command: pwd\n    working_dir: ./inner\n",
        )
        .expect("write config");

        let config = ArigConfig::load(&path).expect("load");

        assert_eq!(
            config.services["a"].working_dir.as_deref(),
            Some(dir.join("inner").to_string_lossy().as_ref()),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absolute_working_dir_is_left_alone() {
        let absolute = std::env::temp_dir().join("elsewhere");
        let mut config: ArigConfig = serde_yaml::from_str(&format!(
            "services:\n  a:\n    command: pwd\n    working_dir: {}\n",
            absolute.display()
        ))
        .expect("parse");

        config.rebase_working_dirs(Path::new("/some/config/dir"));

        assert_eq!(
            config.services["a"].working_dir.as_deref(),
            Some(absolute.to_string_lossy().as_ref()),
        );
    }

    #[test]
    fn a_config_in_the_working_directory_leaves_working_dir_alone() {
        let mut config: ArigConfig =
            serde_yaml::from_str("services:\n  a:\n    command: pwd\n    working_dir: ./inner\n")
                .expect("parse");

        config.rebase_working_dirs(Path::new(""));

        assert_eq!(config.services["a"].working_dir.as_deref(), Some("./inner"));
    }

    #[test]
    fn a_port_maps_both_sides() {
        assert_eq!(
            PortMapping::parse("8080:80").expect("parse"),
            PortMapping {
                host: 8080,
                container: 80
            }
        );
    }

    #[test]
    fn a_bare_port_is_the_same_on_both_sides() {
        assert_eq!(
            PortMapping::parse("5432").expect("parse"),
            PortMapping {
                host: 5432,
                container: 5432
            }
        );
    }

    #[test]
    fn a_port_that_is_not_a_number_says_which_side_was_wrong() {
        let err = PortMapping::parse("http:80")
            .err()
            .expect("'http' is not a port number");
        assert!(err.to_string().contains("host"), "got: {err}");
    }

    #[test]
    fn a_port_out_of_range_is_rejected() {
        assert!(
            PortMapping::parse("70000:80").is_err(),
            "70000 does not fit in a port"
        );
    }
}
