//! Serving MCP from inside a running program (the Kerf app), so an
//! assistant can work on the project that is open there.
//!
//! MCP clients start servers as programs speaking over stdio, so they run
//! `kerf-mcp --app`, a bridge that connects to the [`Host`] here and relays
//! the conversation. The host listens on `127.0.0.1` (a random port) and
//! only accepts connections that present the secret token from the
//! connection file, which only the user can read.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::KerfServer;

/// What a bridge needs to reach the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub port: u16,
    pub token: String,
    /// Process id of the host (for diagnostics).
    pub pid: u32,
}

/// Where the running app announces itself: `$KERF_ASSISTANT_FILE`, or
/// `assistant.json` in Kerf's runtime folder (e.g. `/run/user/1000/kerf`).
pub fn default_connection_file() -> PathBuf {
    if let Some(p) = std::env::var_os("KERF_ASSISTANT_FILE") {
        return PathBuf::from(p);
    }
    dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("kerf")
        .join("assistant.json")
}

impl Connection {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    /// Write the file readable by the current user only.
    fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    }
}

/// 128 random bits, hex. `RandomState` keys come from the operating
/// system's random source.
fn new_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let part = |salt: u64| {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        h.finish()
    };
    format!("{:016x}{:016x}", part(1), part(2))
}

/// Compare without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A connected assistant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub id: u64,
    /// The client program's name, e.g. "claude-code".
    pub name: String,
}

#[derive(Default)]
struct Shared {
    clients: Mutex<Vec<Client>>,
    /// Cancelled to disconnect everyone; replaced afterwards.
    sessions: Mutex<CancellationToken>,
    next_id: Mutex<u64>,
}

/// A running host. Stops (and removes its connection file) when dropped.
pub struct Host {
    file: PathBuf,
    port: u16,
    stop: CancellationToken,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Host {
    /// Start serving; `make_server` builds the server for each connection
    /// (they normally share one executor). `on_change` is called whenever a
    /// client connects or disconnects (e.g. to repaint an indicator).
    pub fn start(
        file: PathBuf,
        make_server: impl Fn() -> KerfServer + Send + Sync + 'static,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kerf-assistant")
            .enable_all()
            .build()?;
        let listener =
            runtime.block_on(TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))))?;
        let port = listener.local_addr()?.port();
        let token = new_token();
        Connection {
            port,
            token: token.clone(),
            pid: std::process::id(),
        }
        .write(&file)?;

        let stop = CancellationToken::new();
        let shared = Arc::new(Shared::default());
        let make_server = Arc::new(make_server);
        let on_change = Arc::new(on_change);
        let thread = {
            let (stop, shared) = (stop.clone(), shared.clone());
            std::thread::Builder::new()
                .name("kerf-assistant-host".into())
                .spawn(move || {
                    runtime.block_on(accept_loop(
                        listener,
                        token,
                        stop,
                        shared,
                        make_server,
                        on_change,
                    ));
                    runtime.shutdown_timeout(Duration::from_secs(2));
                })?
        };
        Ok(Self {
            file,
            port,
            stop,
            shared,
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn connection_file(&self) -> &Path {
        &self.file
    }

    /// The assistants connected now.
    pub fn clients(&self) -> Vec<Client> {
        self.shared.clients.lock().unwrap().clone()
    }

    /// End every current connection (new ones are still accepted).
    pub fn disconnect_all(&self) {
        let mut s = self.shared.sessions.lock().unwrap();
        s.cancel();
        *s = CancellationToken::new();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.disconnect_all();
        self.stop.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Only remove the file if it is still ours (another instance may
        // have started since).
        if Connection::read(&self.file).is_ok_and(|c| c.port == self.port) {
            let _ = std::fs::remove_file(&self.file);
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    token: String,
    stop: CancellationToken,
    shared: Arc<Shared>,
    make_server: Arc<dyn Fn() -> KerfServer + Send + Sync>,
    on_change: Arc<dyn Fn() + Send + Sync>,
) {
    loop {
        let stream = tokio::select! {
            _ = stop.cancelled() => return,
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(_) => continue,
            },
        };
        let (token, shared, make_server, on_change) = (
            token.clone(),
            shared.clone(),
            make_server.clone(),
            on_change.clone(),
        );
        tokio::spawn(async move {
            let _ = serve_one(stream, &token, &shared, &*make_server, &*on_change).await;
        });
    }
}

/// Read one line (up to `\n`) without reading past it.
async fn read_line(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 256 {
        if stream.read(&mut byte).await? == 0 || byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&line).trim().to_string())
}

async fn serve_one(
    mut stream: TcpStream,
    token: &str,
    shared: &Shared,
    make_server: &(dyn Fn() -> KerfServer + Send + Sync),
    on_change: &(dyn Fn() + Send + Sync),
) -> std::io::Result<()> {
    let hello = tokio::time::timeout(Duration::from_secs(5), read_line(&mut stream))
        .await
        .map_err(|_| std::io::Error::other("no greeting"))??;
    let presented = hello.strip_prefix("KERF ").unwrap_or("");
    if !same(presented.as_bytes(), token.as_bytes()) {
        stream.write_all(b"ERR wrong token\n").await?;
        return Ok(());
    }
    stream.write_all(b"OK\n").await?;

    let session_ct = shared.sessions.lock().unwrap().clone();
    let service = make_server()
        .serve(stream)
        .await
        .map_err(std::io::Error::other)?;
    let id = {
        let mut n = shared.next_id.lock().unwrap();
        *n += 1;
        *n
    };
    let name = service
        .peer_info()
        .map(|i| i.client_info.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "assistant".into());
    shared.clients.lock().unwrap().push(Client { id, name });
    on_change();

    // "Disconnect" in the app ends the session.
    let ct = service.cancellation_token();
    let watcher = tokio::spawn(async move {
        session_ct.cancelled().await;
        ct.cancel();
    });
    let _ = service.waiting().await;
    watcher.abort();

    shared.clients.lock().unwrap().retain(|c| c.id != id);
    on_change();
    Ok(())
}

/// The bridge: relay stdin/stdout to the host announced in `file`.
pub async fn bridge(file: &Path) -> Result<(), String> {
    let conn = Connection::read(file).map_err(|_| {
        format!(
            "Kerf isn't accepting assistants (no {}). Open Kerf and turn on \
             Preferences → Assistant.",
            file.display()
        )
    })?;
    let mut stream = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], conn.port)))
        .await
        .map_err(|_| {
            "Kerf isn't running, or has turned assistants off. Open Kerf and turn on \
             Preferences → Assistant."
                .to_string()
        })?;
    stream
        .write_all(format!("KERF {}\n", conn.token).as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let reply = read_line(&mut stream).await.map_err(|e| e.to_string())?;
    if reply != "OK" {
        return Err(format!("Kerf refused the connection: {reply}"));
    }
    // Relay both ways; stop as soon as either side ends (the client closing
    // stdin, or Kerf ending the session), so the client sees the end too.
    let (mut from_kerf, mut to_kerf) = stream.into_split();
    let (mut stdin, mut stdout) = (tokio::io::stdin(), tokio::io::stdout());
    tokio::select! {
        r = tokio::io::copy(&mut stdin, &mut to_kerf) => r.map(|_| ()),
        r = tokio::io::copy(&mut from_kerf, &mut stdout) => r.map(|_| ()),
    }
    .map_err(|e| e.to_string())
}
