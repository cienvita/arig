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

/// Longest path an AF_UNIX address can hold, NUL terminator included. Linux
/// allows 108 bytes, macOS and the BSDs 104.
#[cfg(all(unix, target_os = "linux"))]
const SUN_PATH_MAX: usize = 108;
#[cfg(all(unix, not(target_os = "linux")))]
const SUN_PATH_MAX: usize = 104;

/// Bases that can hold sockets, most preferred first. Sockets live outside the
/// workspace: a workspace deep enough to push its own socket path past
/// SUN_PATH_MAX could not be started at all.
#[cfg(unix)]
fn runtime_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    // Set by logind and friends. Already per-user, already short.
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        bases.push(PathBuf::from(dir).join("arig"));
    }
    // The fallbacks are shared, so the uid keeps two users on one host from
    // racing for a directory neither of them can write into.
    let uid = unsafe { libc::getuid() };
    let leaf = format!("arig-{uid}");
    if let Some(dir) = std::env::var_os("TMPDIR").filter(|d| !d.is_empty()) {
        bases.push(PathBuf::from(dir).join(&leaf));
    }
    bases.push(PathBuf::from("/tmp").join(&leaf));
    bases
}

#[cfg(unix)]
fn endpoint_from_canonical(abs: &Path) -> Result<Endpoint> {
    // Unix paths are case-sensitive, so hash the bytes as they are.
    let h = fnv1a_64(abs.as_os_str().as_encoded_bytes());
    for parent in runtime_bases() {
        let address = parent
            .join(format!("{h:016x}.sock"))
            .to_string_lossy()
            .into_owned();
        if address.len() >= SUN_PATH_MAX {
            continue;
        }
        return Ok(Endpoint {
            address,
            pidfile: parent.join(format!("{h:016x}.pid")),
            parent,
        });
    }
    anyhow::bail!(
        "no runtime directory short enough for a socket path; \
         set XDG_RUNTIME_DIR or TMPDIR to a shorter directory"
    )
}

#[cfg(windows)]
fn endpoint_from_canonical(abs: &Path) -> Result<Endpoint> {
    // Pipe names are a global namespace, so we mix the canonical workspace path
    // into the name. Path comparison on Windows is case-insensitive at the FS
    // level, so we lowercase first to keep two clients of the same dir in
    // agreement.
    let key = abs.as_os_str().to_string_lossy().to_lowercase();
    let h = fnv1a_64(key.as_bytes());
    Ok(Endpoint {
        address: format!(r"\\.\pipe\arig-{h:016x}"),
    })
}

/// FNV-1a, chosen because it is deterministic across rustc versions (unlike the
/// std DefaultHasher), so upgrades don't orphan existing supervisors.
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
    use std::os::unix::fs::DirBuilderExt;
    // 0700 because the fallback bases are world-writable and connecting to the
    // socket is full control of the supervisor.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&endpoint.parent)
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

    #[test]
    fn distinct_workspaces_get_distinct_endpoints() {
        let here = std::env::current_dir().unwrap();
        let up = here.parent().unwrap().to_path_buf();
        assert_ne!(
            Endpoint::for_workspace(&here).unwrap().address,
            Endpoint::for_workspace(&up).unwrap().address,
        );
    }

    #[cfg(unix)]
    fn deep_workspace(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("arig-test-{tag}-{}", std::process::id()));
        let mut dir = root.clone();
        for _ in 0..12 {
            dir = dir.join("0123456789abcdef0123456789abcdef");
        }
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binds_in_a_deep_workspace() {
        // Regression: the socket used to sit under the workspace, so a path
        // this deep pushed it past SUN_PATH_MAX and could not be bound at all.
        let (root, dir) = deep_workspace("deep");
        let endpoint = Endpoint::for_workspace(&dir).unwrap();
        assert!(
            endpoint.address.len() < SUN_PATH_MAX,
            "{}",
            endpoint.address
        );

        let listener = bind(&endpoint).unwrap();
        assert!(probe(&endpoint).await);
        drop(listener);
        cleanup(&endpoint);
        let _ = std::fs::remove_dir_all(&root);
    }
}
