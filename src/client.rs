use crate::ipc;
use crate::protocol::{self, Request, ServiceSnapshot};
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
    let log = supervisor_log(workspace);
    let resp = match tokio::time::timeout(timeout, protocol::exchange(stream, &Request::Wait)).await
    {
        // A supervisor that dies outright answers nothing, so the reason for
        // it is only in its log.
        Ok(resp) => resp.with_context(|| format!("check {} for details", log.display()))?,
        Err(_) => anyhow::bail!(
            "services were not ready within {}",
            humantime::format_duration(timeout)
        ),
    };
    if !resp.ok {
        anyhow::bail!(
            "wait: {}. check {} for details",
            resp.error.unwrap_or_else(|| "unknown".into()),
            log.display(),
        );
    }
    println!("arig: ready");
    Ok(())
}

pub async fn stop(workspace: &Path, service: &str, timeout: Duration) -> Result<()> {
    let req = Request::Stop {
        service: service.to_string(),
    };
    lifecycle(
        workspace,
        req,
        timeout,
        "stop",
        &format!("'{service}' stopped"),
    )
    .await
}

pub async fn start(
    workspace: &Path,
    service: &str,
    no_wait: bool,
    timeout: Duration,
) -> Result<()> {
    let req = Request::Start {
        service: service.to_string(),
        no_wait,
    };
    lifecycle(workspace, req, timeout, "start", &started(service, no_wait)).await
}

pub async fn restart(
    workspace: &Path,
    service: &str,
    no_wait: bool,
    timeout: Duration,
) -> Result<()> {
    let req = Request::Restart {
        service: service.to_string(),
        no_wait,
    };
    lifecycle(
        workspace,
        req,
        timeout,
        "restart",
        &started(service, no_wait),
    )
    .await
}

/// What a start got as far as. Without a probe there is nothing to be ready
/// for, so the claim is only that it started.
fn started(service: &str, no_wait: bool) -> String {
    if no_wait {
        format!("'{service}' started")
    } else {
        format!("'{service}' started and ready")
    }
}

/// Send a lifecycle command and report what the supervisor made of it. The
/// supervisor holds the connection until the command settles, so the timeout
/// covers the whole operation rather than the round trip.
async fn lifecycle(
    workspace: &Path,
    req: Request,
    timeout: Duration,
    verb: &str,
    done: &str,
) -> Result<()> {
    let endpoint = ipc::Endpoint::for_workspace(workspace)?;
    let stream = ipc::connect(&endpoint)
        .await
        .with_context(|| format!("no supervisor at {}", endpoint.address))?;

    let resp = match tokio::time::timeout(timeout, protocol::exchange(stream, &req)).await {
        Ok(resp) => resp.with_context(|| format!("exchange {verb}"))?,
        Err(_) => anyhow::bail!(
            "{verb} did not finish within {}",
            humantime::format_duration(timeout)
        ),
    };
    if !resp.ok {
        anyhow::bail!("{verb}: {}", explain(resp.error, verb));
    }
    println!("arig: {done}");
    Ok(())
}

/// A supervisor too old for a command rejects it as a malformed request,
/// which reads like a bug rather than the version skew it is.
fn explain(error: Option<String>, verb: &str) -> String {
    let reason = error.unwrap_or_else(|| "unknown".into());
    if reason.contains("parse request") {
        return format!(
            "this supervisor predates `arig {verb}`; bounce the stack to pick up the new binary"
        );
    }
    reason
}

/// Where a detached supervisor's own output goes. Fixed rather than read from
/// the config, since it holds whatever a supervisor printed before its
/// configured logging was up.
fn supervisor_log(workspace: &Path) -> std::path::PathBuf {
    workspace.join(".arig/var/supervisor.log")
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

fn print_ps(services: &[ServiceSnapshot]) {
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

    // A supervisor from before these existed reports none of them, and its
    // table is printed the way it always was rather than with empty columns.
    let uptimes: Vec<String> = services.iter().map(uptime).collect();
    let notes: Vec<String> = services.iter().map(|s| note(s, services)).collect();
    let show_desired = services.iter().any(|s| s.desired.is_some());
    let show_uptime = services.iter().any(|s| s.uptime_secs.is_some());
    let show_note = notes.iter().any(|n| !n.is_empty());
    let uptime_w = uptimes.iter().map(String::len).max().unwrap_or(6).max(6);

    let mut header = format!(
        "{:<name_w$}  {:>4}  {:>7}  {:<kind_w$}  {:<status_w$}  {:<5}",
        "NAME", "WAVE", "PID", "KIND", "STATUS", "READY",
    );
    if show_desired {
        header.push_str("  DESIRED");
    }
    if show_uptime {
        header.push_str(&format!("  {:>uptime_w$}", "UPTIME"));
    }
    if show_note {
        header.push_str("  NOTE");
    }
    println!("{}", header.trim_end());

    for (i, s) in services.iter().enumerate() {
        let pid = match s.pid {
            Some(pid) => pid.to_string(),
            None => "-".to_string(),
        };
        let mut row = format!(
            "{:<name_w$}  {:>4}  {:>7}  {:<kind_w$}  {:<status_w$}  {:<5}",
            s.name,
            s.wave,
            pid,
            s.kind,
            s.status,
            s.ready.as_str(),
        );
        if show_desired {
            row.push_str(&format!("  {:<7}", s.desired.as_deref().unwrap_or("-")));
        }
        if show_uptime {
            row.push_str(&format!("  {:>uptime_w$}", uptimes[i]));
        }
        if show_note {
            row.push_str(&format!("  {}", notes[i]));
        }
        println!("{}", row.trim_end());
    }
}

fn uptime(service: &ServiceSnapshot) -> String {
    match service.uptime_secs {
        Some(secs) => humantime::format_duration(Duration::from_secs(secs)).to_string(),
        None => "-".to_string(),
    }
}

/// What is off about a service's dependencies. Stopping one deliberately
/// leaves its dependents running, so the row is the only place that shows the
/// connection. A dependency with no row at all is a oneshot that completed,
/// which is the ordinary case rather than something to report.
fn note(service: &ServiceSnapshot, all: &[ServiceSnapshot]) -> String {
    service
        .depends_on
        .iter()
        .filter_map(|dep| all.iter().find(|s| &s.name == dep))
        .filter(|dep| dep.status != protocol::RUNNING)
        .map(|dep| format!("dep '{}' {}", dep.name, dep.status))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Readiness;

    fn row(name: &str, status: &str, depends_on: &[&str]) -> ServiceSnapshot {
        ServiceSnapshot {
            name: name.to_string(),
            kind: "service".to_string(),
            wave: 0,
            pid: Some(1),
            status: status.to_string(),
            ready: Readiness::Unchecked,
            desired: Some("up".to_string()),
            restarts: 0,
            uptime_secs: None,
            depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn a_stopped_dependency_is_reported_on_its_dependents_row() {
        let rows = [row("db", "stopped", &[]), row("api", "running", &["db"])];

        assert_eq!(note(&rows[1], &rows), "dep 'db' stopped");
    }

    #[test]
    fn an_untouched_stack_has_nothing_to_note() {
        let rows = [row("db", "running", &[]), row("api", "running", &["db"])];

        assert!(rows.iter().all(|r| note(r, &rows).is_empty()));
    }

    /// A completed oneshot has no row at all, and its dependents are running
    /// exactly as intended.
    #[test]
    fn a_dependency_that_left_no_row_is_not_a_note() {
        let rows = [row("api", "running", &["migrate"])];

        assert!(note(&rows[0], &rows).is_empty());
    }
}
