//! Ultra-low-latency QUIC transport layer for Glide.
//!
//! Uses QUIC datagrams for pointer deltas and high-resolution scroll (zero TCP
//! head-of-line blocking), a dedicated reliable bidirectional stream for keystrokes,
//! buttons, and screen-switch transitions, and independent unidirectional streams
//! for bulk clipboard payloads.

pub mod discovery;
pub mod tls;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
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

/// Builds a QUIC `TransportConfig` tuned for interactive sub-millisecond LAN input.
fn build_transport_config() -> TransportConfig {
    let mut transport = TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_millis(750)));
    if let Ok(timeout) = IdleTimeout::try_from(Duration::from_secs(4)) {
        transport.max_idle_timeout(Some(timeout));
    }
    transport.datagram_receive_buffer_size(Some(64 * 1024));
    transport.datagram_send_buffer_size(64 * 1024);
    transport.max_concurrent_bidi_streams(4u32.into());
    transport.max_concurrent_uni_streams(32u32.into());
    transport
}

/// QUIC server listener bound to a local UDP address.
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

        let endpoint = Endpoint::server(server_config, bind_addr)
            .with_context(|| format!("failed to bind QUIC UDP socket on {bind_addr}"))?;
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

    /// Accepts the next incoming QUIC connection and opens the control bidirectional stream.
    pub async fn accept(&self) -> Result<GlideSession> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| anyhow!("QUIC server endpoint closed"))?;
        let conn = incoming.await.context("QUIC server handshake failed")?;
        let (ctrl_send, ctrl_recv) = conn
            .accept_bi()
            .await
            .context("client did not open control stream")?;
        Ok(GlideSession::new(conn, ctrl_send, ctrl_recv))
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

    let mut endpoint = Endpoint::client(bind_addr).context("failed to bind QUIC client socket")?;
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
            ctrl_send: ControlSender { stream: send },
            ctrl_recv: ControlReceiver { stream: recv },
        }
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.conn.remote_address()
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

    /// Receives the next clipboard payload sent over a unidirectional QUIC stream.
    pub async fn recv_clipboard(&self) -> Result<ClipboardPayload> {
        recv_clipboard_on(&self.conn).await
    }
}

/// Sends a clipboard payload over a fresh unidirectional QUIC stream on `conn`.
pub async fn send_clipboard_on(conn: &Connection, payload: &ClipboardPayload) -> Result<()> {
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
        .context("failed to open clipboard uni stream")?;
    let hdr = payload.encode_header();
    uni.write_all(&hdr)
        .await
        .context("failed writing clipboard header")?;
    uni.write_all(&payload.data)
        .await
        .context("failed writing clipboard body")?;
    uni.finish().context("failed finishing clipboard stream")?;
    Ok(())
}

/// Accepts and validates the next unidirectional clipboard stream on `conn`.
pub async fn recv_clipboard_on(conn: &Connection) -> Result<ClipboardPayload> {
    let mut uni = conn
        .accept_uni()
        .await
        .context("clipboard uni stream closed")?;
    let mut hdr = [0u8; ClipboardPayload::HEADER_LEN];
    uni.read_exact(&mut hdr)
        .await
        .context("failed reading clipboard header")?;
    let (format, sha256, len) = ClipboardPayload::decode_header(&hdr)?;
    let mut data = vec![0u8; len];
    uni.read_exact(&mut data)
        .await
        .context("failed reading clipboard body")?;

    let actual_hash: [u8; 32] = Sha256::digest(&data).into();
    if actual_hash != sha256 {
        return Err(anyhow!("clipboard payload SHA-256 verification failed"));
    }

    Ok(ClipboardPayload {
        format,
        sha256,
        data: Bytes::from(data),
    })
}

/// Reliable ordered control stream writer.
pub struct ControlSender {
    stream: SendStream,
}

impl ControlSender {
    pub async fn send(&mut self, msg: &ControlMessage) -> Result<()> {
        let framed = msg.encode_framed();
        self.stream
            .write_all(&framed)
            .await
            .context("failed to write control message")?;
        Ok(())
    }
}

/// Reliable ordered control stream reader.
pub struct ControlReceiver {
    stream: RecvStream,
}

impl ControlReceiver {
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
        let mut body = vec![0u8; len];
        self.stream
            .read_exact(&mut body)
            .await
            .context("control stream closed while reading frame body")?;
        ControlMessage::decode_body(&body).context("failed to decode control message")
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
            // Keep connection alive briefly while client reads
            tokio::time::sleep(Duration::from_millis(100)).await;
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

        let clip = client.recv_clipboard().await.unwrap();
        assert_eq!(clip.format, ClipboardFormat::TextUtf8);
        assert_eq!(
            std::str::from_utf8(&clip.data).unwrap(),
            "Grüße aus Deutschland: äöüß€@|"
        );

        server_task.await.unwrap();
    }
}
