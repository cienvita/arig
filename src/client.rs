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
    lifecycle(workspace, req, timeout, "start", &started(service)).await
}

pub async fn restart(
    workspace: &Path,
    service: &str,
    build: bool,
    no_wait: bool,
    timeout: Duration,
) -> Result<()> {
    let req = Request::Restart {
        service: service.to_string(),
        build,
        no_wait,
    };
    lifecycle(
        workspace,
        req,
        timeout,
        "restart",
        &format!("'{service}' restarted"),
    )
    .await
}

pub async fn build(workspace: &Path, service: &str, timeout: Duration) -> Result<()> {
    let req = Request::Build {
        service: service.to_string(),
    };
    lifecycle(
        workspace,
        req,
        timeout,
        "build",
        &format!("'{service}' built"),
    )
    .await
}

/// What a start got as far as. The supervisor reports success either way and
/// does not say whether a probe was involved, so the client cannot claim the
/// service is ready: a service with no `ready:` block never was checked. What
/// the exit code means is in the README.
fn started(service: &str) -> String {
    format!("'{service}' started")
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
    for line in render_ps(services) {
        println!("{line}");
    }
}

/// The `ps` table, header first. Every column is as wide as the widest thing
/// in it, header included, since a value wider than its header would push
/// everything after it out of line.
fn render_ps(services: &[ServiceSnapshot]) -> Vec<String> {
    let width = |header: &str, values: &mut dyn Iterator<Item = usize>| {
        values.max().unwrap_or(0).max(header.len())
    };
    let name_w = width("NAME", &mut services.iter().map(|s| s.name.len()));
    let kind_w = width("KIND", &mut services.iter().map(|s| s.kind.len()));
    let status_w = width("STATUS", &mut services.iter().map(|s| s.status.len()));
    let ready_w = width(
        "READY",
        &mut services.iter().map(|s| s.ready.as_str().len()),
    );

    // A supervisor from before these existed reports none of them, and its
    // table is printed the way it always was rather than with empty columns.
    let uptimes: Vec<String> = services.iter().map(uptime).collect();
    let notes: Vec<String> = services.iter().map(|s| note(s, services)).collect();
    let show_desired = services.iter().any(|s| s.desired.is_some());
    // Only worth a column once something has actually been restarted.
    let show_restarts = services.iter().any(|s| s.restarts > 0);
    let show_uptime = services.iter().any(|s| s.uptime_secs.is_some());
    let show_note = notes.iter().any(|n| !n.is_empty());
    let desired_w = width(
        "DESIRED",
        &mut services
            .iter()
            .map(|s| s.desired.as_deref().unwrap_or("-").len()),
    );
    let restarts_w = width(
        "RESTARTS",
        &mut services.iter().map(|s| s.restarts.to_string().len()),
    );
    let uptime_w = width("UPTIME", &mut uptimes.iter().map(String::len));

    let mut header = format!(
        "{:<name_w$}  {:>4}  {:>7}  {:<kind_w$}  {:<status_w$}  {:<ready_w$}",
        "NAME", "WAVE", "PID", "KIND", "STATUS", "READY",
    );
    if show_desired {
        header.push_str(&format!("  {:<desired_w$}", "DESIRED"));
    }
    if show_restarts {
        header.push_str(&format!("  {:>restarts_w$}", "RESTARTS"));
    }
    if show_uptime {
        header.push_str(&format!("  {:>uptime_w$}", "UPTIME"));
    }
    if show_note {
        header.push_str("  NOTE");
    }

    let mut table = vec![header.trim_end().to_string()];
    for (i, s) in services.iter().enumerate() {
        let pid = match s.pid {
            Some(pid) => pid.to_string(),
            None => "-".to_string(),
        };
        let mut row = format!(
            "{:<name_w$}  {:>4}  {:>7}  {:<kind_w$}  {:<status_w$}  {:<ready_w$}",
            s.name,
            s.wave,
            pid,
            s.kind,
            s.status,
            s.ready.as_str(),
        );
        if show_desired {
            row.push_str(&format!(
                "  {:<desired_w$}",
                s.desired.as_deref().unwrap_or("-")
            ));
        }
        if show_restarts {
            row.push_str(&format!("  {:>restarts_w$}", s.restarts));
        }
        if show_uptime {
            row.push_str(&format!("  {:>uptime_w$}", uptimes[i]));
        }
        if show_note {
            row.push_str(&format!("  {}", notes[i]));
        }
        table.push(row.trim_end().to_string());
    }
    table
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

    /// `pending` is wider than the `READY` heading, and every column after it
    /// used to be pushed out of line on the row that carried it.
    #[test]
    fn a_value_wider_than_its_heading_keeps_the_table_aligned() {
        let mut waiting = row("api", "running", &[]);
        waiting.ready = Readiness::Pending;
        waiting.restarts = 3;
        waiting.uptime_secs = Some(90);
        let rows = [row("db", "running", &[]), waiting];

        assert_eq!(
            render_ps(&rows).join("\n"),
            concat!(
                "NAME  WAVE      PID  KIND     STATUS   READY    DESIRED  RESTARTS  UPTIME\n",
                "db       0        1  service  running  -        up              0       -\n",
                "api      0        1  service  running  pending  up              3  1m 30s",
            )
        );
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
