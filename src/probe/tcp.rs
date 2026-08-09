//! The built-in probe: a service is ready once something accepts a TCP
//! connection on its address.

use super::{Probe, ReadyCheck};
use crate::config::ReadyProbe;
use async_trait::async_trait;
use std::time::Duration;
use tokio::net::TcpStream;

/// The name this probe registers under.
pub const NAME: &str = "tcp";

/// How long one connect gets before it counts as a failed attempt. Shorter
/// than the kernel's retry interval, so a host that blackholes packets still
/// gets polled at roughly the same rate as one that refuses outright.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

pub struct TcpProbe;

impl Probe for TcpProbe {
    fn name(&self) -> &'static str {
        NAME
    }

    fn claims(&self, spec: &ReadyProbe) -> bool {
        spec.tcp.is_some()
    }

    fn prepare(&self, spec: &ReadyProbe) -> anyhow::Result<Box<dyn ReadyCheck>> {
        let addr = spec
            .tcp
            .clone()
            .ok_or_else(|| anyhow::anyhow!("ready block has no tcp address"))?;
        Ok(Box::new(TcpCheck { addr }))
    }
}

struct TcpCheck {
    addr: String,
}

#[async_trait]
impl ReadyCheck for TcpCheck {
    fn target(&self) -> &str {
        &self.addr
    }

    async fn check(&self) -> Result<(), String> {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.addr)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("connect timed out".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn spec(tcp: Option<&str>) -> ReadyProbe {
        ReadyProbe {
            tcp: tcp.map(|s| s.to_string()),
            timeout: Duration::from_secs(60),
        }
    }

    fn check(addr: &str) -> Box<dyn ReadyCheck> {
        TcpProbe.prepare(&spec(Some(addr))).expect("prepare")
    }

    #[test]
    fn a_ready_block_without_a_tcp_address_is_not_ours() {
        assert!(!TcpProbe.claims(&spec(None)));
        assert!(TcpProbe.claims(&spec(Some("127.0.0.1:1"))));
    }

    #[tokio::test]
    async fn a_listening_port_is_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr").to_string();

        assert_eq!(check(&addr).check().await, Ok(()));
    }

    #[tokio::test]
    async fn a_closed_port_reports_why_it_failed() {
        // Bind then drop, so the port is one nothing is listening on rather
        // than one that might belong to something else on the machine.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr").to_string();
        drop(listener);

        let err = check(&addr)
            .check()
            .await
            .expect_err("nothing is listening on a closed port");
        assert!(!err.is_empty(), "the kernel reports this verbatim");
    }
}
