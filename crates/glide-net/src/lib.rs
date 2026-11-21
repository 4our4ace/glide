//! Ultra-low-latency QUIC transport layer for Glide.
//!
//! Uses QUIC datagrams for pointer deltas and high-resolution scroll (zero TCP
//! head-of-line blocking), a dedicated reliable bidirectional stream for keystrokes,
//! buttons, and screen-switch transitions, and independent unidirectional streams
//! for bulk clipboard payloads.

pub mod discovery;
pub mod tls;

use anyhow::{anyhow, Context, Result};
use bytes::{Bytes, BytesMut};
pub use glide_proto::DEFAULT_MAX_FILE_BYTES;
use glide_proto::{
    ClipboardPayload, ControlMessage, DatagramMessage, MAX_CLIPBOARD_BYTES, MAX_CONTROL_FRAME_LEN,
};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
pub use quinn::Connection;
use quinn::{Endpoint, IdleTimeout, RecvStream, SendStream, TransportConfig};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tls::{ensure_crypto_provider, FingerprintVerifier, ServerIdentity, TrustMode, ALPN_GLIDE_V1};

/// Builds a QUIC `TransportConfig` tuned for interactive sub-millisecond LAN/Wi-Fi input.
///
/// - `initial_rtt(5ms)` avoids Quinn's default 333ms WAN RTT assumption.
/// - `keep_alive_interval(120ms)` prevents laptop Wi-Fi NICs from entering 802.11 Power Save
///   Mode (DTIM sleep), which otherwise causes 150-200ms wakeup spikes!
/// - `mtu_discovery_config(None)` avoids transient probe stalls on LANs.
fn build_transport_config() -> TransportConfig {
    let mut transport = TransportConfig::default();
    transport.initial_rtt(Duration::from_millis(5));
    transport.keep_alive_interval(Some(Duration::from_millis(120)));
    transport.mtu_discovery_config(None);
    if let Ok(timeout) = IdleTimeout::try_from(Duration::from_secs(4)) {
        transport.max_idle_timeout(Some(timeout));
    }
    transport.datagram_receive_buffer_size(Some(128 * 1024));
    transport.datagram_send_buffer_size(128 * 1024);
    transport.max_concurrent_bidi_streams(4u32.into());
    transport.max_concurrent_uni_streams(32u32.into());
    transport
}

/// Binds a UDP socket configured with DSCP EF (`0xB8` -> Wi-Fi WMM `AC_VO` low-latency queue)
/// and generous kernel buffers so input datagrams bypass router/Wi-Fi background queues.
fn bind_low_latency_udp(bind_addr: SocketAddr) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if bind_addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .context("failed to create QUIC UDP socket")?;
    let _ = sock.set_recv_buffer_size(256 * 1024);
    let _ = sock.set_send_buffer_size(256 * 1024);
    if bind_addr.is_ipv4() {
        // DSCP 46 (Expedited Forwarding) = TOS 0xB8 -> Wi-Fi WMM AC_VO low-latency priority
        let _ = sock.set_tos(0xB8);
    }
    sock.set_nonblocking(true)?;
    sock.bind(&bind_addr.into())
        .with_context(|| format!("failed to bind QUIC UDP socket on {bind_addr}"))?;
    Ok(sock.into())
}

/// QUIC server listener bound to a local UDP address.
#[derive(Clone)]
pub struct GlideServerEndpoint {
    endpoint: Endpoint,
    local_addr: SocketAddr,
    fingerprint: String,
}

impl GlideServerEndpoint {
    pub fn bind(bind_addr: SocketAddr, identity: &ServerIdentity) -> Result<Self> {
        ensure_crypto_provider();

        let mut tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![identity.cert_der.clone()],
                identity.key_der.clone_key(),
            )
            .context("failed to build rustls server config")?;
        tls_config.alpn_protocols = vec![ALPN_GLIDE_V1.to_vec()];

        let quic_crypto = QuicServerConfig::try_from(tls_config)
            .context("failed to build QUIC server crypto config")?;
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
        server_config.transport_config(Arc::new(build_transport_config()));

        let udp = bind_low_latency_udp(bind_addr)?;
        let endpoint = Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server_config),
            udp,
            Arc::new(quinn::TokioRuntime),
        )
        .with_context(|| format!("failed to initialize QUIC endpoint on {bind_addr}"))?;
        let local_addr = endpoint.local_addr()?;

        Ok(Self {
            endpoint,
            local_addr,
            fingerprint: identity.fingerprint.clone(),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Gracefully closes the QUIC listener and active connections, releasing the UDP port immediately.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"server stopped");
    }

    /// Accepts the next incoming QUIC connection handle.
    ///
    /// Returns `None` only when this server endpoint has been closed.
    pub async fn accept_incoming(&self) -> Option<quinn::Incoming> {
        self.endpoint.accept().await
    }

    /// Completes the QUIC handshake and opens the bidirectional control stream
    /// for an incoming connection returned by [`Self::accept_incoming`].
    pub async fn complete_incoming(incoming: quinn::Incoming) -> Result<GlideSession> {
        let conn = incoming.await.context("QUIC server handshake failed")?;
        let (ctrl_send, ctrl_recv) = conn
            .accept_bi()
            .await
            .context("client did not open control stream")?;
        Ok(GlideSession::new(conn, ctrl_send, ctrl_recv))
    }

    /// Accepts the next incoming QUIC connection and opens the control bidirectional stream.
    pub async fn accept(&self) -> Result<GlideSession> {
        let incoming = self
            .accept_incoming()
            .await
            .ok_or_else(|| anyhow!("QUIC server endpoint closed"))?;
        Self::complete_incoming(incoming).await
    }
}

/// Connects to a Glide QUIC server and opens the bidirectional control stream.
pub async fn connect_client(server_addr: SocketAddr, trust: TrustMode) -> Result<GlideSession> {
    ensure_crypto_provider();

    let verifier = Arc::new(FingerprintVerifier::new(trust));
    let mut tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![ALPN_GLIDE_V1.to_vec()];

    let quic_crypto = QuicClientConfig::try_from(tls_config)
        .context("failed to build QUIC client crypto config")?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(Arc::new(build_transport_config()));

    let bind_addr: SocketAddr = if server_addr.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };

    let udp = bind_low_latency_udp(bind_addr)?;
    let mut endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        udp,
        Arc::new(quinn::TokioRuntime),
    )
    .context("failed to bind QUIC client socket")?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint
        .connect(server_addr, "glide.local")
        .with_context(|| format!("failed to initiate QUIC connect to {server_addr}"))?
        .await
        .with_context(|| format!("QUIC handshake failed with {server_addr}"))?;

    let (ctrl_send, ctrl_recv) = conn
        .open_bi()
        .await
        .context("failed to open QUIC control stream")?;

    Ok(GlideSession::new(conn, ctrl_send, ctrl_recv))
}

/// Active encrypted QUIC session split cleanly into independent send/receive handles.
pub struct GlideSession {
    pub conn: Connection,
    pub ctrl_send: ControlSender,
    pub ctrl_recv: ControlReceiver,
}

impl GlideSession {
    fn new(conn: Connection, send: SendStream, recv: RecvStream) -> Self {
        Self {
            conn,
            ctrl_send: ControlSender::new(send),
            ctrl_recv: ControlReceiver::new(recv),
        }
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Returns the SHA-256 fingerprint of the peer's TLS leaf certificate, if available.
    pub fn peer_fingerprint(&self) -> Option<String> {
        let certs = self
            .conn
            .peer_identity()?
            .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()?;
        let leaf = certs.first()?;
        Some(tls::sha256_fingerprint(leaf.as_ref()))
    }

    /// Sends a mouse delta or high-res scroll immediately as an unreliable QUIC datagram.
    #[inline]
    pub fn send_datagram(&self, msg: DatagramMessage) -> Result<()> {
        self.conn
            .send_datagram(msg.encode())
            .context("failed to send QUIC datagram")
    }

    /// Receives the next unreliable QUIC datagram.
    pub async fn recv_datagram(&self) -> Result<DatagramMessage> {
        let bytes = self
            .conn
            .read_datagram()
            .await
            .context("QUIC datagram stream closed")?;
        DatagramMessage::decode(&bytes).context("malformed QUIC datagram")
    }

    /// Sends a clipboard payload over an independent unidirectional QUIC stream so
    /// large transfers never block mouse or keyboard traffic.
    pub async fn send_clipboard(&self, payload: &ClipboardPayload) -> Result<()> {
        send_clipboard_on(&self.conn, payload).await
    }

    /// Receives the next bulk payload (clipboard or file) sent over a unidirectional QUIC stream.
    pub async fn recv_bulk(&self) -> Result<glide_proto::BulkTransfer> {
        recv_bulk_on(&self.conn).await
    }

    /// Receives the next bulk payload enforcing a custom `max_file_bytes` ceiling.
    pub async fn recv_bulk_with_limit(
        &self,
        max_file_bytes: usize,
    ) -> Result<glide_proto::BulkTransfer> {
        recv_bulk_on_with_limit(&self.conn, max_file_bytes).await
    }
}

/// Sends a clipboard payload over a fresh unidirectional QUIC stream on `conn`.
pub async fn send_clipboard_on(conn: &Connection, payload: &ClipboardPayload) -> Result<()> {
    use glide_proto::BULK_KIND_CLIPBOARD;

    if payload.data.len() > MAX_CLIPBOARD_BYTES {
        return Err(anyhow!(
            "clipboard payload too large ({} bytes > max {})",
            payload.data.len(),
            MAX_CLIPBOARD_BYTES
        ));
    }

    let mut uni = conn
        .open_uni()
        .await
        .context("failed to open bulk uni stream")?;

    let mut hdr = [0u8; 1 + ClipboardPayload::HEADER_LEN];
    hdr[0] = BULK_KIND_CLIPBOARD;
    hdr[1..].copy_from_slice(&payload.encode_header());
    uni.write_all(&hdr).await?;
    uni.write_all(&payload.data).await?;
    uni.finish().context("failed finishing bulk stream")?;
    Ok(())
}

/// Sends a file payload over a fresh unidirectional QUIC stream on `conn` using `DEFAULT_MAX_FILE_BYTES`.
pub async fn send_file_on(conn: &Connection, file: &glide_proto::FilePayload) -> Result<()> {
    send_file_on_with_limit(conn, file, DEFAULT_MAX_FILE_BYTES).await
}

/// Sends a file payload over a fresh unidirectional QUIC stream enforcing `max_file_bytes`.
pub async fn send_file_on_with_limit(
    conn: &Connection,
    file: &glide_proto::FilePayload,
    max_file_bytes: usize,
) -> Result<()> {
    use glide_proto::BULK_KIND_FILE;

    let effective_limit = max_file_bytes.min(u32::MAX as usize);
    if file.data.len() > effective_limit {
        return Err(anyhow!(
            "file payload too large ({} bytes > max {})",
            file.data.len(),
            effective_limit
        ));
    }

    let mut uni = conn
        .open_uni()
        .await
        .context("failed to open bulk uni stream")?;

    let safe_name = glide_proto::sanitize_filename(&file.filename);
    let name_bytes = safe_name.as_bytes();
    let name_len = (name_bytes.len().min(u16::MAX as usize)) as u16;

    let mut hdr = Vec::with_capacity(1 + 2 + name_len as usize + 32 + 4);
    hdr.push(BULK_KIND_FILE);
    hdr.extend_from_slice(&name_len.to_be_bytes());
    hdr.extend_from_slice(&name_bytes[..name_len as usize]);
    hdr.extend_from_slice(&file.sha256);
    hdr.extend_from_slice(&(file.data.len() as u32).to_be_bytes());
    uni.write_all(&hdr).await?;
    uni.write_all(&file.data).await?;
    uni.finish().context("failed finishing bulk stream")?;
    Ok(())
}

/// Sends a cross-edge drag-and-drop file payload over a fresh unidirectional QUIC stream using `DEFAULT_MAX_FILE_BYTES`.
pub async fn send_drag_file_on(
    conn: &Connection,
    drag_file: &glide_proto::DragFilePayload,
) -> Result<()> {
    send_drag_file_on_with_limit(conn, drag_file, DEFAULT_MAX_FILE_BYTES).await
}

/// Sends a cross-edge drag-and-drop file payload over a fresh unidirectional QUIC stream enforcing `max_file_bytes`.
pub async fn send_drag_file_on_with_limit(
    conn: &Connection,
    drag_file: &glide_proto::DragFilePayload,
    max_file_bytes: usize,
) -> Result<()> {
    use glide_proto::BULK_KIND_DRAG_FILE;

    let file = &drag_file.file;
    let effective_limit = max_file_bytes.min(u32::MAX as usize);
    if file.data.len() > effective_limit {
        return Err(anyhow!(
            "file payload too large ({} bytes > max {})",
            file.data.len(),
            effective_limit
        ));
    }

    let mut uni = conn
        .open_uni()
        .await
        .context("failed to open bulk uni stream")?;

    let safe_name = glide_proto::sanitize_filename(&file.filename);
    let name_bytes = safe_name.as_bytes();
    let name_len = (name_bytes.len().min(u16::MAX as usize)) as u16;

    let mut hdr = Vec::with_capacity(1 + 8 + 2 + name_len as usize + 32 + 4);
    hdr.push(BULK_KIND_DRAG_FILE);
    hdr.extend_from_slice(&drag_file.drag_id.to_be_bytes());
    hdr.extend_from_slice(&name_len.to_be_bytes());
    hdr.extend_from_slice(&name_bytes[..name_len as usize]);
    hdr.extend_from_slice(&file.sha256);
    hdr.extend_from_slice(&(file.data.len() as u32).to_be_bytes());
    uni.write_all(&hdr).await?;
    uni.write_all(&file.data).await?;
    uni.finish().context("failed finishing bulk stream")?;
    Ok(())
}

/// Sends a clipboard or file payload over an independent unidirectional QUIC stream using `DEFAULT_MAX_FILE_BYTES`.
pub async fn send_bulk_on(conn: &Connection, transfer: &glide_proto::BulkTransfer) -> Result<()> {
    send_bulk_on_with_limit(conn, transfer, DEFAULT_MAX_FILE_BYTES).await
}

/// Sends a clipboard or file payload over an independent unidirectional QUIC stream enforcing `max_file_bytes`.
pub async fn send_bulk_on_with_limit(
    conn: &Connection,
    transfer: &glide_proto::BulkTransfer,
    max_file_bytes: usize,
) -> Result<()> {
    match transfer {
        glide_proto::BulkTransfer::Clipboard(payload) => send_clipboard_on(conn, payload).await,
        glide_proto::BulkTransfer::File(file) => {
            send_file_on_with_limit(conn, file, max_file_bytes).await
        }
        glide_proto::BulkTransfer::DragFile(drag_file) => {
            send_drag_file_on_with_limit(conn, drag_file, max_file_bytes).await
        }
    }
}

const BULK_INITIAL_ALLOC_CAP: usize = 64 * 1024;
const BULK_READ_CHUNK_SIZE: usize = 64 * 1024;

/// Reads exactly `declared_len` bytes incrementally without pre-allocating more than
/// `min(declared_len, 64 KiB)` upfront, preventing single-header memory exhaustion.
async fn read_exact_incremental<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    declared_len: usize,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    if declared_len == 0 {
        return Ok(Vec::new());
    }
    let mut data = Vec::with_capacity(declared_len.min(BULK_INITIAL_ALLOC_CAP));
    let mut chunk = vec![0u8; declared_len.min(BULK_READ_CHUNK_SIZE)];
    while data.len() < declared_len {
        let to_read = (declared_len - data.len()).min(chunk.len());
        let n = reader
            .read(&mut chunk[..to_read])
            .await
            .context("stream error while reading bulk payload")?;
        if n == 0 {
            return Err(anyhow!(
                "bulk stream ended early (truncated: expected {declared_len} bytes, got {})",
                data.len()
            ));
        }
        data.extend_from_slice(&chunk[..n]);
    }
    debug_assert_eq!(data.len(), declared_len);
    Ok(data)
}

async fn read_file_payload(
    uni: &mut RecvStream,
    max_file_bytes: usize,
) -> Result<glide_proto::FilePayload> {
    use glide_proto::{sanitize_filename, FilePayload};

    let mut name_len_buf = [0u8; 2];
    uni.read_exact(&mut name_len_buf).await?;
    let name_len = u16::from_be_bytes(name_len_buf) as usize;
    let name_raw = read_exact_incremental(uni, name_len)
        .await
        .context("failed reading file transfer name")?;
    let filename = sanitize_filename(&String::from_utf8_lossy(&name_raw));

    let mut sha256 = [0u8; 32];
    uni.read_exact(&mut sha256).await?;
    let mut len_buf = [0u8; 4];
    uni.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let effective_limit = max_file_bytes.min(u32::MAX as usize);
    if len > effective_limit {
        return Err(anyhow!(
            "file exceeds maximum size ({len} > {effective_limit})"
        ));
    }
    let data = read_exact_incremental(uni, len)
        .await
        .context("failed reading file transfer body")?;
    let actual_hash: [u8; 32] = Sha256::digest(&data).into();
    if actual_hash != sha256 {
        return Err(anyhow!("file payload SHA-256 verification failed"));
    }
    Ok(FilePayload {
        filename,
        sha256,
        data: Bytes::from(data),
    })
}

/// Reads and validates a single unidirectional bulk stream (clipboard, file, or drag file)
/// using `DEFAULT_MAX_FILE_BYTES`.
pub async fn recv_bulk_stream(uni: RecvStream) -> Result<glide_proto::BulkTransfer> {
    recv_bulk_stream_with_limit(uni, DEFAULT_MAX_FILE_BYTES).await
}

/// Reads and validates a single unidirectional bulk stream enforcing `max_file_bytes`.
pub async fn recv_bulk_stream_with_limit(
    uni: RecvStream,
    max_file_bytes: usize,
) -> Result<glide_proto::BulkTransfer> {
    let limit = std::sync::atomic::AtomicUsize::new(max_file_bytes);
    recv_bulk_stream_with_atomic_limit(uni, &limit).await
}

/// Reads and validates a single unidirectional bulk stream enforcing a live-updated
/// `AtomicUsize` ceiling read once the stream arrives (so live `UpdateMaxFileBytes` changes
/// apply immediately to the next transfer instead of using a stale pre-accept snapshot).
pub async fn recv_bulk_stream_with_atomic_limit(
    mut uni: RecvStream,
    max_file_bytes: &std::sync::atomic::AtomicUsize,
) -> Result<glide_proto::BulkTransfer> {
    use glide_proto::{
        BulkTransfer, DragFilePayload, BULK_KIND_CLIPBOARD, BULK_KIND_DRAG_FILE, BULK_KIND_FILE,
    };

    let mut kind = [0u8; 1];
    uni.read_exact(&mut kind)
        .await
        .context("failed reading bulk kind tag")?;

    match kind[0] {
        BULK_KIND_CLIPBOARD => {
            let mut hdr = [0u8; ClipboardPayload::HEADER_LEN];
            uni.read_exact(&mut hdr)
                .await
                .context("failed reading clipboard header")?;
            let (format, sha256, len) = ClipboardPayload::decode_header(&hdr)?;
            let data = read_exact_incremental(&mut uni, len)
                .await
                .context("failed reading clipboard body")?;
            let actual_hash: [u8; 32] = Sha256::digest(&data).into();
            if actual_hash != sha256 {
                return Err(anyhow!("clipboard payload SHA-256 verification failed"));
            }
            Ok(BulkTransfer::Clipboard(ClipboardPayload {
                format,
                sha256,
                data: Bytes::from(data),
            }))
        }
        BULK_KIND_FILE => {
            let limit = max_file_bytes.load(std::sync::atomic::Ordering::Relaxed);
            Ok(BulkTransfer::File(read_file_payload(&mut uni, limit).await?))
        }
        BULK_KIND_DRAG_FILE => {
            let mut id_buf = [0u8; 8];
            uni.read_exact(&mut id_buf).await?;
            let drag_id = u64::from_be_bytes(id_buf);
            let limit = max_file_bytes.load(std::sync::atomic::Ordering::Relaxed);
            let file = read_file_payload(&mut uni, limit).await?;
            Ok(BulkTransfer::DragFile(DragFilePayload { drag_id, file }))
        }
        other => Err(anyhow!("unknown bulk transfer kind: 0x{other:02X}")),
    }
}

/// Accepts and validates the next unidirectional bulk stream (clipboard, file, or drag file)
/// using `DEFAULT_MAX_FILE_BYTES`.
pub async fn recv_bulk_on(conn: &Connection) -> Result<glide_proto::BulkTransfer> {
    recv_bulk_on_with_limit(conn, DEFAULT_MAX_FILE_BYTES).await
}

/// Accepts and validates the next unidirectional bulk stream enforcing `max_file_bytes`.
pub async fn recv_bulk_on_with_limit(
    conn: &Connection,
    max_file_bytes: usize,
) -> Result<glide_proto::BulkTransfer> {
    let uni = conn.accept_uni().await.context("bulk uni stream closed")?;
    recv_bulk_stream_with_limit(uni, max_file_bytes).await
}

/// Accepts and validates the next unidirectional bulk stream enforcing a live-updated
/// `AtomicUsize` ceiling read once the stream arrives.
pub async fn recv_bulk_on_with_atomic_limit(
    conn: &Connection,
    max_file_bytes: &std::sync::atomic::AtomicUsize,
) -> Result<glide_proto::BulkTransfer> {
    let uni = conn.accept_uni().await.context("bulk uni stream closed")?;
    recv_bulk_stream_with_atomic_limit(uni, max_file_bytes).await
}

/// Accepts the next clipboard payload (for tests/backwards callers).
pub async fn recv_clipboard_on(conn: &Connection) -> Result<ClipboardPayload> {
    match recv_bulk_on(conn).await? {
        glide_proto::BulkTransfer::Clipboard(c) => Ok(c),
        glide_proto::BulkTransfer::File(_) | glide_proto::BulkTransfer::DragFile(_) => {
            Err(anyhow!("expected clipboard transfer, got file"))
        }
    }
}

/// Reliable ordered control stream writer with a reusable framing buffer.
pub struct ControlSender {
    stream: SendStream,
    buf: BytesMut,
}

impl ControlSender {
    const INITIAL_CAP: usize = 64;
    const MAX_RETAINED_CAP: usize = 4096;

    fn new(stream: SendStream) -> Self {
        Self {
            stream,
            buf: BytesMut::with_capacity(Self::INITIAL_CAP),
        }
    }

    pub async fn send(&mut self, msg: &ControlMessage) -> Result<()> {
        self.buf.clear();
        msg.encode_framed_into(&mut self.buf);
        let res = self
            .stream
            .write_all(&self.buf)
            .await
            .context("failed to write control message");
        if self.buf.capacity() > Self::MAX_RETAINED_CAP {
            self.buf = BytesMut::with_capacity(Self::INITIAL_CAP);
        }
        res
    }
}

/// Reliable ordered control stream reader with a reusable decode buffer.
pub struct ControlReceiver {
    stream: RecvStream,
    buf: Vec<u8>,
}

impl ControlReceiver {
    const INITIAL_CAP: usize = 64;
    const MAX_RETAINED_CAP: usize = 4096;

    fn new(stream: RecvStream) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(Self::INITIAL_CAP),
        }
    }

    pub async fn recv(&mut self) -> Result<ControlMessage> {
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .context("control stream closed while reading frame length")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_CONTROL_FRAME_LEN {
            return Err(anyhow!(
                "control frame length {len} exceeds maximum {MAX_CONTROL_FRAME_LEN}"
            ));
        }
        self.buf.resize(len, 0);
        self.stream
            .read_exact(&mut self.buf)
            .await
            .context("control stream closed while reading frame body")?;
        let msg =
            ControlMessage::decode_body(&self.buf).context("failed to decode control message")?;
        if self.buf.capacity() > Self::MAX_RETAINED_CAP {
            self.buf = Vec::with_capacity(Self::INITIAL_CAP);
        }
        Ok(msg)
    }
}

/// Computes SHA-256 of arbitrary bytes (helper for clipboard hashing).
pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glide_hid::HidUsage;
    use glide_proto::{ClipboardFormat, Edge, OsKind, PROTOCOL_VERSION};

    #[tokio::test]
    async fn quic_end_to_end_datagram_control_and_clipboard() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let server = GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = server.local_addr();

        let server_task = tokio::spawn(async move {
            let mut sess = server.accept().await.unwrap();
            let hello = sess.ctrl_recv.recv().await.unwrap();
            assert!(matches!(hello, ControlMessage::Hello { .. }));

            sess.ctrl_send
                .send(&ControlMessage::Key {
                    usage: HidUsage::ALT_RIGHT,
                    pressed: true,
                })
                .await
                .unwrap();

            sess.send_datagram(DatagramMessage::MouseMove {
                seq: 7,
                dx: 12.5,
                dy: -4.25,
            })
            .unwrap();

            let text = Bytes::from_static("Grüße aus Deutschland: äöüß€@|".as_bytes());
            let clip = ClipboardPayload {
                format: ClipboardFormat::TextUtf8,
                sha256: sha256_bytes(&text),
                data: text,
            };
            sess.send_clipboard(&clip).await.unwrap();

            let file_bytes = Bytes::from_static(b"hello file transfer");
            let file = glide_proto::FilePayload {
                filename: "notes.txt".into(),
                sha256: sha256_bytes(&file_bytes),
                data: file_bytes,
            };
            send_file_on(&sess.conn, &file).await.unwrap();

            let drag_bytes = Bytes::from_static(b"cross-edge dragged payload");
            let drag_file = glide_proto::DragFilePayload {
                drag_id: 99,
                file: glide_proto::FilePayload {
                    filename: "Büro report.pdf".into(),
                    sha256: sha256_bytes(&drag_bytes),
                    data: drag_bytes,
                },
            };
            send_drag_file_on(&sess.conn, &drag_file).await.unwrap();
            tokio::time::sleep(Duration::from_millis(120)).await;
        });

        let mut client = connect_client(addr, TrustMode::Pinned(fp)).await.unwrap();
        client
            .ctrl_send
            .send(&ControlMessage::Hello {
                version: PROTOCOL_VERSION,
                peer_name: "ubuntu-test".into(),
                edge_on_server: Edge::Right,
                os: OsKind::LinuxX11,
            })
            .await
            .unwrap();

        let key = client.ctrl_recv.recv().await.unwrap();
        assert_eq!(
            key,
            ControlMessage::Key {
                usage: HidUsage::ALT_RIGHT,
                pressed: true
            }
        );

        let dgram = client.recv_datagram().await.unwrap();
        assert_eq!(
            dgram,
            DatagramMessage::MouseMove {
                seq: 7,
                dx: 12.5,
                dy: -4.25
            }
        );

        let mut got_clip = false;
        let mut got_file = false;
        let mut got_drag_file = false;
        for _ in 0..3 {
            match client.recv_bulk().await.unwrap() {
                glide_proto::BulkTransfer::Clipboard(clip) => {
                    assert_eq!(clip.format, ClipboardFormat::TextUtf8);
                    assert_eq!(
                        std::str::from_utf8(&clip.data).unwrap(),
                        "Grüße aus Deutschland: äöüß€@|"
                    );
                    got_clip = true;
                }
                glide_proto::BulkTransfer::File(f) => {
                    assert_eq!(f.filename, "notes.txt");
                    assert_eq!(f.data.as_ref(), b"hello file transfer");
                    got_file = true;
                }
                glide_proto::BulkTransfer::DragFile(df) => {
                    assert_eq!(df.drag_id, 99);
                    assert_eq!(df.file.filename, "Büro report.pdf");
                    assert_eq!(df.file.data.as_ref(), b"cross-edge dragged payload");
                    got_drag_file = true;
                }
            }
        }
        assert!(got_clip && got_file && got_drag_file);

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn server_endpoint_close_allows_immediate_rebind() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let ep1 = GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = ep1.local_addr();
        ep1.close();
        drop(ep1);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ep2 = GlideServerEndpoint::bind(addr, &identity)
            .expect("re-binding same UDP addr after close() must succeed");
        assert_eq!(ep2.local_addr(), addr);
        ep2.close();
    }

    #[tokio::test]
    async fn read_exact_incremental_reads_chunks_and_rejects_truncated_large_header() {
        // Multi-chunk exact read (> 64 KiB)
        let payload: Vec<u8> = (0..(150 * 1024)).map(|i| (i % 251) as u8).collect();
        let mut cursor = std::io::Cursor::new(payload.clone());
        let got = read_exact_incremental(&mut cursor, payload.len())
            .await
            .unwrap();
        assert_eq!(got, payload);

        // Zero-length read
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_exact_incremental(&mut empty, 0).await.unwrap().is_empty());

        // Malicious 256 MiB declared length with only 16 bytes on the wire fails fast without OOM
        let mut tiny = std::io::Cursor::new(b"0123456789abcdef".as_slice());
        let err = read_exact_incremental(&mut tiny, glide_proto::MAX_FILE_BYTES)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("truncated"));
    }

    #[tokio::test]
    async fn custom_file_size_limit_enforced_on_send_and_recv() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let server = GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = server.local_addr();

        let server_task = tokio::spawn(async move {
            let mut sess = server.accept().await.unwrap();
            let _hello = sess.ctrl_recv.recv().await.unwrap();

            // 1. Sender refuses payload exceeding its custom 32-byte limit without opening a stream.
            let oversized = Bytes::from(vec![0xAA; 64]);
            let big_file = glide_proto::FilePayload {
                filename: "too-big.bin".into(),
                sha256: sha256_bytes(&oversized),
                data: oversized,
            };
            let err = send_file_on_with_limit(&sess.conn, &big_file, 32)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("file payload too large"));

            // 2. Sender allows 64 bytes with a 128-byte limit, while receiver only allows 32 bytes.
            send_file_on_with_limit(&sess.conn, &big_file, 128)
                .await
                .unwrap();

            // 3. Sender sends a 24-byte file that fits within the receiver's 32-byte limit.
            let ok_data = Bytes::from(vec![0x55; 24]);
            let ok_file = glide_proto::FilePayload {
                filename: "fits.bin".into(),
                sha256: sha256_bytes(&ok_data),
                data: ok_data,
            };
            send_file_on_with_limit(&sess.conn, &ok_file, 32)
                .await
                .unwrap();

            tokio::time::sleep(Duration::from_millis(120)).await;
        });

        let mut client = connect_client(addr, TrustMode::Pinned(fp)).await.unwrap();
        client
            .ctrl_send
            .send(&ControlMessage::Hello {
                version: PROTOCOL_VERSION,
                peer_name: "limit-test".into(),
                edge_on_server: Edge::Right,
                os: OsKind::LinuxX11,
            })
            .await
            .unwrap();

        // First stream on the wire is 64 bytes -> rejected under 32-byte receiver limit.
        let recv_err = client.recv_bulk_with_limit(32).await.unwrap_err();
        assert!(recv_err.to_string().contains("exceeds maximum size"));

        // Second stream on the wire is 24 bytes -> accepted under 32-byte receiver limit.
        match client.recv_bulk_with_limit(32).await.unwrap() {
            glide_proto::BulkTransfer::File(f) => {
                assert_eq!(f.filename, "fits.bin");
                assert_eq!(f.data.len(), 24);
            }
            other => panic!("expected file transfer, got {other:?}"),
        }

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_huge_header_does_not_oom_and_multichunk_transfer_succeeds() {
        let identity = ServerIdentity::generate_ephemeral().unwrap();
        let fp = identity.fingerprint.clone();
        let server = GlideServerEndpoint::bind("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = server.local_addr();

        let server_task = tokio::spawn(async move {
            let mut sess = server.accept().await.unwrap();
            let _hello = sess.ctrl_recv.recv().await.unwrap();

            // 1. Corrupt/truncated stream claiming u32::MAX (~4 GiB) but sending only 16 bytes
            let mut uni = sess.conn.open_uni().await.unwrap();
            uni.write_all(&[glide_proto::BULK_KIND_FILE]).await.unwrap();
            let name = b"fake-4gb.bin";
            uni.write_all(&(name.len() as u16).to_be_bytes())
                .await
                .unwrap();
            uni.write_all(name).await.unwrap();
            uni.write_all(&[0u8; 32]).await.unwrap();
            uni.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
            uni.write_all(&[0xAB; 16]).await.unwrap();
            uni.finish().unwrap();

            // 2. Real multi-chunk (> 64 KiB) transfer of 200 KiB succeeds across incremental chunks
            let payload_data: Vec<u8> = (0..(200 * 1024)).map(|i| (i % 251) as u8).collect();
            let real_file = glide_proto::FilePayload {
                filename: "multichunk.bin".into(),
                sha256: sha256_bytes(&payload_data),
                data: Bytes::from(payload_data),
            };
            send_file_on_with_limit(&sess.conn, &real_file, 1024 * 1024)
                .await
                .unwrap();

            tokio::time::sleep(Duration::from_millis(120)).await;
        });

        let mut client = connect_client(addr, TrustMode::Pinned(fp)).await.unwrap();
        client
            .ctrl_send
            .send(&ControlMessage::Hello {
                version: PROTOCOL_VERSION,
                peer_name: "oom-guard-test".into(),
                edge_on_server: Edge::Right,
                os: OsKind::LinuxX11,
            })
            .await
            .unwrap();

        // Allow up to 4 GiB so the length check passes and incremental reading catches the truncation
        let err = client
            .recv_bulk_with_limit(u32::MAX as usize)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("bulk stream ended early"),
            "unexpected error: {err:#}"
        );

        match client.recv_bulk_with_limit(1024 * 1024).await.unwrap() {
            glide_proto::BulkTransfer::File(f) => {
                assert_eq!(f.filename, "multichunk.bin");
                assert_eq!(f.data.len(), 200 * 1024);
                assert_eq!(f.data[0], 0);
                assert_eq!(f.data[251], 0);
                assert_eq!(f.data[252], 1);
            }
            other => panic!("expected multi-chunk file, got {other:?}"),
        }

        server_task.await.unwrap();
    }
}
