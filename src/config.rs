use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

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
}

impl ArigConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: ArigConfig = serde_yaml::from_str(&contents)?;
        Ok(config)
    }
}
