use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// One-shot RPC the CLI sends to a workspace's supervisor. Newline-delimited
/// JSON: each request and response is a single line on the wire.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    /// List currently-tracked services.
    Ps,
    /// Block until every wave is up and every readiness probe has passed.
    /// The supervisor holds the connection open until then.
    Wait,
    /// Trigger supervisor shutdown.
    Down,
    /// Stop one service and leave the rest alone. The supervisor holds the
    /// connection until the service is gone.
    Stop { service: String },
    /// Start one service that is not running. Held until its readiness probe
    /// passes unless `no_wait` says otherwise.
    Start {
        service: String,
        /// Defaulted so a flag added later still parses on both ends.
        #[serde(default)]
        no_wait: bool,
    },
    /// Stop and start one service, in one command.
    Restart {
        service: String,
        #[serde(default)]
        no_wait: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<ServiceSnapshot>>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            services: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            services: None,
        }
    }

    pub fn ps(services: Vec<ServiceSnapshot>) -> Self {
        Self {
            ok: true,
            error: None,
            services: Some(services),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSnapshot {
    pub name: String,
    pub kind: String,
    pub wave: usize,
    /// Absent for a runtime whose services are not host processes.
    pub pid: Option<u32>,
    pub status: String,
    /// Defaulted rather than required: a detached supervisor outlives an
    /// upgrade, so a newer `arig ps` has to read a response from an older
    /// supervisor that has no readiness to report. Everything below is
    /// defaulted for the same reason.
    #[serde(default)]
    pub ready: Readiness,
    /// What the operator asked for: "up" or "stopped". Absent from a
    /// supervisor that has no notion of it.
    #[serde(default)]
    pub desired: Option<String>,
    /// How many times this service has been started again since the stack
    /// came up.
    #[serde(default)]
    pub restarts: u64,
    /// How long the current instance has been running. Absent for a service
    /// that is not running, and from a supervisor too old to report it.
    #[serde(default)]
    pub uptime_secs: Option<u64>,
    /// The service's direct dependencies, so `ps` can mark a row whose
    /// dependency is no longer running.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// The `status` a service in ordinary operation reports. Both ends need it:
/// the supervisor writes it, and `ps` marks dependencies that are not it.
pub const RUNNING: &str = "running";

/// Where a service is against its readiness probe. Separate from `status`,
/// which reports the process: a service can be running and not yet ready.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Readiness {
    /// No probe gates this service, so there is nothing to wait for. Also
    /// what a supervisor too old to report readiness degrades to.
    #[default]
    Unchecked,
    Pending,
    Ready,
}

impl Readiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Readiness::Unchecked => "-",
            Readiness::Pending => "pending",
            Readiness::Ready => "ready",
        }
    }
}

pub async fn read_request<R: AsyncRead + Unpin>(reader: R) -> Result<Request> {
    let mut br = BufReader::new(reader);
    let mut line = String::new();
    br.read_line(&mut line).await.context("read request")?;
    serde_json::from_str(line.trim()).context("parse request")
}

pub async fn write_response<W: AsyncWrite + Unpin>(writer: &mut W, resp: &Response) -> Result<()> {
    let body = serde_json::to_string(resp).context("serialize response")?;
    writer.write_all(body.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A detached supervisor outlives an upgrade of the binary, so a newer
    /// `arig ps` has to read a response written by an older supervisor.
    #[test]
    fn a_response_from_before_readiness_still_parses() {
        let older = r#"{"ok":true,"services":[
            {"name":"api","kind":"service","wave":0,"pid":22,"status":"running"}
        ]}"#;

        let resp: Response = serde_json::from_str(older).expect("an older response must parse");
        let services = resp.services.expect("the response carries services");
        assert_eq!(services[0].ready, Readiness::Unchecked);
        assert_eq!(services[0].restarts, 0);
        assert_eq!(services[0].uptime_secs, None);
        assert!(services[0].depends_on.is_empty());
    }

    #[test]
    fn a_lifecycle_request_names_its_service_on_the_wire() {
        let json = serde_json::to_string(&Request::Restart {
            service: "api".to_string(),
            no_wait: true,
        })
        .expect("serialize");

        assert_eq!(json, r#"{"op":"restart","service":"api","no_wait":true}"#);
    }

    /// The flags are defaulted so that a client sending only what it knows
    /// about still parses, whichever end is newer.
    #[test]
    fn a_lifecycle_request_without_flags_parses() {
        let req: Request =
            serde_json::from_str(r#"{"op":"start","service":"api"}"#).expect("parse");

        let Request::Start { service, no_wait } = req else {
            panic!("expected a start request, got {req:?}");
        };
        assert_eq!(service, "api");
        assert!(!no_wait);
    }

    #[test]
    fn readiness_goes_over_the_wire_lowercased() {
        let json = serde_json::to_string(&Readiness::Pending).expect("serialize");
        assert_eq!(json, r#""pending""#);
    }
}

/// Client helper: send a request on the stream and read back the response.
pub async fn exchange<S>(stream: S, req: &Request) -> Result<Response>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (rd, mut wr) = tokio::io::split(stream);
    let body = serde_json::to_string(req).context("serialize request")?;
    wr.write_all(body.as_bytes()).await?;
    wr.write_all(b"\n").await?;
    wr.flush().await?;

    let mut br = BufReader::new(rd);
    let mut line = String::new();
    // A supervisor that exits mid-request closes the stream instead of
    // answering, which reads as EOF rather than as a parse failure.
    if br.read_line(&mut line).await.context("read response")? == 0 {
        anyhow::bail!("supervisor closed the connection without answering");
    }
    serde_json::from_str(line.trim()).context("parse response")
}
