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
    /// Trigger supervisor shutdown.
    Down,
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
    br.read_line(&mut line).await.context("read response")?;
    serde_json::from_str(line.trim()).context("parse response")
}
