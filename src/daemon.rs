//! Optional ultra-lightweight single-instance background sync daemon (`teams daemon`).

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, watch},
};

use crate::{
    auth::AuthManager,
    config::{ensure_parent, AppConfig},
    graph::GraphClient,
    models::{CoreEvent, POLL_RETRY_SUMMARY},
    storage::Storage,
    sync::{self, FocusTarget, SyncCommand, SyncController},
};

const IPC_CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const TUI_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const FAST_TCP_PROBE_TIMEOUT: Duration = Duration::from_millis(25);
const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_IPC_FRAME_BYTES: usize = 4 * 1024 * 1024;
const STARTUP_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(60);
const STOP_WAIT_TIMEOUT: Duration = Duration::from_millis(1500);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(30);
const LOCK_STALE_AGE: Duration = Duration::from_secs(5);
const BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub version: Option<String>,
    pub transport: Option<String>,
    pub endpoint: Option<String>,
    pub user_id: Option<String>,
    pub uptime_secs: Option<u64>,
    pub attached_clients: usize,
    pub sync_busy: bool,
    pub sync_summary: String,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
}

/// Handle returned when a TUI client attaches to a live background sync daemon.
pub struct TuiDaemonConnection {
    pub sync_ctrl: SyncController,
    pub events_rx: mpsc::UnboundedReceiver<CoreEvent>,
    pub status: DaemonStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DaemonRuntimeInfo {
    pid: u32,
    version: String,
    transport: String,
    endpoint: String,
    secret: String,
    user_id: Option<String>,
    started_at_unix: u64,
    started_at: String,
}

#[derive(Debug, Clone)]
struct DaemonPaths {
    #[cfg_attr(not(unix), allow(dead_code))]
    config_dir: PathBuf,
    state_file: PathBuf,
    lock_file: PathBuf,
}

impl DaemonPaths {
    fn from_config_dir(dir: &Path) -> Self {
        Self {
            config_dir: dir.to_path_buf(),
            state_file: dir.join("daemon.json"),
            lock_file: dir.join("daemon.lock"),
        }
    }

    fn current() -> Self {
        Self::from_config_dir(&crate::config::config_dir())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClientFrame {
    secret: String,
    op: ClientOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientOp {
    Status,
    Nudge {
        cmd: Option<SyncCommand>,
        focus: Option<FocusTarget>,
    },
    Shutdown,
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientStreamCmd {
    Command(SyncCommand),
    SetFocus(FocusTarget),
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum ServerFrame {
    Status(DaemonStatus),
    Ack { ok: bool, error: Option<String> },
    Subscribed(DaemonStatus),
    Event(CoreEvent),
    Pong,
    Error { message: String },
}

#[derive(Debug, Default)]
struct SharedSyncStatus {
    sync_busy: bool,
    sync_summary: String,
    last_sync_at: Option<String>,
    last_error: Option<String>,
}

struct DaemonSharedState {
    pid: u32,
    version: String,
    transport: String,
    endpoint: String,
    user_id: Option<String>,
    secret: String,
    started_instant: Instant,
    attached_clients: Arc<AtomicUsize>,
    sync_status: Mutex<SharedSyncStatus>,
}

impl DaemonSharedState {
    fn new(
        info: &DaemonRuntimeInfo,
        started_instant: Instant,
    ) -> Self {
        Self {
            pid: info.pid,
            version: info.version.clone(),
            transport: info.transport.clone(),
            endpoint: info.endpoint.clone(),
            user_id: info.user_id.clone(),
            secret: info.secret.clone(),
            started_instant,
            attached_clients: Arc::new(AtomicUsize::new(0)),
            sync_status: Mutex::new(SharedSyncStatus::default()),
        }
    }

    fn snapshot_status(&self) -> DaemonStatus {
        let guard = self
            .sync_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        DaemonStatus {
            running: true,
            pid: Some(self.pid),
            version: Some(self.version.clone()),
            transport: Some(self.transport.clone()),
            endpoint: Some(self.endpoint.clone()),
            user_id: self.user_id.clone(),
            uptime_secs: Some(self.started_instant.elapsed().as_secs()),
            attached_clients: self.attached_clients.load(Ordering::SeqCst),
            sync_busy: guard.sync_busy,
            sync_summary: guard.sync_summary.clone(),
            last_sync_at: guard.last_sync_at.clone(),
            last_error: guard.last_error.clone(),
        }
    }

    fn apply_event(&self, ev: &CoreEvent) {
        let mut guard = self
            .sync_status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match ev {
            CoreEvent::SyncStateChanged { busy, summary } => {
                guard.sync_busy = *busy;
                guard.sync_summary = summary.clone();
            }
            CoreEvent::ConversationsUpdated(_) | CoreEvent::MessagesUpdated { .. } => {
                guard.last_sync_at = Some(chrono::Utc::now().to_rfc3339());
                guard.last_error = None;
            }
            CoreEvent::SyncError(err) => {
                guard.last_error = Some(err.clone());
            }
            _ => {}
        }
    }
}

struct AttachedClientGuard {
    counter: Arc<AtomicUsize>,
    sync_ctrl: SyncController,
}

impl AttachedClientGuard {
    fn acquire(counter: &Arc<AtomicUsize>, sync_ctrl: &SyncController) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self {
            counter: Arc::clone(counter),
            sync_ctrl: sync_ctrl.clone(),
        }
    }
}

impl Drop for AttachedClientGuard {
    fn drop(&mut self) {
        if self.counter.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.sync_ctrl.set_focus(None, None);
            self.sync_ctrl.reset_window_focus();
        }
    }
}

struct StartupLockGuard {
    path: PathBuf,
}

impl StartupLockGuard {
    async fn acquire(path: &Path) -> Result<Self> {
        ensure_parent(path)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if let Ok(meta) = fs::metadata(path) {
                        if let Ok(modified) = meta.modified() {
                            if SystemTime::now()
                                .duration_since(modified)
                                .unwrap_or_default()
                                >= LOCK_STALE_AGE
                            {
                                let _ = fs::remove_file(path);
                                continue;
                            }
                        }
                    }
                    if Instant::now() >= deadline {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("Failed to create daemon startup lock file: {}", path.display())
                    });
                }
            }
        }
    }
}

impl Drop for StartupLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

enum IpcListener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

impl IpcListener {
    async fn accept(&self) -> io::Result<IpcStream> {
        match self {
            IpcListener::Tcp(listener) => {
                let (stream, _) = listener.accept().await?;
                let _ = stream.set_nodelay(true);
                Ok(IpcStream::Tcp(stream))
            }
            #[cfg(unix)]
            IpcListener::Unix(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok(IpcStream::Unix(stream))
            }
        }
    }
}

enum IpcStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl AsyncRead for IpcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            IpcStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            IpcStream::Unix(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            IpcStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            IpcStream::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            IpcStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            IpcStream::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            IpcStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            IpcStream::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    crate::hex_encode(bytes)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn is_user_compatible(expected_user_id: Option<&str>, actual_user_id: Option<&str>) -> bool {
    match (
        expected_user_id.map(str::trim).filter(|s| !s.is_empty()),
        actual_user_id.map(str::trim).filter(|s| !s.is_empty()),
    ) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => true,
    }
}

fn write_secret_file_atomic(path: &Path, data: &[u8]) -> Result<()> {
    ensure_parent(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Invalid state file path: {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("daemon.json");
    let tmp = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("Failed to create temporary state file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temporary state file: {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync temporary state file: {}", tmp.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set permissions on state file: {}", tmp.display()))?;
    }

    #[cfg(not(unix))]
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("Failed to create temporary state file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temporary state file: {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync temporary state file: {}", tmp.display()))?;
    }

    if let Err(err) = fs::rename(&tmp, path) {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("Failed to replace existing state file: {}", path.display()))?;
            fs::rename(&tmp, path)
                .with_context(|| format!("Failed to atomically save state file: {}", path.display()))?;
        } else {
            let _ = fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("Failed to save state file: {}", path.display()));
        }
    }
    Ok(())
}

fn read_runtime_info_or_clean(paths: &DaemonPaths) -> Option<DaemonRuntimeInfo> {
    let raw = fs::read(&paths.state_file).ok()?;
    match serde_json::from_slice::<DaemonRuntimeInfo>(&raw) {
        Ok(info) => Some(info),
        Err(_) => {
            let _ = fs::remove_file(&paths.state_file);
            None
        }
    }
}

fn cleanup_stale_files(paths: &DaemonPaths, info: Option<&DaemonRuntimeInfo>) {
    let _ = fs::remove_file(&paths.state_file);
    #[cfg(unix)]
    if let Some(meta) = info {
        if meta.transport == "unix" && !meta.endpoint.is_empty() {
            let _ = fs::remove_file(&meta.endpoint);
        }
    }
    #[cfg(not(unix))]
    let _ = info;
}

fn cleanup_owned_files(paths: &DaemonPaths, our_secret: &str, sock_path: Option<&Path>) {
    if let Ok(raw) = fs::read(&paths.state_file) {
        if let Ok(info) = serde_json::from_slice::<DaemonRuntimeInfo>(&raw) {
            if constant_time_eq(info.secret.as_bytes(), our_secret.as_bytes()) {
                let _ = fs::remove_file(&paths.state_file);
            }
        }
    }
    #[cfg(unix)]
    if let Some(path) = sock_path {
        let _ = fs::remove_file(path);
    }
    #[cfg(not(unix))]
    let _ = sock_path;
}

#[cfg(unix)]
fn select_unix_socket_path(config_dir: &Path) -> PathBuf {
    select_unix_socket_path_with_xdg(config_dir, std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
}

#[cfg(unix)]
fn select_unix_socket_path_with_xdg(config_dir: &Path, xdg_runtime_dir: Option<&str>) -> PathBuf {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;

    let candidate = config_dir.join("daemon.sock");
    if candidate.as_os_str().as_bytes().len() <= 90 {
        return candidate;
    }

    let canonical = crate::config::canonicalize_existing_ancestor(config_dir);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_os_str().as_bytes());
    let digest = crate::hex_encode(hasher.finalize());
    let short_hash = &digest[..12];

    if let Some(trimmed) = xdg_runtime_dir.map(str::trim).filter(|s| !s.is_empty()) {
        let xdg_candidate = PathBuf::from(trimmed)
            .join("teams-cli")
            .join(format!("daemon-{short_hash}.sock"));
        if xdg_candidate.as_os_str().as_bytes().len() <= 90 {
            return xdg_candidate;
        }
    }

    PathBuf::from(format!("/tmp/teams-cli-{short_hash}/daemon.sock"))
}

#[cfg(unix)]
fn ensure_unix_socket_dir(sock_path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = sock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create socket directory: {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to set permissions on socket directory: {}", parent.display()))?;
    }
    Ok(())
}

#[cfg_attr(not(unix), allow(unused_variables))]
async fn bind_ipc_listener(
    paths: &DaemonPaths,
) -> Result<(IpcListener, String, String, Option<PathBuf>)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let sock_path = select_unix_socket_path(&paths.config_dir);
        if ensure_unix_socket_dir(&sock_path).is_ok() {
            if sock_path.exists() {
                if std::os::unix::net::UnixStream::connect(&sock_path).is_ok() {
                    bail!("Daemon socket is already active: {}", sock_path.display());
                }
                let _ = fs::remove_file(&sock_path);
            }
            if let Ok(listener) = tokio::net::UnixListener::bind(&sock_path) {
                let _ = fs::set_permissions(&sock_path, fs::Permissions::from_mode(0o600));
                let endpoint = sock_path.to_string_lossy().into_owned();
                return Ok((
                    IpcListener::Unix(listener),
                    "unix".to_string(),
                    endpoint,
                    Some(sock_path),
                ));
            }
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind local IPC TCP listener")?;
    let endpoint = listener
        .local_addr()
        .context("Failed to get local IPC TCP address")?
        .to_string();
    Ok((
        IpcListener::Tcp(listener),
        "tcp".to_string(),
        endpoint,
        None,
    ))
}

async fn connect_ipc(transport: &str, endpoint: &str, timeout: Duration) -> Result<IpcStream> {
    match transport {
        #[cfg(unix)]
        "unix" => {
            let stream =
                tokio::time::timeout(timeout, tokio::net::UnixStream::connect(endpoint))
                    .await
                    .context("Unix socket connection timed out")?
                    .context("Unix socket connection failed")?;
            Ok(IpcStream::Unix(stream))
        }
        "tcp" => {
            let addr: SocketAddr = endpoint
                .parse()
                .with_context(|| format!("Invalid TCP endpoint: {endpoint}"))?;
            if !addr.ip().is_loopback() {
                bail!("Refusing to connect to non-loopback IPC address for security: {endpoint}");
            }
            // Windows loopback TCP connect takes ~2s to surface WSAECONNREFUSED on a dead port.
            let connect_timeout = if cfg!(windows) {
                timeout.max(Duration::from_millis(2_500))
            } else {
                timeout
            };
            let stream = tokio::time::timeout(connect_timeout, TcpStream::connect(addr))
                .await
                .context("TCP IPC connection timed out")?
                .context("TCP IPC connection failed")?;
            let _ = stream.set_nodelay(true);
            Ok(IpcStream::Tcp(stream))
        }
        other => bail!("Unsupported IPC transport: {other}"),
    }
}

fn is_stale_connect_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|io_err| {
            matches!(
                io_err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::AddrNotAvailable
            )
        })
    })
}

async fn read_limited_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let mut line = String::new();
    let mut limited = reader.take(MAX_IPC_FRAME_BYTES as u64 + 1);
    let n = limited.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_IPC_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame exceeds 4 MiB limit",
        ));
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

async fn write_json_line<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    frame: &T,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    tokio::time::timeout(IPC_WRITE_TIMEOUT, async {
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok::<_, io::Error>(())
    })
    .await
    .context("IPC write timed out")??;
    Ok(())
}

async fn ipc_roundtrip(
    paths: &DaemonPaths,
    op: ClientOp,
    timeout: Duration,
) -> Result<Option<ServerFrame>> {
    let Some(info) = read_runtime_info_or_clean(paths) else {
        return Ok(None);
    };
    let stream = match connect_ipc(&info.transport, &info.endpoint, timeout).await {
        Ok(s) => s,
        Err(err) => {
            if is_stale_connect_error(&err) {
                cleanup_stale_files(paths, Some(&info));
            }
            return Ok(None);
        }
    };

    let (read_half, mut write_half) = tokio::io::split(stream);
    let req = ClientFrame {
        secret: info.secret.clone(),
        op,
    };
    if !matches!(
        tokio::time::timeout(timeout, write_json_line(&mut write_half, &req)).await,
        Ok(Ok(()))
    ) {
        return Ok(None);
    }

    let mut reader = BufReader::new(read_half);
    match tokio::time::timeout(timeout, read_limited_line(&mut reader)).await {
        Ok(Ok(Some(line))) => match serde_json::from_str::<ServerFrame>(&line) {
            Ok(ServerFrame::Error { .. }) | Err(_) => Ok(None),
            Ok(frame) => Ok(Some(frame)),
        },
        _ => Ok(None),
    }
}

/// Fast (<0.5ms) local check whether a live daemon state file exists for `expected_user_id` (if known).
pub fn is_daemon_alive_fast(_cfg: &AppConfig, expected_user_id: Option<&str>) -> bool {
    is_daemon_alive_fast_at(&DaemonPaths::current(), expected_user_id)
}

fn is_daemon_alive_fast_at(paths: &DaemonPaths, expected_user_id: Option<&str>) -> bool {
    let Some(info) = read_runtime_info_or_clean(paths) else {
        return false;
    };
    if info.version != env!("CARGO_PKG_VERSION") {
        return false;
    }
    if !is_user_compatible(expected_user_id, info.user_id.as_deref()) {
        return false;
    }

    match info.transport.as_str() {
        #[cfg(unix)]
        "unix" => {
            if !Path::new(&info.endpoint).exists() {
                cleanup_stale_files(paths, Some(&info));
                return false;
            }
            if std::os::unix::net::UnixStream::connect(&info.endpoint).is_ok() {
                true
            } else {
                cleanup_stale_files(paths, Some(&info));
                false
            }
        }
        "tcp" => {
            let Ok(addr) = info.endpoint.parse::<SocketAddr>() else {
                cleanup_stale_files(paths, Some(&info));
                return false;
            };
            if !addr.ip().is_loopback() {
                return false;
            }
            match std::net::TcpStream::connect_timeout(&addr, FAST_TCP_PROBE_TIMEOUT) {
                Ok(_) => true,
                Err(err) => {
                    if fast_probe_error_proves_stale(&err) {
                        cleanup_stale_files(paths, Some(&info));
                    }
                    false
                }
            }
        }
        _ => {
            cleanup_stale_files(paths, Some(&info));
            false
        }
    }
}

/// Whether a failed fast TCP probe proves that nothing listens on the endpoint. Only a refused
/// connect does: a daemon busy for longer than the 25 ms budget times out instead, and must keep
/// its state file so the next command attaches to it rather than starting a second daemon. The
/// slower status / stop paths remove files that really are stale.
fn fast_probe_error_proves_stale(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::ConnectionRefused
}

/// Queries live daemon status over IPC (cleaning up stale state files if unreachable).
pub async fn query_daemon_status(_cfg: &AppConfig) -> Result<DaemonStatus> {
    query_daemon_status_at(&DaemonPaths::current()).await
}

async fn query_daemon_status_at(paths: &DaemonPaths) -> Result<DaemonStatus> {
    match ipc_roundtrip(paths, ClientOp::Status, IPC_CONNECT_TIMEOUT).await? {
        Some(ServerFrame::Status(status)) => Ok(status),
        _ => Ok(DaemonStatus::default()),
    }
}

/// Ensures the background daemon process is running, spawning `<current_exe> daemon run` detached if needed.
/// A daemon of another version, or one signed in as an account other than `expected_user_id`
/// (when both are known), is stopped and replaced.
pub async fn ensure_daemon_started(
    cfg: &AppConfig,
    expected_user_id: Option<&str>,
) -> Result<DaemonStatus> {
    ensure_daemon_started_at(cfg, &DaemonPaths::current(), expected_user_id).await
}

/// Whether a running daemon can serve this process: same version, and the same account when
/// both sides know theirs.
fn running_daemon_is_reusable(status: &DaemonStatus, expected_user_id: Option<&str>) -> bool {
    status.running
        && status.version.as_deref() == Some(env!("CARGO_PKG_VERSION"))
        && is_user_compatible(expected_user_id, status.user_id.as_deref())
}

async fn ensure_daemon_started_at(
    cfg: &AppConfig,
    paths: &DaemonPaths,
    expected_user_id: Option<&str>,
) -> Result<DaemonStatus> {
    let status = query_daemon_status_at(paths).await?;
    if status.running {
        if running_daemon_is_reusable(&status, expected_user_id) {
            return Ok(status);
        }
        let _ = stop_daemon_at(paths).await?;
    }

    let exe = std::env::current_exe().context("Failed to get current executable path")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }

    let mut child = cmd.spawn().context("Failed to spawn background daemon process")?;
    let deadline = Instant::now() + STARTUP_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        let status = query_daemon_status_at(paths).await?;
        if status.running {
            return Ok(status);
        }
        if let Ok(Some(exit)) = child.try_wait() {
            if !exit.success() {
                if let Ok(auth) = AuthManager::new(cfg.clone()) {
                    if auth.current_tokens().await.is_none() {
                        bail!("Not logged in. Run 'teams login' first.");
                    }
                }
                bail!("Background daemon exited unexpectedly (exit: {exit})");
            }
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    }

    let final_status = query_daemon_status_at(paths).await?;
    if final_status.running {
        return Ok(final_status);
    }
    bail!("Background daemon startup timed out (3s)")
}

/// Gracefully stops the running background daemon over IPC, if running.
pub async fn stop_daemon(_cfg: &AppConfig) -> Result<bool> {
    stop_daemon_at(&DaemonPaths::current()).await
}

/// Stops the running daemon when it is signed in as an account other than `user_id`, e.g.
/// after `teams login` switched accounts. A daemon whose account is unknown is left alone.
pub async fn stop_daemon_if_other_user(_cfg: &AppConfig, user_id: &str) -> Result<bool> {
    stop_daemon_if_other_user_at(&DaemonPaths::current(), user_id).await
}

async fn stop_daemon_if_other_user_at(paths: &DaemonPaths, user_id: &str) -> Result<bool> {
    let status = query_daemon_status_at(paths).await?;
    if !status.running || is_user_compatible(Some(user_id), status.user_id.as_deref()) {
        return Ok(false);
    }
    stop_daemon_at(paths).await
}

async fn stop_daemon_at(paths: &DaemonPaths) -> Result<bool> {
    let Some(info) = read_runtime_info_or_clean(paths) else {
        return Ok(false);
    };

    let ack = ipc_roundtrip(paths, ClientOp::Shutdown, IPC_CONNECT_TIMEOUT).await?;
    let was_running = matches!(ack, Some(ServerFrame::Ack { ok: true, .. }));
    if !was_running {
        cleanup_stale_files(paths, Some(&info));
        return Ok(false);
    }

    let deadline = Instant::now() + STOP_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        if !paths.state_file.exists() {
            break;
        }
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }
    cleanup_stale_files(paths, Some(&info));
    Ok(true)
}

/// Sends a single fire-and-forget `SyncCommand` (or optional focus update) to the running daemon.
pub async fn nudge_daemon(
    _cfg: &AppConfig,
    cmd: Option<SyncCommand>,
    focus: Option<FocusTarget>,
) -> Result<bool> {
    nudge_daemon_at(&DaemonPaths::current(), cmd, focus).await
}

async fn nudge_daemon_at(
    paths: &DaemonPaths,
    cmd: Option<SyncCommand>,
    focus: Option<FocusTarget>,
) -> Result<bool> {
    match ipc_roundtrip(paths, ClientOp::Nudge { cmd, focus }, IPC_CONNECT_TIMEOUT).await? {
        Some(ServerFrame::Ack { ok, .. }) => Ok(ok),
        _ => Ok(false),
    }
}

/// Attempts to attach a TUI session to a running daemon, returning a forwarding `SyncController` and live `CoreEvent` stream.
pub async fn try_connect_tui(
    cfg: &AppConfig,
    expected_user_id: Option<&str>,
) -> Option<TuiDaemonConnection> {
    try_connect_tui_at(cfg, &DaemonPaths::current(), expected_user_id).await
}

async fn try_connect_tui_at(
    _cfg: &AppConfig,
    paths: &DaemonPaths,
    expected_user_id: Option<&str>,
) -> Option<TuiDaemonConnection> {
    let info = read_runtime_info_or_clean(paths)?;
    if info.version != env!("CARGO_PKG_VERSION") {
        return None;
    }
    if !is_user_compatible(expected_user_id, info.user_id.as_deref()) {
        return None;
    }

    let stream = match connect_ipc(&info.transport, &info.endpoint, TUI_CONNECT_TIMEOUT).await {
        Ok(s) => s,
        Err(err) => {
            if is_stale_connect_error(&err) {
                cleanup_stale_files(paths, Some(&info));
            }
            return None;
        }
    };

    let (read_half, mut write_half) = tokio::io::split(stream);
    let subscribe_frame = ClientFrame {
        secret: info.secret,
        op: ClientOp::Subscribe,
    };
    if !matches!(
        tokio::time::timeout(
            TUI_CONNECT_TIMEOUT,
            write_json_line(&mut write_half, &subscribe_frame),
        )
        .await,
        Ok(Ok(()))
    ) {
        return None;
    }

    let mut reader = BufReader::new(read_half);
    let initial_status =
        match tokio::time::timeout(TUI_CONNECT_TIMEOUT, read_limited_line(&mut reader)).await {
            Ok(Ok(Some(raw))) => match serde_json::from_str::<ServerFrame>(&raw) {
                Ok(ServerFrame::Subscribed(status)) => status,
                _ => return None,
            },
            _ => return None,
        };

    let (focus_tx, mut focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let (events_tx, events_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = events_tx.closed() => break,
                changed = focus_rx.changed() => {
                    match changed {
                        Ok(()) => {
                            let focus = focus_rx.borrow_and_update().clone();
                            if write_json_line(&mut write_half, &ClientStreamCmd::SetFocus(focus))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                maybe_cmd = nudge_rx.recv() => {
                    match maybe_cmd {
                        // Never kill the shared background daemon when a single TUI client closes.
                        Some(SyncCommand::Shutdown) => continue,
                        Some(cmd) => {
                            if write_json_line(&mut write_half, &ClientStreamCmd::Command(cmd))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                line = read_limited_line(&mut reader) => {
                    match line {
                        Ok(Some(raw)) => {
                            match serde_json::from_str::<ServerFrame>(&raw) {
                                Ok(ServerFrame::Event(ev)) => {
                                    if events_tx.send(ev).is_err() {
                                        break;
                                    }
                                }
                                Ok(ServerFrame::Pong) => {}
                                Ok(ServerFrame::Error { .. }) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }
        }
    });

    Some(TuiDaemonConnection {
        sync_ctrl,
        events_rx,
        status: initial_status,
    })
}

/// Runs the background sync daemon in the foreground until a `Shutdown` RPC or Ctrl+C arrives.
pub async fn run_daemon_foreground(cfg: AppConfig) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let Some(tokens) = auth.current_tokens().await else {
        bail!("Not logged in. Run 'teams login' first.");
    };
    let user_id = tokens.user_id.clone();
    let storage = Storage::open(&crate::config::db_path(user_id.as_deref()))?;
    let mut graph = GraphClient::new(cfg.clone(), auth)?;
    if let Some(ref uid) = user_id {
        graph = graph.with_my_user_id(uid.clone());
    }

    let paths = DaemonPaths::current();
    run_daemon_inner(cfg, paths, graph, storage, user_id, true, true).await
}

async fn run_daemon_inner(
    cfg: AppConfig,
    paths: DaemonPaths,
    graph: GraphClient,
    storage: Storage,
    user_id: Option<String>,
    run_bootstrap: bool,
    enable_signals: bool,
) -> Result<()> {
    let (engine_event_tx, engine_event_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let sync_ctrl = sync::spawn_sync_engine(
        cfg,
        graph.clone(),
        storage.clone(),
        engine_event_tx.clone(),
    );

    if run_bootstrap {
        let boot_graph = graph;
        let boot_storage = storage.clone();
        let boot_tx = engine_event_tx.clone();
        tokio::spawn(async move {
            run_initial_bootstrap(boot_graph, boot_storage, boot_tx).await;
        });
    }

    run_daemon_with_engine(
        paths,
        user_id,
        storage,
        sync_ctrl,
        engine_event_rx,
        enable_signals,
    )
    .await
}

async fn run_initial_bootstrap(
    graph: GraphClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) {
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: true,
        summary: "Initial sync...".to_string(),
    });

    match sync::bootstrap_conversations(&graph, &storage).await {
        Ok(()) => {
            let st_convs = storage.clone();
            if let Ok(Ok(convs)) =
                tokio::task::spawn_blocking(move || st_convs.list_conversations()).await
            {
                let _ = event_tx.send(CoreEvent::ConversationsUpdated(convs));
            }
            let st_teams = storage.clone();
            if let Ok(Ok(teams)) = tokio::task::spawn_blocking(move || st_teams.list_teams()).await
            {
                let _ = event_tx.send(CoreEvent::TeamsUpdated(teams));
            }
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: "Idle".to_string(),
            });
        }
        Err(_) => {
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
        }
    }
}

async fn run_daemon_with_engine(
    paths: DaemonPaths,
    user_id: Option<String>,
    storage: Storage,
    sync_ctrl: SyncController,
    mut engine_event_rx: mpsc::UnboundedReceiver<CoreEvent>,
    enable_signals: bool,
) -> Result<()> {
    let lock_guard = StartupLockGuard::acquire(&paths.lock_file).await?;
    if query_daemon_status_at(&paths).await?.running {
        sync_ctrl.request(SyncCommand::Shutdown);
        return Ok(());
    }

    let (listener, transport, endpoint, sock_path) = bind_ipc_listener(&paths).await?;
    let now = chrono::Utc::now();
    let info = DaemonRuntimeInfo {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        transport,
        endpoint,
        secret: generate_secret(),
        user_id,
        started_at_unix: now.timestamp().max(0) as u64,
        started_at: now.to_rfc3339(),
    };
    let serialized = serde_json::to_vec_pretty(&info)?;
    if let Err(err) = write_secret_file_atomic(&paths.state_file, &serialized) {
        cleanup_owned_files(&paths, &info.secret, sock_path.as_deref());
        sync_ctrl.request(SyncCommand::Shutdown);
        return Err(err);
    }
    drop(lock_guard);

    let shared = Arc::new(DaemonSharedState::new(&info, Instant::now()));
    let (broadcast_tx, _) = broadcast::channel::<CoreEvent>(BROADCAST_CAPACITY);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let fanout_shared = Arc::clone(&shared);
    let fanout_tx = broadcast_tx.clone();
    let mut fanout_shutdown = shutdown_rx.clone();
    let fanout_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = fanout_shutdown.changed() => {
                    if *fanout_shutdown.borrow() {
                        break;
                    }
                }
                maybe_ev = engine_event_rx.recv() => {
                    match maybe_ev {
                        Some(ev) => {
                            fanout_shared.apply_event(&ev);
                            let _ = fanout_tx.send(ev);
                        }
                        None => break,
                    }
                }
            }
        }
    });

    let mut ctrl_c_fut = if enable_signals {
        Some(Box::pin(tokio::signal::ctrl_c()))
    } else {
        None
    };

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = async {
                if let Some(fut) = ctrl_c_fut.as_mut() {
                    let _ = fut.await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if ctrl_c_fut.is_some() => {
                let _ = shutdown_tx.send(true);
                break;
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok(stream) => {
                        let conn_shared = Arc::clone(&shared);
                        let conn_storage = storage.clone();
                        let conn_ctrl = sync_ctrl.clone();
                        let conn_broadcast = broadcast_tx.clone();
                        let conn_shutdown_tx = shutdown_tx.clone();
                        let conn_shutdown_rx = shutdown_rx.clone();
                        tokio::spawn(async move {
                            let _ = handle_client_connection(
                                stream,
                                conn_shared,
                                conn_storage,
                                conn_ctrl,
                                conn_broadcast,
                                conn_shutdown_tx,
                                conn_shutdown_rx,
                            )
                            .await;
                        });
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
    }

    cleanup_owned_files(&paths, &info.secret, sock_path.as_deref());
    sync_ctrl.request(SyncCommand::Shutdown);
    fanout_handle.abort();
    Ok(())
}

async fn handle_client_connection(
    stream: IpcStream,
    shared: Arc<DaemonSharedState>,
    storage: Storage,
    sync_ctrl: SyncController,
    broadcast_tx: broadcast::Sender<CoreEvent>,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let Some(first_line) = read_limited_line(&mut reader).await? else {
        return Ok(());
    };
    let Ok(frame) = serde_json::from_str::<ClientFrame>(&first_line) else {
        let _ = write_json_line(
            &mut write_half,
            &ServerFrame::Error {
                message: "invalid_frame".into(),
            },
        )
        .await;
        return Ok(());
    };

    if !constant_time_eq(frame.secret.as_bytes(), shared.secret.as_bytes()) {
        let _ = write_json_line(
            &mut write_half,
            &ServerFrame::Error {
                message: "unauthorized".into(),
            },
        )
        .await;
        return Ok(());
    }

    match frame.op {
        ClientOp::Status => {
            let status = shared.snapshot_status();
            write_json_line(&mut write_half, &ServerFrame::Status(status)).await?;
        }
        ClientOp::Nudge { cmd, focus } => {
            if let Some(f) = focus {
                sync_ctrl.set_focus(f.target, f.thread_root_id);
            }
            if let Some(c) = cmd {
                if !matches!(c, SyncCommand::Shutdown) {
                    sync_ctrl.request(c);
                }
            }
            write_json_line(
                &mut write_half,
                &ServerFrame::Ack {
                    ok: true,
                    error: None,
                },
            )
            .await?;
        }
        ClientOp::Shutdown => {
            let _ = shutdown_tx.send(true);
            write_json_line(
                &mut write_half,
                &ServerFrame::Ack {
                    ok: true,
                    error: None,
                },
            )
            .await?;
        }
        ClientOp::Subscribe => {
            let _guard = AttachedClientGuard::acquire(&shared.attached_clients, &sync_ctrl);
            sync_ctrl.notify_activity();
            let mut broadcast_rx = broadcast_tx.subscribe();
            let status = shared.snapshot_status();
            write_json_line(&mut write_half, &ServerFrame::Subscribed(status)).await?;

            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    line = read_limited_line(&mut reader) => {
                        match line {
                            Ok(Some(raw)) => {
                                match serde_json::from_str::<ClientStreamCmd>(&raw) {
                                    Ok(ClientStreamCmd::Command(cmd)) => {
                                        if !matches!(cmd, SyncCommand::Shutdown) {
                                            sync_ctrl.request(cmd);
                                        }
                                    }
                                    Ok(ClientStreamCmd::SetFocus(focus)) => {
                                        sync_ctrl.set_focus(focus.target, focus.thread_root_id);
                                    }
                                    Ok(ClientStreamCmd::Ping) => {
                                        if write_json_line(&mut write_half, &ServerFrame::Pong).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            Ok(None) | Err(_) => break,
                        }
                    }
                    ev_res = broadcast_rx.recv() => {
                        match ev_res {
                            Ok(ev) => {
                                if write_json_line(&mut write_half, &ServerFrame::Event(ev)).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let st = storage.clone();
                                if let Ok(Ok(convs)) = tokio::task::spawn_blocking(move || st.list_conversations()).await {
                                    let ev = CoreEvent::ConversationsUpdated(convs);
                                    if write_json_line(&mut write_half, &ServerFrame::Event(ev)).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ConversationKind, ConversationSummary, ConversationTarget};

    struct TestDaemonHarness {
        _dir: tempfile::TempDir,
        paths: DaemonPaths,
        focus_rx: watch::Receiver<FocusTarget>,
        nudge_rx: mpsc::UnboundedReceiver<SyncCommand>,
        engine_event_tx: mpsc::UnboundedSender<CoreEvent>,
        handle: Option<tokio::task::JoinHandle<Result<()>>>,
    }

    impl TestDaemonHarness {
        async fn start(user_id: Option<&str>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let paths = DaemonPaths::from_config_dir(dir.path());
            let (focus_tx, focus_rx) = watch::channel(FocusTarget::default());
            let (nudge_tx, nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
            let (engine_event_tx, engine_event_rx) = mpsc::unbounded_channel::<CoreEvent>();
            let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, true);

            let task_paths = paths.clone();
            let uid = user_id.map(str::to_string);
            let storage = Storage::open_in_memory().unwrap();
            let handle = tokio::spawn(async move {
                run_daemon_with_engine(task_paths, uid, storage, sync_ctrl, engine_event_rx, false)
                    .await
            });

            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if paths.state_file.exists() && is_daemon_alive_fast_at(&paths, user_id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            Self {
                _dir: dir,
                paths,
                focus_rx,
                nudge_rx,
                engine_event_tx,
                handle: Some(handle),
            }
        }

        async fn wait_exit(&mut self) -> Result<()> {
            if let Some(handle) = self.handle.take() {
                handle.await??;
            }
            Ok(())
        }
    }

    impl Drop for TestDaemonHarness {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.abort();
            }
        }
    }

    #[tokio::test]
    async fn daemon_lifecycle_status_and_graceful_shutdown() {
        let mut harness = TestDaemonHarness::start(Some("user-1")).await;

        assert!(is_daemon_alive_fast_at(&harness.paths, Some("user-1")));
        assert!(is_daemon_alive_fast_at(&harness.paths, None));
        assert!(!is_daemon_alive_fast_at(&harness.paths, Some("other-user")));

        let status = query_daemon_status_at(&harness.paths).await.unwrap();
        assert!(status.running);
        assert_eq!(status.pid, Some(std::process::id()));
        assert_eq!(status.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(status.user_id.as_deref(), Some("user-1"));
        assert_eq!(status.attached_clients, 0);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&harness.paths.state_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let stopped = stop_daemon_at(&harness.paths).await.unwrap();
        assert!(stopped);
        harness.wait_exit().await.unwrap();

        assert!(!harness.paths.state_file.exists());
        assert!(!is_daemon_alive_fast_at(&harness.paths, Some("user-1")));
        let after = query_daemon_status_at(&harness.paths).await.unwrap();
        assert!(!after.running);
    }

    #[tokio::test]
    async fn daemon_rejects_wrong_secret_token() {
        let mut harness = TestDaemonHarness::start(Some("user-sec")).await;
        let info = read_runtime_info_or_clean(&harness.paths).unwrap();
        assert_eq!(info.secret.len(), 64);

        let stream = connect_ipc(&info.transport, &info.endpoint, Duration::from_millis(500))
            .await
            .unwrap();
        let (read_half, mut write_half) = tokio::io::split(stream);
        let bad_frame = ClientFrame {
            secret: "0".repeat(64),
            op: ClientOp::Status,
        };
        write_json_line(&mut write_half, &bad_frame).await.unwrap();

        let mut lines = BufReader::new(read_half).lines();
        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: ServerFrame = serde_json::from_str(&resp_line).unwrap();
        assert!(matches!(resp, ServerFrame::Error { ref message } if message == "unauthorized"));

        // Daemon must still be alive and state file intact after unauthorized attempt.
        let status = query_daemon_status_at(&harness.paths).await.unwrap();
        assert!(status.running);

        assert!(stop_daemon_at(&harness.paths).await.unwrap());
        harness.wait_exit().await.unwrap();
    }

    #[tokio::test]
    async fn tui_attach_broadcast_and_disconnect_keeps_daemon_alive() {
        let mut harness = TestDaemonHarness::start(Some("user-tui")).await;
        let cfg = AppConfig::default();

        let mut conn = try_connect_tui_at(&cfg, &harness.paths, Some("user-tui"))
            .await
            .expect("should attach to running daemon");
        assert!(conn.status.running);
        assert_eq!(conn.status.attached_clients, 1);

        // Initial NotifyUserActivity triggered on Subscribe.
        let first_cmd = tokio::time::timeout(Duration::from_secs(1), harness.nudge_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first_cmd, SyncCommand::NotifyUserActivity));

        let live_status = query_daemon_status_at(&harness.paths).await.unwrap();
        assert_eq!(live_status.attached_clients, 1);

        // Forward SyncCommand & FocusTarget from TUI -> daemon.
        conn.sync_ctrl.request(SyncCommand::RefreshConversations);
        let forwarded_cmd = tokio::time::timeout(Duration::from_secs(1), harness.nudge_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(forwarded_cmd, SyncCommand::RefreshConversations));

        conn.sync_ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "chat-42".to_string(),
            }),
            Some("root-1".to_string()),
        );
        tokio::time::timeout(Duration::from_secs(1), harness.focus_rx.changed())
            .await
            .unwrap()
            .unwrap();
        let focus = harness.focus_rx.borrow().clone();
        assert_eq!(
            focus.target,
            Some(ConversationTarget::Chat {
                chat_id: "chat-42".to_string()
            })
        );
        assert_eq!(focus.thread_root_id.as_deref(), Some("root-1"));

        // Broadcast CoreEvent from daemon -> TUI + status update.
        harness
            .engine_event_tx
            .send(CoreEvent::SyncStateChanged {
                busy: true,
                summary: "Syncing".to_string(),
            })
            .unwrap();
        harness
            .engine_event_tx
            .send(CoreEvent::ConversationsUpdated(vec![ConversationSummary {
                conv_key: "chat:chat-42".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "chat-42".to_string(),
                },
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "Test Chat".to_string(),
                topic: None,
                unread_count: 1,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: Some("hello".to_string()),
                last_updated_at: None,
                pinned: false,
                peer_user_id: None,
                web_url: None,
            }]))
            .unwrap();

        let ev1 = tokio::time::timeout(Duration::from_secs(1), conn.events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            ev1,
            CoreEvent::SyncStateChanged { busy: true, .. }
        ));

        let ev2 = tokio::time::timeout(Duration::from_secs(1), conn.events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev2, CoreEvent::ConversationsUpdated(ref items) if items.len() == 1));

        let updated_status = query_daemon_status_at(&harness.paths).await.unwrap();
        assert!(updated_status.sync_busy);
        assert_eq!(updated_status.sync_summary, "Syncing");
        assert!(updated_status.last_sync_at.is_some());

        // Simulating TUI exit sends Shutdown through sync_ctrl then drops connection:
        // shared daemon MUST keep running with attached_clients == 0.
        conn.sync_ctrl.request(SyncCommand::Shutdown);
        drop(conn);

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut detached = false;
        while Instant::now() < deadline {
            let st = query_daemon_status_at(&harness.paths).await.unwrap();
            if st.running && st.attached_clients == 0 {
                detached = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(detached, "daemon should remain running with 0 clients");
        assert!(
            harness.focus_rx.borrow().target.is_none(),
            "focus must be cleared when last TUI client detaches"
        );

        // Also test nudge_daemon RPC directly.
        assert!(nudge_daemon_at(
            &harness.paths,
            Some(SyncCommand::SyncActiveNow),
            None,
        )
        .await
        .unwrap());
        let nudged = tokio::time::timeout(Duration::from_secs(1), harness.nudge_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(nudged, SyncCommand::SyncActiveNow));

        assert!(stop_daemon_at(&harness.paths).await.unwrap());
        harness.wait_exit().await.unwrap();
    }

    #[tokio::test]
    async fn stale_daemon_json_is_auto_cleaned_by_probe_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::from_config_dir(dir.path());

        // Find an unused ephemeral port then immediately drop the listener.
        let dead_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };

        let stale = DaemonRuntimeInfo {
            pid: 999_999,
            version: env!("CARGO_PKG_VERSION").to_string(),
            transport: "tcp".to_string(),
            endpoint: format!("127.0.0.1:{dead_port}"),
            secret: "a".repeat(64),
            user_id: Some("stale-user".to_string()),
            started_at_unix: 1_700_000_000,
            started_at: "2023-11-14T22:13:20Z".to_string(),
        };
        write_secret_file_atomic(
            &paths.state_file,
            &serde_json::to_vec_pretty(&stale).unwrap(),
        )
        .unwrap();
        assert!(paths.state_file.exists());

        // Fast probe reports the dead endpoint. It removes daemon.json only when the connect
        // was refused; on Windows a closed loopback port can time out within 25 ms instead, and
        // then the status path below does the cleanup.
        let refused = std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{dead_port}").parse().unwrap(),
            FAST_TCP_PROBE_TIMEOUT,
        )
        .err()
        .is_some_and(|e| fast_probe_error_proves_stale(&e));
        assert!(!is_daemon_alive_fast_at(&paths, Some("stale-user")));
        assert_eq!(paths.state_file.exists(), !refused);

        // Recreate stale file and verify query_daemon_status_at also cleans it up.
        write_secret_file_atomic(
            &paths.state_file,
            &serde_json::to_vec_pretty(&stale).unwrap(),
        )
        .unwrap();
        assert!(paths.state_file.exists());

        let status = query_daemon_status_at(&paths).await.unwrap();
        assert!(!status.running);
        assert!(!paths.state_file.exists());

        // Corrupt JSON is also auto-cleaned.
        write_secret_file_atomic(&paths.state_file, b"{corrupt json").unwrap();
        assert!(!is_daemon_alive_fast_at(&paths, None));
        assert!(!paths.state_file.exists());
    }

    // TEAMS-31: a probe that times out says "not reachable now", not "stale".
    #[test]
    fn regression_teams_31_only_a_refused_fast_probe_removes_the_state_file() {
        use std::io::{Error, ErrorKind};
        assert!(fast_probe_error_proves_stale(&Error::from(ErrorKind::ConnectionRefused)));
        for kind in [ErrorKind::TimedOut, ErrorKind::WouldBlock, ErrorKind::Interrupted, ErrorKind::Other] {
            assert!(!fast_probe_error_proves_stale(&Error::from(kind)), "{kind:?}");
        }
    }

    // TEAMS-14: a daemon signed in as another account is not reused.
    #[test]
    fn regression_teams_14_daemon_reuse_requires_same_account() {
        let status = |user: Option<&str>| DaemonStatus {
            running: true,
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            user_id: user.map(str::to_string),
            ..DaemonStatus::default()
        };
        assert!(running_daemon_is_reusable(&status(Some("user-1")), Some("user-1")));
        assert!(!running_daemon_is_reusable(&status(Some("user-1")), Some("user-2")));
        // Unknown on either side stays compatible, as before.
        assert!(running_daemon_is_reusable(&status(Some("user-1")), None));
        assert!(running_daemon_is_reusable(&status(None), Some("user-2")));
        let mut old = status(Some("user-1"));
        old.version = Some("0.0.1".to_string());
        assert!(!running_daemon_is_reusable(&old, Some("user-1")));
        assert!(!running_daemon_is_reusable(&DaemonStatus::default(), None));
    }

    #[tokio::test]
    async fn regression_teams_14_login_as_other_account_stops_daemon() {
        let mut harness = TestDaemonHarness::start(Some("user-1")).await;

        // Same account: the daemon keeps running.
        assert!(!stop_daemon_if_other_user_at(&harness.paths, "user-1").await.unwrap());
        assert!(query_daemon_status_at(&harness.paths).await.unwrap().running);

        // Another account: it is shut down and its state file is gone.
        assert!(stop_daemon_if_other_user_at(&harness.paths, "user-2").await.unwrap());
        harness.wait_exit().await.unwrap();
        assert!(!harness.paths.state_file.exists());
        assert!(!query_daemon_status_at(&harness.paths).await.unwrap().running);

        // Nothing running: nothing to do.
        assert!(!stop_daemon_if_other_user_at(&harness.paths, "user-2").await.unwrap());
    }

    #[tokio::test]
    async fn single_instance_guard_exits_cleanly_when_already_running() {
        let mut harness = TestDaemonHarness::start(Some("user-dup")).await;

        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
        let (_ev_tx, ev_rx) = mpsc::unbounded_channel();
        let dup_ctrl = SyncController::from_channels(focus_tx, nudge_tx, true);

        let dup_res = run_daemon_with_engine(
            harness.paths.clone(),
            Some("user-dup".into()),
            Storage::open_in_memory().unwrap(),
            dup_ctrl,
            ev_rx,
            false,
        )
        .await;
        assert!(dup_res.is_ok());

        // Original daemon must still be alive.
        assert!(query_daemon_status_at(&harness.paths).await.unwrap().running);

        assert!(stop_daemon_at(&harness.paths).await.unwrap());
        harness.wait_exit().await.unwrap();
    }

    #[tokio::test]
    async fn ipc_roundtrip_preserves_state_file_on_read_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DaemonPaths::from_config_dir(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();

        // Accept the connection and hold it open without responding so read times out.
        let stall_server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(2)).await;
                drop(stream);
            }
        });

        let info = DaemonRuntimeInfo {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            transport: "tcp".to_string(),
            endpoint,
            secret: "b".repeat(64),
            user_id: Some("slow-user".to_string()),
            started_at_unix: 1_700_000_000,
            started_at: "2023-11-14T22:13:20Z".to_string(),
        };
        write_secret_file_atomic(
            &paths.state_file,
            &serde_json::to_vec_pretty(&info).unwrap(),
        )
        .unwrap();

        let resp = ipc_roundtrip(&paths, ClientOp::Status, Duration::from_millis(80))
            .await
            .unwrap();
        assert!(resp.is_none());
        assert!(
            paths.state_file.exists(),
            "read timeout must NOT delete live daemon.json"
        );

        stall_server.abort();
    }

    #[tokio::test]
    async fn read_limited_line_rejects_oversized_frame() {
        let oversized = vec![b'x'; MAX_IPC_FRAME_BYTES + 16];
        let mut reader = BufReader::new(std::io::Cursor::new(oversized));
        let err = read_limited_line(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let valid = b"{\"ok\":true}\n{\"ok\":false}\n";
        let mut valid_reader = BufReader::new(std::io::Cursor::new(valid));
        assert_eq!(
            read_limited_line(&mut valid_reader).await.unwrap().as_deref(),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            read_limited_line(&mut valid_reader).await.unwrap().as_deref(),
            Some("{\"ok\":false}")
        );
        assert_eq!(read_limited_line(&mut valid_reader).await.unwrap(), None);
    }

    #[test]
    fn core_event_messages_updated_ipc_serde_compat() {
        // 1) Legacy IPC frame without `changed_msg_ids` deserializes cleanly to `None` (snapshot fallback).
        let legacy_json = r#"{"type":"event","data":{"MessagesUpdated":{"conv_key":"chat:c1","parent_msg_id":null,"messages":[],"notify_candidate":null}}}"#;
        let legacy_frame: ServerFrame = serde_json::from_str(legacy_json).unwrap();
        match legacy_frame {
            ServerFrame::Event(CoreEvent::MessagesUpdated {
                conv_key,
                parent_msg_id,
                messages,
                notify_candidate,
                changed_msg_ids,
            }) => {
                assert_eq!(conv_key, "chat:c1");
                assert!(parent_msg_id.is_none());
                assert!(messages.is_empty());
                assert!(notify_candidate.is_none());
                assert_eq!(changed_msg_ids, None);
            }
            other => panic!("unexpected frame: {other:?}"),
        }

        // 2) `changed_msg_ids: None` omits the field on serialization.
        let snapshot_frame = ServerFrame::Event(CoreEvent::MessagesUpdated {
            conv_key: "chat:c1".to_string(),
            parent_msg_id: None,
            messages: Vec::new(),
            notify_candidate: None,
            changed_msg_ids: None,
        });
        let snapshot_json = serde_json::to_string(&snapshot_frame).unwrap();
        assert!(!snapshot_json.contains("changed_msg_ids"));

        // 3) `changed_msg_ids: Some(...)` round-trips identically.
        let inc_frame = ServerFrame::Event(CoreEvent::MessagesUpdated {
            conv_key: "chat:c1".to_string(),
            parent_msg_id: None,
            messages: Vec::new(),
            notify_candidate: None,
            changed_msg_ids: Some(vec!["m42".to_string()]),
        });
        let inc_json = serde_json::to_string(&inc_frame).unwrap();
        assert!(inc_json.contains("\"changed_msg_ids\":[\"m42\"]"));
        let decoded: ServerFrame = serde_json::from_str(&inc_json).unwrap();
        match decoded {
            ServerFrame::Event(CoreEvent::MessagesUpdated {
                changed_msg_ids, ..
            }) => {
                assert_eq!(changed_msg_ids, Some(vec!["m42".to_string()]));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn select_unix_socket_path_symlink_hash_consistency_and_xdg_profile_isolation() {
        let root = tempfile::tempdir().unwrap();
        let real_home = root.path().join("real-home");
        let sym_home = root.path().join("sym-home");
        let long_seg = "very-long-profile-directory-segment-for-uds-fallback-threshold-check";
        let real_profile_a = real_home.join(long_seg).join("profile-alpha");
        let sym_profile_a = sym_home.join(long_seg).join("profile-alpha");
        let real_profile_b = real_home.join(long_seg).join("profile-beta");
        fs::create_dir_all(&real_profile_a).unwrap();
        std::os::unix::fs::symlink(&real_home, &sym_home).unwrap();

        let xdg = Some("/run/user/1000");
        let sock_real_a = select_unix_socket_path_with_xdg(&real_profile_a, xdg);
        let sock_sym_a = select_unix_socket_path_with_xdg(&sym_profile_a, xdg);
        assert_eq!(sock_real_a, sock_sym_a);
        assert!(sock_real_a.starts_with("/run/user/1000/teams-cli/daemon-"));

        // Non-existent tail under symlinked vs canonical parent also hashes identically.
        let sym_profile_b = sym_home.join(long_seg).join("profile-beta");
        let sock_real_b = select_unix_socket_path_with_xdg(&real_profile_b, xdg);
        let sock_sym_b = select_unix_socket_path_with_xdg(&sym_profile_b, xdg);
        assert_eq!(sock_real_b, sock_sym_b);

        // Two different long-path profiles must get distinct sockets in XDG_RUNTIME_DIR.
        assert_ne!(sock_real_a, sock_real_b);

        // /tmp fallback without XDG_RUNTIME_DIR also matches across symlink and isolates profiles.
        assert_eq!(
            select_unix_socket_path_with_xdg(&real_profile_a, None),
            select_unix_socket_path_with_xdg(&sym_profile_a, None)
        );
        assert_ne!(
            select_unix_socket_path_with_xdg(&real_profile_a, None),
            select_unix_socket_path_with_xdg(&real_profile_b, None)
        );
    }
}
