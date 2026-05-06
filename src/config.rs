use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct ArigConfig {
    pub services: HashMap<String, ServiceConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    #[default]
    Service,
    Oneshot,
}

#[derive(Debug, Deserialize)]
pub struct ServiceConfig {
    pub command: String,
    #[serde(rename = "type", default)]
    pub service_type: ServiceType,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub ready: Option<ReadyProbe>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadyProbe {
    /// TCP host:port to connect to. Probe passes when connect() succeeds.
    pub tcp: Option<String>,
    /// Total time to keep retrying before giving up. e.g. "30s", "1m 30s".
    #[serde(default = "default_probe_timeout", with = "humantime_serde")]
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
