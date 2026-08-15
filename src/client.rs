use crate::ipc;
use crate::protocol::{self, Request};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

const DOWN_TIMEOUT: Duration = Duration::from_secs(30);
const DOWN_POLL: Duration = Duration::from_millis(200);

pub async fn down(workspace: &Path) -> Result<()> {
    let endpoint = ipc::Endpoint::for_workspace(workspace)?;
    let stream = ipc::connect(&endpoint)
        .await
        .with_context(|| format!("no supervisor at {}", endpoint.address))?;

    let resp = protocol::exchange(stream, &Request::Down)
        .await
        .context("exchange down")?;
    if !resp.ok {
        anyhow::bail!("down: {}", resp.error.unwrap_or_else(|| "unknown".into()));
    }

    let deadline = Instant::now() + DOWN_TIMEOUT;
    while ipc::probe(&endpoint).await {
        if Instant::now() >= deadline {
            anyhow::bail!("supervisor did not exit within {DOWN_TIMEOUT:?}");
        }
        tokio::time::sleep(DOWN_POLL).await;
    }
    println!("arig: stopped");
    Ok(())
}

/// Block until the supervisor reports every wave up and every probe passed.
pub async fn wait(workspace: &Path, timeout: Duration) -> Result<()> {
    let endpoint = ipc::Endpoint::for_workspace(workspace)?;
    let stream = ipc::connect(&endpoint)
        .await
        .with_context(|| format!("no supervisor at {}", endpoint.address))?;

    // The supervisor holds the connection until it is up, so the timeout is
    // on the exchange rather than on a poll loop.
    let resp = match tokio::time::timeout(timeout, protocol::exchange(stream, &Request::Wait)).await
    {
        Ok(resp) => resp.context("exchange wait")?,
        Err(_) => anyhow::bail!(
            "services were not ready within {}",
            humantime::format_duration(timeout)
        ),
    };
    if !resp.ok {
        anyhow::bail!("wait: {}", resp.error.unwrap_or_else(|| "unknown".into()));
    }
    println!("arig: ready");
    Ok(())
}

pub async fn ps(workspace: &Path) -> Result<()> {
    let endpoint = ipc::Endpoint::for_workspace(workspace)?;
    let stream = ipc::connect(&endpoint)
        .await
        .with_context(|| format!("no supervisor at {}", endpoint.address))?;

    let resp = protocol::exchange(stream, &Request::Ps)
        .await
        .context("exchange ps")?;
    if !resp.ok {
        anyhow::bail!("ps: {}", resp.error.unwrap_or_else(|| "unknown".into()));
    }
    let services = resp.services.unwrap_or_default();
    print_ps(&services);
    Ok(())
}

fn print_ps(services: &[protocol::ServiceSnapshot]) {
    let name_w = services
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let kind_w = services
        .iter()
        .map(|s| s.kind.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let status_w = services
        .iter()
        .map(|s| s.status.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!(
        "{:<name_w$}  {:>4}  {:>7}  {:<kind_w$}  {:<status_w$}  READY",
        "NAME", "WAVE", "PID", "KIND", "STATUS",
    );
    for s in services {
        let pid = match s.pid {
            Some(pid) => pid.to_string(),
            None => "-".to_string(),
        };
        println!(
            "{:<name_w$}  {:>4}  {:>7}  {:<kind_w$}  {:<status_w$}  {}",
            s.name,
            s.wave,
            pid,
            s.kind,
            s.status,
            s.ready.as_str(),
        );
    }
}
