//! Named implementations of the extension traits. Built-ins register here at
//! startup the same way anything added later will.

use crate::event::Bus;
use crate::runtime::Runtime;
use crate::runtime::process::ProcessRuntime;
use std::collections::HashMap;
use std::sync::Arc;

/// The runtime a service runs on when it does not ask for another. Nothing in
/// the config selects a runtime yet, so this is every service.
pub const DEFAULT_RUNTIME: &str = crate::runtime::process::NAME;

#[derive(Default)]
pub struct Registry {
    runtimes: HashMap<&'static str, Arc<dyn Runtime>>,
}

impl Registry {
    pub fn with_builtins(bus: &Bus) -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(ProcessRuntime::new(bus.clone())));
        registry
    }

    pub fn register(&mut self, runtime: Arc<dyn Runtime>) {
        self.runtimes.insert(runtime.name(), runtime);
    }

    pub fn runtime(&self, name: &str) -> anyhow::Result<&Arc<dyn Runtime>> {
        self.runtimes
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown runtime '{name}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_include_the_process_runtime() {
        let registry = Registry::with_builtins(&Bus::new(1));
        assert!(registry.runtime(DEFAULT_RUNTIME).is_ok());
    }

    #[test]
    fn an_unregistered_runtime_names_itself_in_the_error() {
        let err = Registry::default()
            .runtime("docker")
            .err()
            .expect("an unregistered name must not resolve");
        assert!(err.to_string().contains("docker"), "got: {err}");
    }
}
