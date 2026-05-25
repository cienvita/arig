use anyhow::{Context, Result};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Where a workspace's supervisor listens. Derived deterministically from the
/// canonical workspace path so the CLI can find its own supervisor without a
/// registry.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// What the client passes to connect.
    pub address: String,
    /// Directory the supervisor must create before binding. Unix only.
    #[cfg(unix)]
    pub parent: PathBuf,
    /// Pidfile path. Unix only: AF_UNIX socket files do not auto-clean after a
    /// crash, so liveness is determined by reading this and checking the PID.
    #[cfg(unix)]
    pub pidfile: PathBuf,
}

impl Endpoint {
    pub fn for_workspace(workspace: &Path) -> Result<Self> {
        let abs = workspace
            .canonicalize()
            .with_context(|| format!("canonicalize {}", workspace.display()))?;
        endpoint_from_canonical(&abs)
    }
}

#[cfg(unix)]
fn endpoint_from_canonical(abs: &Path) -> Result<Endpoint> {
    let parent = abs.join(".arig/var/run");
    Ok(Endpoint {
        address: parent.join("arig.sock").to_string_lossy().into_owned(),
        pidfile: parent.join("arig.pid"),
        parent,
    })
}

#[cfg(windows)]
fn endpoint_from_canonical(abs: &Path) -> Result<Endpoint> {
    // Pipe names are a global namespace, so we mix the canonical workspace path
    // into the name. FNV-1a is deterministic across rustc versions (unlike the
    // std DefaultHasher), so upgrades don't orphan existing supervisors. Path
    // comparison on Windows is case-insensitive at the FS level, so we
    // lowercase first to keep two clients of the same dir in agreement.
    let key = abs.as_os_str().to_string_lossy().to_lowercase();
    let h = fnv1a_64(key.as_bytes());
    Ok(Endpoint {
        address: format!(r"\\.\pipe\arig-{h:016x}"),
    })
}

#[cfg(windows)]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(unix)]
pub type Listener = tokio::net::UnixListener;
#[cfg(windows)]
pub type Listener = tokio::net::windows::named_pipe::NamedPipeServer;

/// Server-side stream returned by Acceptor::accept. Duplex AsyncRead+AsyncWrite.
#[cfg(unix)]
pub type ServerStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type ServerStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// Client-side stream returned by connect. Duplex AsyncRead+AsyncWrite.
#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;
#[cfg(windows)]
pub type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
pub fn bind(endpoint: &Endpoint) -> Result<Listener> {
    std::fs::create_dir_all(&endpoint.parent)
        .with_context(|| format!("create {}", endpoint.parent.display()))?;
    cleanup_stale(endpoint)?;
    let listener =
        Listener::bind(&endpoint.address).with_context(|| format!("bind {}", endpoint.address))?;
    std::fs::write(&endpoint.pidfile, std::process::id().to_string())
        .with_context(|| format!("write {}", endpoint.pidfile.display()))?;
    Ok(listener)
}

#[cfg(windows)]
pub fn bind(endpoint: &Endpoint) -> Result<Listener> {
    use tokio::net::windows::named_pipe::ServerOptions;
    // first_pipe_instance(true) makes create() fail if anyone else already owns
    // this name, so two supervisors for the same workspace can't both bind.
    ServerOptions::new()
        .first_pipe_instance(true)
        .create(&endpoint.address)
        .with_context(|| format!("bind named pipe {}", endpoint.address))
}

#[cfg(unix)]
fn cleanup_stale(endpoint: &Endpoint) -> Result<()> {
    let sock = std::path::Path::new(&endpoint.address);
    if !sock.exists() {
        let _ = std::fs::remove_file(&endpoint.pidfile);
        return Ok(());
    }
    if let Ok(text) = std::fs::read_to_string(&endpoint.pidfile)
        && let Ok(pid) = text.trim().parse::<i32>()
        && pid_alive(pid)
    {
        anyhow::bail!("another supervisor is already running for this workspace (pid {pid})");
    }
    let _ = std::fs::remove_file(sock);
    let _ = std::fs::remove_file(&endpoint.pidfile);
    Ok(())
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    // EPERM means the process exists but we can't signal it; still alive.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn cleanup(endpoint: &Endpoint) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&endpoint.address);
        let _ = std::fs::remove_file(&endpoint.pidfile);
    }
    #[cfg(windows)]
    {
        // Named pipes are released when the server handle drops; nothing to do.
        let _ = endpoint;
    }
}

/// Hides per-platform accept idioms: Unix listeners hand out streams forever,
/// Windows named pipes serve one client per instance and must reserve the next
/// before releasing the old one.
pub struct Acceptor {
    #[cfg(unix)]
    listener: Listener,
    #[cfg(windows)]
    current: Option<Listener>,
    #[cfg(windows)]
    name: String,
}

impl Acceptor {
    #[cfg(unix)]
    pub fn new(listener: Listener, _name: String) -> Self {
        Self { listener }
    }

    #[cfg(windows)]
    pub fn new(listener: Listener, name: String) -> Self {
        Self {
            current: Some(listener),
            name,
        }
    }

    #[cfg(unix)]
    pub async fn accept(&mut self) -> Result<ServerStream> {
        let (stream, _) = self.listener.accept().await.context("accept")?;
        Ok(stream)
    }

    #[cfg(windows)]
    pub async fn accept(&mut self) -> Result<ServerStream> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let server = self
            .current
            .take()
            .ok_or_else(|| anyhow::anyhow!("acceptor not initialised"))?;
        server.connect().await.context("connect named pipe")?;
        // Reserve the next instance before yielding the current one so a
        // racing client doesn't see a momentary gap in the pipe name.
        let next = ServerOptions::new()
            .create(&self.name)
            .context("create next pipe instance")?;
        self.current = Some(next);
        Ok(server)
    }
}

#[cfg(unix)]
pub async fn connect(endpoint: &Endpoint) -> Result<Stream> {
    Stream::connect(&endpoint.address)
        .await
        .with_context(|| format!("connect {}", endpoint.address))
}

#[cfg(windows)]
pub async fn connect(endpoint: &Endpoint) -> Result<Stream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    ClientOptions::new()
        .open(&endpoint.address)
        .with_context(|| format!("connect {}", endpoint.address))
}

#[cfg(unix)]
pub async fn probe(endpoint: &Endpoint) -> bool {
    tokio::net::UnixStream::connect(&endpoint.address)
        .await
        .is_ok()
}

#[cfg(windows)]
pub async fn probe(endpoint: &Endpoint) -> bool {
    use tokio::net::windows::named_pipe::ClientOptions;
    ClientOptions::new().open(&endpoint.address).is_ok()
}

pub async fn wait_ready(endpoint: &Endpoint, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if probe(endpoint).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "supervisor did not bind {} within {}",
                endpoint.address,
                humantime::format_duration(timeout),
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_stable_for_same_workspace() {
        let dir = std::env::current_dir().unwrap();
        let a = Endpoint::for_workspace(&dir).unwrap();
        let b = Endpoint::for_workspace(&dir).unwrap();
        assert_eq!(a.address, b.address);
    }
}
