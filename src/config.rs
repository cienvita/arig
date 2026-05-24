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
    /// Command line to execute. Run via the system shell.
    pub command: String,
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

impl ArigConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: ArigConfig = serde_yaml::from_str(&contents)?;
        Ok(config)
    }
}
