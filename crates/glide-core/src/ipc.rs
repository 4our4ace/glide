//! Lightweight, secure, zero-extra-dependency local control IPC (`glide ctl` & `glide send`).
//!
//! - On Unix (Linux / macOS): binds a Unix domain socket at `$XDG_RUNTIME_DIR/glide/ctl.sock`
//!   (falling back to `<config_dir>/ctl.sock`) with `0o700` parent directory permissions and
//!   `0o600` socket permissions, cleaning up stale sockets on startup and removing the socket
//!   on [`Drop`].
//! - On Windows: binds a loopback-only `127.0.0.1:0` TCP listener and writes the chosen port plus
//!   a 32-byte CSPRNG hex auth token into `<config_dir>/ctl.endpoint`, verified with a
//!   constant-time comparison on every request.
//!
//! Protocol: newline-delimited JSON requests and responses, bounded to [`MAX_IPC_LINE_BYTES`]
//! (64 KiB) with a [`IPC_IO_TIMEOUT`] (5s) per-message timeout.

use crate::config::default_config_dir;
use crate::runtime::{PeerSummary, RuntimeCommand, RuntimeState, StatusSnapshot};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use glide_net::tls::random_hex_token_32;
use glide_proto::Edge;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Maximum allowed single JSON line length (64 KiB) for IPC requests and responses.
pub const MAX_IPC_LINE_BYTES: usize = 64 * 1024;
/// Per-operation IPC read/write timeout.
pub const IPC_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Target screen for `glide ctl switch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchDestination {
    Left,
    Right,
    Top,
    Bottom,
    Local,
}

impl SwitchDestination {
    pub const fn to_edge(self) -> Option<Edge> {
        match self {
            Self::Left => Some(Edge::Left),
            Self::Right => Some(Edge::Right),
            Self::Top => Some(Edge::Top),
            Self::Bottom => Some(Edge::Bottom),
            Self::Local => None,
        }
    }
}

/// Control command payload carried inside an [`IpcRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum IpcAction {
    Status,
    Lock,
    Unlock,
    ToggleLock,
    Switch { target: SwitchDestination },
    SendFile { path: String },
}

/// Newline-delimited JSON IPC request envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(flatten)]
    pub action: IpcAction,
}

/// Compact runtime status returned by `glide ctl status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcStatus {
    pub mode: String,
    pub status: String,
    pub active_target: String,
    pub locked: bool,
    pub peers: Vec<PeerSummary>,
    pub rtt_ms: Option<f32>,
}

impl From<StatusSnapshot> for IpcStatus {
    fn from(snap: StatusSnapshot) -> Self {
        Self {
            mode: snap.mode,
            status: snap.status,
            active_target: snap.active_target,
            locked: snap.locked,
            peers: snap.peers,
            rtt_ms: snap.rtt_ms,
        }
    }
}

/// Newline-delimited JSON IPC response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<IpcStatus>,
}

impl IpcResponse {
    pub fn ok_message(msg: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(msg.into()),
            error: None,
            status: None,
        }
    }

    pub fn ok_status(status: IpcStatus) -> Self {
        Self {
            ok: true,
            message: None,
            error: None,
            status: Some(status),
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: None,
            error: Some(msg.into()),
            status: None,
        }
    }
}

/// Endpoint descriptor persisted on Windows (or TCP-token mode) in `<config_dir>/ctl.endpoint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointFile {
    pub port: u16,
    pub token: String,
}

/// Bridge connecting the local IPC server to a running Glide GUI, CLI server, or CLI client.
pub trait IpcBackend: Send + Sync + 'static {
    fn status(&self) -> IpcStatus;
    fn max_file_bytes(&self) -> usize;
    fn send_command(&self, cmd: RuntimeCommand) -> Result<()>;
}

/// Simple [`IpcBackend`] backed directly by a [`RuntimeState`] and [`RuntimeCommand`] sender
/// (used by standalone `glide server`, `glide client`, and unit tests).
pub struct DirectIpcBackend {
    runtime: RuntimeState,
    cmd_tx: mpsc::UnboundedSender<RuntimeCommand>,
    max_file_bytes: Arc<AtomicUsize>,
}

impl DirectIpcBackend {
    pub fn new(
        runtime: RuntimeState,
        cmd_tx: mpsc::UnboundedSender<RuntimeCommand>,
        max_file_bytes: usize,
    ) -> Self {
        Self {
            runtime,
            cmd_tx,
            max_file_bytes: Arc::new(AtomicUsize::new(max_file_bytes)),
        }
    }
}

impl IpcBackend for DirectIpcBackend {
    fn status(&self) -> IpcStatus {
        IpcStatus::from(self.runtime.snapshot())
    }

    fn max_file_bytes(&self) -> usize {
        self.max_file_bytes.load(Ordering::Relaxed)
    }

    fn send_command(&self, cmd: RuntimeCommand) -> Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| anyhow!("Glide runtime is no longer running"))
    }
}

/// Constant-time byte-slice comparison for verifying IPC auth tokens.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Returns the default Unix domain socket path:
/// `$XDG_RUNTIME_DIR/glide/ctl.sock`, falling back to `<config_dir>/ctl.sock`.
#[cfg(unix)]
pub fn default_ipc_socket_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("glide").join("ctl.sock");
    }
    default_config_dir().join("ctl.sock")
}

/// Returns the default TCP endpoint descriptor path (`<config_dir>/ctl.endpoint`).
pub fn default_ipc_endpoint_path() -> PathBuf {
    default_config_dir().join("ctl.endpoint")
}

/// Handle to a running local IPC server; aborts the listener and cleans up socket/endpoint files on [`Drop`].
pub struct IpcServerHandle {
    task: tokio::task::JoinHandle<()>,
    cleanup_path: PathBuf,
    expected_token: Option<String>,
    tcp_addr: Option<SocketAddr>,
}

impl IpcServerHandle {
    /// Returns the local TCP loopback address when running in TCP+token mode.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addr
    }

    /// Returns the socket or endpoint file path associated with this IPC server.
    pub fn path(&self) -> &Path {
        &self.cleanup_path
    }
}

impl Drop for IpcServerHandle {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(ref token) = self.expected_token {
            // Only remove the endpoint file if it still belongs to this server instance.
            if let Ok(raw) = fs::read_to_string(&self.cleanup_path) {
                if let Ok(ep) = serde_json::from_str::<EndpointFile>(&raw) {
                    if constant_time_eq(ep.token.as_bytes(), token.as_bytes()) {
                        let _ = fs::remove_file(&self.cleanup_path);
                    }
                }
            }
        } else {
            let _ = fs::remove_file(&self.cleanup_path);
        }
    }
}

/// Starts the platform-default local IPC server:
/// - Unix: Unix domain socket (`0o700` dir, `0o600` socket).
/// - Windows: `127.0.0.1:0` TCP listener + 32-byte random hex auth token in `ctl.endpoint`.
pub async fn start_default_ipc(backend: Arc<dyn IpcBackend>) -> Result<IpcServerHandle> {
    #[cfg(unix)]
    {
        start_unix(default_ipc_socket_path(), backend).await
    }
    #[cfg(not(unix))]
    {
        start_tcp_with_token(default_ipc_endpoint_path(), backend).await
    }
}

/// Starts a Unix domain socket IPC server at `socket_path` with `0o700` parent dir and `0o600` socket.
#[cfg(unix)]
pub async fn start_unix(
    socket_path: PathBuf,
    backend: Arc<dyn IpcBackend>,
) -> Result<IpcServerHandle> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use tokio::net::{UnixListener, UnixStream};

    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create IPC directory {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set 0700 permissions on {}", parent.display()))?;
    }

    if let Ok(meta) = fs::symlink_metadata(&socket_path) {
        if !meta.file_type().is_socket() {
            anyhow::bail!(
                "refusing to overwrite non-socket file at {}",
                socket_path.display()
            );
        }
        // Probe whether another live Glide instance owns this socket before unlinking stale files.
        let alive = matches!(
            tokio::time::timeout(Duration::from_millis(250), UnixStream::connect(&socket_path))
                .await,
            Ok(Ok(_))
        );
        if alive {
            anyhow::bail!(
                "another Glide IPC server is already listening at {}",
                socket_path.display()
            );
        }
        fs::remove_file(&socket_path).with_context(|| {
            format!(
                "failed to remove stale IPC socket {}",
                socket_path.display()
            )
        })?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind IPC socket {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "failed to set 0600 permissions on IPC socket {}",
            socket_path.display()
        )
    })?;

    debug!(path = %socket_path.display(), "Glide local IPC server listening (Unix socket)");
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let b = Arc::clone(&backend);
                    tokio::spawn(async move {
                        if let Err(e) = handle_ipc_stream(stream, b, None).await {
                            debug!(error = %e, "IPC Unix connection ended");
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "IPC Unix accept error");
                    break;
                }
            }
        }
    });

    Ok(IpcServerHandle {
        task,
        cleanup_path: socket_path,
        expected_token: None,
        tcp_addr: None,
    })
}

/// Starts a loopback-only (`127.0.0.1:0`) TCP IPC server guarded by a 32-byte random hex auth token
/// written to `endpoint_path`.
pub async fn start_tcp_with_token(
    endpoint_path: PathBuf,
    backend: Arc<dyn IpcBackend>,
) -> Result<IpcServerHandle> {
    if let Some(parent) = endpoint_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create IPC directory {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind loopback IPC TCP listener")?;
    let addr = listener.local_addr()?;
    let token = random_hex_token_32()?;
    let ep = EndpointFile {
        port: addr.port(),
        token: token.clone(),
    };
    let encoded = serde_json::to_string_pretty(&ep)?;
    fs::write(&endpoint_path, encoded)
        .with_context(|| format!("failed to write IPC endpoint file {}", endpoint_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&endpoint_path, fs::Permissions::from_mode(0o600));
    }

    debug!(%addr, path = %endpoint_path.display(), "Glide local IPC server listening (loopback TCP)");
    let expected = Arc::new(token.clone());
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _peer)) => {
                    let b = Arc::clone(&backend);
                    let tok = Arc::clone(&expected);
                    tokio::spawn(async move {
                        if let Err(e) = handle_ipc_stream(stream, b, Some(tok)).await {
                            debug!(error = %e, "IPC TCP connection ended");
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "IPC TCP accept error");
                    break;
                }
            }
        }
    });

    Ok(IpcServerHandle {
        task,
        cleanup_path: endpoint_path,
        expected_token: Some(token),
        tcp_addr: Some(addr),
    })
}

async fn read_bounded_line<R: AsyncRead + Unpin>(reader: R, max_len: usize) -> Result<String> {
    let mut limited = BufReader::new(reader.take((max_len + 1) as u64));
    let mut buf = Vec::new();
    let n = tokio::time::timeout(IPC_IO_TIMEOUT, limited.read_until(b'\n', &mut buf))
        .await
        .map_err(|_| anyhow!("IPC read timed out"))?
        .context("IPC read failed")?;
    if n == 0 {
        anyhow::bail!("IPC connection closed before newline");
    }
    if buf.len() > max_len {
        anyhow::bail!("IPC line exceeds maximum allowed length ({max_len} bytes)");
    }
    if buf.ends_with(b"\n") {
        buf.pop();
    }
    if buf.ends_with(b"\r") {
        buf.pop();
    }
    String::from_utf8(buf).context("IPC line is not valid UTF-8")
}

async fn write_response_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &IpcResponse,
) -> Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    tokio::time::timeout(IPC_IO_TIMEOUT, async {
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| anyhow!("IPC write timed out"))?
    .context("IPC write failed")
}

async fn handle_ipc_stream<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    backend: Arc<dyn IpcBackend>,
    expected_token: Option<Arc<String>>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let line = match read_bounded_line(reader, MAX_IPC_LINE_BYTES).await {
        Ok(line) => line,
        Err(e) => {
            let _ = write_response_line(&mut writer, &IpcResponse::err(e.to_string())).await;
            return Ok(());
        }
    };

    let req: IpcRequest = match serde_json::from_str(&line) {
        Ok(req) => req,
        Err(e) => {
            write_response_line(
                &mut writer,
                &IpcResponse::err(format!("invalid IPC request: {e}")),
            )
            .await?;
            return Ok(());
        }
    };

    if let Some(ref expected) = expected_token {
        let provided = req.token.as_deref().unwrap_or("");
        if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            write_response_line(&mut writer, &IpcResponse::err("unauthorized")).await?;
            return Ok(());
        }
    }

    let resp = dispatch_action(req.action, backend).await;
    write_response_line(&mut writer, &resp).await
}

fn read_regular_file_bounded(path: &Path, max_bytes: usize) -> Result<(String, Bytes)> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("file path cannot be empty");
    }
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to inspect file '{}'", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("'{}' is not a regular file", path.display());
    }
    if meta.len() > max_bytes as u64 {
        anyhow::bail!(
            "file '{}' ({} bytes) exceeds max file transfer limit ({} bytes)",
            path.display(),
            meta.len(),
            max_bytes
        );
    }

    let file = fs::File::open(path)
        .with_context(|| format!("failed to open file '{}'", path.display()))?;
    let opened_meta = file.metadata()?;
    if !opened_meta.is_file() {
        anyhow::bail!("'{}' is not a regular file", path.display());
    }

    let mut data = Vec::with_capacity(opened_meta.len().min(max_bytes as u64) as usize);
    file.take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut data)
        .with_context(|| format!("failed to read file '{}'", path.display()))?;
    if data.len() > max_bytes {
        anyhow::bail!(
            "file '{}' grew beyond max file transfer limit ({} bytes)",
            path.display(),
            max_bytes
        );
    }

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("cannot determine filename for '{}'", path.display()))?
        .to_owned();

    Ok((filename, Bytes::from(data)))
}

async fn dispatch_action(action: IpcAction, backend: Arc<dyn IpcBackend>) -> IpcResponse {
    match action {
        IpcAction::Status => IpcResponse::ok_status(backend.status()),

        IpcAction::Lock => {
            let st = backend.status();
            if st.mode != "server" {
                return IpcResponse::err(format!(
                    "lock is only supported while running as server (current mode: {})",
                    st.mode
                ));
            }
            match backend.send_command(RuntimeCommand::SetLock(true)) {
                Ok(()) => IpcResponse::ok_message("Cursor locked"),
                Err(e) => IpcResponse::err(e.to_string()),
            }
        }

        IpcAction::Unlock => {
            let st = backend.status();
            if st.mode != "server" {
                return IpcResponse::err(format!(
                    "unlock is only supported while running as server (current mode: {})",
                    st.mode
                ));
            }
            match backend.send_command(RuntimeCommand::SetLock(false)) {
                Ok(()) => IpcResponse::ok_message("Cursor unlocked"),
                Err(e) => IpcResponse::err(e.to_string()),
            }
        }

        IpcAction::ToggleLock => {
            let st = backend.status();
            if st.mode != "server" {
                return IpcResponse::err(format!(
                    "toggle-lock is only supported while running as server (current mode: {})",
                    st.mode
                ));
            }
            match backend.send_command(RuntimeCommand::ToggleLock) {
                Ok(()) => IpcResponse::ok_message("Toggled cursor lock"),
                Err(e) => IpcResponse::err(e.to_string()),
            }
        }

        IpcAction::Switch { target } => {
            let st = backend.status();
            if st.mode != "server" {
                return IpcResponse::err(format!(
                    "switch is only supported while running as server (current mode: {})",
                    st.mode
                ));
            }
            let edge = target.to_edge();
            if let Some(e) = edge {
                if !st.peers.iter().any(|p| p.edge == e) {
                    return IpcResponse::err(format!(
                        "no connected peer on {:?} edge",
                        e
                    ));
                }
            }
            match backend.send_command(RuntimeCommand::SwitchTarget(edge)) {
                Ok(()) => match target {
                    SwitchDestination::Local => IpcResponse::ok_message("Switched cursor to local"),
                    other => IpcResponse::ok_message(format!(
                        "Switched cursor to {}",
                        format!("{other:?}").to_lowercase()
                    )),
                },
                Err(e) => IpcResponse::err(e.to_string()),
            }
        }

        IpcAction::SendFile { path } => {
            let st = backend.status();
            if st.mode != "server" && st.mode != "client" {
                return IpcResponse::err("Glide is not currently running");
            }
            let max_bytes = backend.max_file_bytes();
            let path_buf = PathBuf::from(path);
            let read_res =
                tokio::task::spawn_blocking(move || read_regular_file_bounded(&path_buf, max_bytes))
                    .await;
            let (filename, data) = match read_res {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => return IpcResponse::err(e.to_string()),
                Err(e) => return IpcResponse::err(format!("file read task failed: {e}")),
            };
            let byte_len = data.len();
            match backend.send_command(RuntimeCommand::SendFile {
                filename: filename.clone(),
                data,
            }) {
                Ok(()) => IpcResponse::ok_message(format!(
                    "Queued '{filename}' ({byte_len} bytes) for transfer"
                )),
                Err(e) => IpcResponse::err(e.to_string()),
            }
        }
    }
}

async fn exchange_stream<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    req: &IpcRequest,
) -> Result<IpcResponse> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut encoded = serde_json::to_string(req)?;
    encoded.push('\n');
    if encoded.len() > MAX_IPC_LINE_BYTES {
        anyhow::bail!(
            "IPC request exceeds maximum line length ({MAX_IPC_LINE_BYTES} bytes)"
        );
    }
    tokio::time::timeout(IPC_IO_TIMEOUT, async {
        writer.write_all(encoded.as_bytes()).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| anyhow!("IPC write timed out"))?
    .context("failed to write IPC request")?;

    let resp_line = read_bounded_line(reader, MAX_IPC_LINE_BYTES).await?;
    serde_json::from_str(&resp_line).context("failed to parse IPC JSON response")
}

/// Sends an IPC request to a Unix domain socket at `socket_path`.
#[cfg(unix)]
pub async fn send_unix_request(socket_path: &Path, req: &IpcRequest) -> Result<IpcResponse> {
    let stream = tokio::time::timeout(
        IPC_IO_TIMEOUT,
        tokio::net::UnixStream::connect(socket_path),
    )
    .await
    .map_err(|_| anyhow!("timed out connecting to Glide IPC socket"))?
    .with_context(|| {
        format!(
            "could not connect to running Glide instance at {} (is Glide running?)",
            socket_path.display()
        )
    })?;
    exchange_stream(stream, req).await
}

/// Sends an IPC request to a loopback TCP server at `addr`.
pub async fn send_tcp_request(addr: SocketAddr, req: &IpcRequest) -> Result<IpcResponse> {
    let stream = tokio::time::timeout(IPC_IO_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("timed out connecting to Glide IPC endpoint"))?
        .with_context(|| format!("could not connect to running Glide instance at {addr}"))?;
    exchange_stream(stream, req).await
}

/// Reads an `EndpointFile` at `endpoint_path` and sends an authenticated request over loopback TCP.
pub async fn send_tcp_endpoint_request(
    endpoint_path: &Path,
    action: IpcAction,
) -> Result<IpcResponse> {
    let raw = fs::read_to_string(endpoint_path).with_context(|| {
        format!(
            "could not read Glide IPC endpoint {} (is Glide running?)",
            endpoint_path.display()
        )
    })?;
    let ep: EndpointFile = serde_json::from_str(&raw)
        .with_context(|| format!("malformed IPC endpoint file {}", endpoint_path.display()))?;
    let addr = SocketAddr::from(([127, 0, 0, 1], ep.port));
    send_tcp_request(
        addr,
        &IpcRequest {
            token: Some(ep.token),
            action,
        },
    )
    .await
}

/// Sends an IPC action to the platform-default running Glide instance.
pub async fn send_default_request(action: IpcAction) -> Result<IpcResponse> {
    #[cfg(unix)]
    {
        send_unix_request(
            &default_ipc_socket_path(),
            &IpcRequest {
                token: None,
                action,
            },
        )
        .await
    }
    #[cfg(not(unix))]
    {
        send_tcp_endpoint_request(&default_ipc_endpoint_path(), action).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glide_proto::OsKind;

    fn make_test_backend(
        mode: &str,
        max_file_bytes: usize,
    ) -> (
        Arc<DirectIpcBackend>,
        RuntimeState,
        mpsc::UnboundedReceiver<RuntimeCommand>,
    ) {
        let rt = RuntimeState::new();
        rt.update(|s| {
            s.mode = mode.into();
            s.status = "Connected (1 peer(s))".into();
            s.active_target = "Local".into();
            s.locked = false;
            s.rtt_ms = Some(1.5);
            s.peers = vec![PeerSummary {
                name: "ubuntu-dev".into(),
                os: OsKind::LinuxX11,
                edge: Edge::Right,
                addr: "192.168.1.50:55555".into(),
                rtt_ms: Some(1.5),
            }];
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let backend = Arc::new(DirectIpcBackend::new(rt.clone(), tx, max_file_bytes));
        (backend, rt, rx)
    }

    #[tokio::test]
    async fn tcp_token_ipc_roundtrip_auth_status_lock_switch_send_and_oversized() {
        let (backend, rt, mut cmd_rx) = make_test_backend("server", 1024);
        let ep_path = std::env::temp_dir().join(format!(
            "glide-ipc-tcp-test-{}-{}.endpoint",
            std::process::id(),
            current_test_nonce()
        ));
        let _ = fs::remove_file(&ep_path);

        let handle = start_tcp_with_token(ep_path.clone(), backend).await.unwrap();
        let addr = handle.tcp_addr().unwrap();

        // 1. Status with valid token succeeds
        let status_resp = send_tcp_endpoint_request(&ep_path, IpcAction::Status)
            .await
            .unwrap();
        assert!(status_resp.ok);
        let st = status_resp.status.unwrap();
        assert_eq!(st.mode, "server");
        assert_eq!(st.active_target, "Local");
        assert!(!st.locked);
        assert_eq!(st.peers.len(), 1);
        assert_eq!(st.rtt_ms, Some(1.5));

        // 2. Unauthorized (missing token & wrong token) is rejected
        let missing_tok = send_tcp_request(
            addr,
            &IpcRequest {
                token: None,
                action: IpcAction::Status,
            },
        )
        .await
        .unwrap();
        assert!(!missing_tok.ok);
        assert_eq!(missing_tok.error.as_deref(), Some("unauthorized"));

        let wrong_tok = send_tcp_request(
            addr,
            &IpcRequest {
                token: Some("0".repeat(64)),
                action: IpcAction::Lock,
            },
        )
        .await
        .unwrap();
        assert!(!wrong_tok.ok);
        assert_eq!(wrong_tok.error.as_deref(), Some("unauthorized"));
        assert!(cmd_rx.try_recv().is_err());

        // 3. Lock / Unlock / ToggleLock queue live RuntimeCommands
        assert!(send_tcp_endpoint_request(&ep_path, IpcAction::Lock)
            .await
            .unwrap()
            .ok);
        assert!(matches!(cmd_rx.recv().await, Some(RuntimeCommand::SetLock(true))));
        rt.update(|s| s.locked = true);

        assert!(send_tcp_endpoint_request(&ep_path, IpcAction::Unlock)
            .await
            .unwrap()
            .ok);
        assert!(matches!(cmd_rx.recv().await, Some(RuntimeCommand::SetLock(false))));

        assert!(send_tcp_endpoint_request(&ep_path, IpcAction::ToggleLock)
            .await
            .unwrap()
            .ok);
        assert!(matches!(cmd_rx.recv().await, Some(RuntimeCommand::ToggleLock)));

        // 4. Switch to connected edge & back to local
        assert!(send_tcp_endpoint_request(
            &ep_path,
            IpcAction::Switch {
                target: SwitchDestination::Right,
            },
        )
        .await
        .unwrap()
        .ok);
        assert!(matches!(
            cmd_rx.recv().await,
            Some(RuntimeCommand::SwitchTarget(Some(Edge::Right)))
        ));

        assert!(send_tcp_endpoint_request(
            &ep_path,
            IpcAction::Switch {
                target: SwitchDestination::Local,
            },
        )
        .await
        .unwrap()
        .ok);
        assert!(matches!(
            cmd_rx.recv().await,
            Some(RuntimeCommand::SwitchTarget(None))
        ));

        // 5. SendFile validates regular file & max_file_bytes
        let tmp_file = std::env::temp_dir().join(format!(
            "glide-ipc-send-{}-{}.txt",
            std::process::id(),
            current_test_nonce()
        ));
        fs::write(&tmp_file, b"hello via glide ipc").unwrap();
        let send_resp = send_tcp_endpoint_request(
            &ep_path,
            IpcAction::SendFile {
                path: tmp_file.display().to_string(),
            },
        )
        .await
        .unwrap();
        assert!(send_resp.ok);
        match cmd_rx.recv().await {
            Some(RuntimeCommand::SendFile { filename, data }) => {
                assert!(filename.ends_with(".txt"));
                assert_eq!(&data[..], b"hello via glide ipc");
            }
            other => panic!("expected SendFile command, got {other:?}"),
        }

        // Oversized file beyond max_file_bytes (1024) is rejected
        fs::write(&tmp_file, vec![b'x'; 2048]).unwrap();
        let too_big = send_tcp_endpoint_request(
            &ep_path,
            IpcAction::SendFile {
                path: tmp_file.display().to_string(),
            },
        )
        .await
        .unwrap();
        assert!(!too_big.ok);
        assert!(too_big.error.unwrap().contains("exceeds"));
        let _ = fs::remove_file(&tmp_file);

        // 6. Oversized raw JSON line (> 64 KiB) is rejected
        let mut raw_stream = TcpStream::connect(addr).await.unwrap();
        let huge = vec![b'a'; MAX_IPC_LINE_BYTES + 128];
        let _ = raw_stream.write_all(&huge).await;
        let _ = raw_stream.write_all(b"\n").await;
        let _ = raw_stream.flush().await;
        let (r, _) = tokio::io::split(raw_stream);
        let err_line = read_bounded_line(r, MAX_IPC_LINE_BYTES).await.unwrap();
        let err_resp: IpcResponse = serde_json::from_str(&err_line).unwrap();
        assert!(!err_resp.ok);
        assert!(err_resp.error.unwrap().contains("exceeds maximum"));

        // 7. Dropping handle cleans up endpoint file
        drop(handle);
        assert!(!ep_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_ipc_roundtrip_permissions_and_cleanup() {
        use std::os::unix::fs::PermissionsExt;

        let (backend, _rt, mut cmd_rx) = make_test_backend("server", 4096);
        let dir = std::env::temp_dir().join(format!(
            "glide-ipc-unix-{}-{}",
            std::process::id(),
            current_test_nonce()
        ));
        let sock_path = dir.join("ctl.sock");
        let _ = fs::remove_dir_all(&dir);

        let handle = start_unix(sock_path.clone(), backend).await.unwrap();

        // Verify 0700 parent directory and 0600 socket permissions
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let sock_mode = fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(sock_mode, 0o600);

        let st = send_unix_request(
            &sock_path,
            &IpcRequest {
                token: None,
                action: IpcAction::Status,
            },
        )
        .await
        .unwrap();
        assert!(st.ok);
        assert_eq!(st.status.unwrap().mode, "server");

        let lock = send_unix_request(
            &sock_path,
            &IpcRequest {
                token: None,
                action: IpcAction::Lock,
            },
        )
        .await
        .unwrap();
        assert!(lock.ok);
        assert!(matches!(cmd_rx.recv().await, Some(RuntimeCommand::SetLock(true))));

        // Second bind against a live socket must refuse to clobber it
        let (backend2, _, _) = make_test_backend("server", 4096);
        assert!(start_unix(sock_path.clone(), backend2).await.is_err());

        drop(handle);
        assert!(!sock_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn constant_time_eq_checks_exact_equality() {
        assert!(constant_time_eq(b"abcdef1234", b"abcdef1234"));
        assert!(!constant_time_eq(b"abcdef1234", b"abcdef1235"));
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(constant_time_eq(b"", b""));
    }

    fn current_test_nonce() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    }
}
