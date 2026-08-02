//! Named implementations of the extension traits. Built-ins register here at
//! startup the same way anything added later will.

use crate::config::ReadyProbe;
use crate::event::Bus;
use crate::probe::tcp::TcpProbe;
use crate::probe::{Probe, ReadyCheck};
use crate::runtime::Runtime;
use crate::runtime::process::ProcessRuntime;
use std::collections::HashMap;
use std::sync::Arc;

/// The runtime a service runs on when it does not ask for another. Nothing in
/// the config selects a runtime yet, so this is every service.
pub const DEFAULT_RUNTIME: &str = crate::runtime::process::NAME;

/// A readiness check bound to a service, with the kind of probe that produced
/// it so the kernel can name it while it waits.
pub struct BoundProbe {
    pub kind: &'static str,
    pub check: Box<dyn ReadyCheck>,
}

#[derive(Default)]
pub struct Registry {
    runtimes: HashMap<&'static str, Arc<dyn Runtime>>,
    probes: HashMap<&'static str, Arc<dyn Probe>>,
}

impl Registry {
    pub fn with_builtins(bus: &Bus) -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(ProcessRuntime::new(bus.clone())));
        registry.register_probe(Arc::new(TcpProbe));
        registry
    }

    pub fn register(&mut self, runtime: Arc<dyn Runtime>) {
        self.runtimes.insert(runtime.name(), runtime);
    }

    pub fn register_probe(&mut self, probe: Arc<dyn Probe>) {
        self.probes.insert(probe.name(), probe);
    }

    pub fn runtime(&self, name: &str) -> anyhow::Result<&Arc<dyn Runtime>> {
        self.runtimes
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown runtime '{name}'"))
    }

    /// Resolve the check a `ready:` block asks for. A block that names no
    /// probe is not an error: the service counts as ready once it has started.
    pub fn ready_check(&self, spec: &ReadyProbe) -> anyhow::Result<Option<BoundProbe>> {
        let mut claimed: Vec<&Arc<dyn Probe>> =
            self.probes.values().filter(|p| p.claims(spec)).collect();

        if claimed.len() > 1 {
            let mut names: Vec<&str> = claimed.iter().map(|p| p.name()).collect();
            names.sort_unstable();
            anyhow::bail!(
                "ready block asks for more than one probe ({}); pick one",
                names.join(", ")
            );
        }

        let Some(probe) = claimed.pop() else {
            return Ok(None);
        };
        Ok(Some(BoundProbe {
            kind: probe.name(),
            check: probe.prepare(spec)?,
        }))
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

    /// Claims every block, so it collides with the tcp probe.
    struct GreedyProbe;

    impl Probe for GreedyProbe {
        fn name(&self) -> &'static str {
            "greedy"
        }

        fn claims(&self, _spec: &ReadyProbe) -> bool {
            true
        }

        fn prepare(&self, _spec: &ReadyProbe) -> anyhow::Result<Box<dyn ReadyCheck>> {
            unreachable!("an ambiguous block is rejected before anything is prepared")
        }
    }

    fn spec(tcp: Option<&str>) -> ReadyProbe {
        ReadyProbe {
            tcp: tcp.map(|s| s.to_string()),
            timeout: std::time::Duration::from_secs(60),
        }
    }

    #[test]
    fn a_tcp_block_resolves_to_the_tcp_probe() {
        let bound = Registry::with_builtins(&Bus::new(1))
            .ready_check(&spec(Some("127.0.0.1:5432")))
            .expect("resolve")
            .expect("a tcp address selects a probe");

        assert_eq!(bound.kind, crate::probe::tcp::NAME);
        assert_eq!(bound.check.target(), "127.0.0.1:5432");
    }

    #[test]
    fn a_block_that_names_no_probe_resolves_to_nothing() {
        let bound = Registry::with_builtins(&Bus::new(1))
            .ready_check(&spec(None))
            .expect("an empty ready block is allowed");

        assert!(bound.is_none());
    }

    #[test]
    fn a_block_two_probes_claim_is_rejected() {
        let mut registry = Registry::with_builtins(&Bus::new(1));
        registry.register_probe(Arc::new(GreedyProbe));

        let err = registry
            .ready_check(&spec(Some("127.0.0.1:5432")))
            .err()
            .expect("an ambiguous block must not resolve");
        assert!(err.to_string().contains("greedy, tcp"), "got: {err}");
    }
}
