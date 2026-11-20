//! Platform-agnostic Glide Client state machine.
//!
//! Supports zero-config LocalSend-style LAN auto-discovery (no IP fiddling!),
//! automatic reconnect, per-client sensitivity scaling, bidirectional clipboard
//! synchronization, and guaranteed release of all held keys on leave/disconnect.

use crate::config::{ClientConfig, DEFAULT_QUIC_PORT};
use anyhow::{anyhow, Context, Result};
use glide_backend::{ClipboardHandle, InputInjector};
use glide_net::discovery::discover_servers;
use glide_net::tls::TrustMode;
use glide_net::{connect_client, recv_clipboard_on, send_clipboard_on};
use glide_proto::{ControlMessage, DatagramMessage, PROTOCOL_VERSION};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::lookup_host;
use tracing::{debug, info, warn};

/// Resolves the target server socket address either from explicit `config.server`
/// or by auto-discovering a running Glide server on the local network.
async fn resolve_server_addr(config: &ClientConfig) -> Result<(SocketAddr, Option<String>)> {
    if let Some(ref raw) = config.server {
        let with_port = if raw.contains(':') {
            raw.clone()
        } else {
            format!("{raw}:{DEFAULT_QUIC_PORT}")
        };
        let mut addrs = lookup_host(&with_port)
            .await
            .with_context(|| format!("failed to resolve server address '{with_port}'"))?;
        let addr = addrs
            .next()
            .ok_or_else(|| anyhow!("no IP address resolved for '{with_port}'"))?;
        return Ok((addr, None));
    }

    info!(
        discovery_port = config.discovery_port,
        filter = ?config.server_name,
        "Searching LAN for a running Glide server..."
    );
    loop {
        let found = discover_servers(
            config.discovery_port,
            Duration::from_secs(2),
            config.server_name.as_deref(),
            true,
        )
        .await?;
        if let Some(srv) = found.into_iter().next() {
            info!(
                name = %srv.name,
                os = %srv.os,
                addr = %srv.addr,
                fingerprint = %srv.fingerprint,
                "Auto-discovered Glide server on LAN"
            );
            return Ok((srv.addr, Some(srv.name)));
        }
    }
}

fn build_trust_mode(config: &ClientConfig) -> TrustMode {
    if config.insecure_skip_verify {
        TrustMode::InsecureSkipVerify
    } else if let Some(ref fp) = config.trusted_fingerprint {
        TrustMode::Pinned(fp.clone())
    } else {
        TrustMode::Tofu {
            store_path: config.resolved_tofu_path(),
        }
    }
}

/// Runs a single connected client session until disconnect or error.
pub async fn run_client_session(
    config: &ClientConfig,
    server_addr: SocketAddr,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
) -> Result<()> {
    let trust = build_trust_mode(config);
    let mut session = connect_client(server_addr, trust).await?;

    // Handshake
    session
        .ctrl_send
        .send(&ControlMessage::Hello {
            version: PROTOCOL_VERSION,
            peer_name: config.name.clone(),
            edge_on_server: config.edge,
            os: injector.os_kind(),
        })
        .await?;

    let welcome = tokio::time::timeout(Duration::from_secs(5), session.ctrl_recv.recv()).await??;
    match welcome {
        ControlMessage::Welcome {
            server_name,
            accepted: true,
            ..
        } => {
            info!(
                server = %server_name,
                addr = %server_addr,
                edge = ?config.edge,
                sensitivity = config.sensitivity,
                "Connected to Glide server"
            );
        }
        ControlMessage::Welcome {
            accepted: false,
            reason,
            ..
        } => {
            return Err(anyhow!("server rejected connection: {reason}"));
        }
        other => {
            return Err(anyhow!("unexpected handshake response: {other:?}"));
        }
    }

    let return_edge = config.edge.opposite();
    let sensitivity = config.sensitivity.clamp(0.05, 10.0);
    let mut focused = false;
    let mut last_seq: Option<u32> = None;

    let conn = session.conn;
    let mut ctrl_send = session.ctrl_send;
    let mut ctrl_recv = session.ctrl_recv;

    let session_result: Result<()> = loop {
        tokio::select! {
            dgram = conn.read_datagram() => {
                let bytes = match dgram {
                    Ok(b) => b,
                    Err(e) => break Err(anyhow!("datagram stream closed: {e}")),
                };
                if !focused {
                    continue;
                }
                let Ok(msg) = DatagramMessage::decode(&bytes) else {
                    continue;
                };
                match msg {
                    DatagramMessage::MouseMove { seq, dx, dy } => {
                        // Drop only genuinely ancient replayed packets while preserving
                        // slightly reordered UDP packets so additive deltas are never lost.
                        if let Some(prev) = last_seq {
                            let diff = seq.wrapping_sub(prev) as i32;
                            if diff < -32 {
                                continue;
                            }
                            if diff > 0 {
                                last_seq = Some(seq);
                            }
                        } else {
                            last_seq = Some(seq);
                        }

                        let sdx = dx * sensitivity;
                        let sdy = dy * sensitivity;
                        match injector.move_relative(
                            sdx,
                            sdy,
                            Some(return_edge),
                            config.edge_threshold_px,
                        ) {
                            Ok(Some((edge, normalized_pos))) => {
                                debug!(?edge, normalized_pos, "Cursor crossed return edge; handing focus back to server");
                                focused = false;
                                let _ = injector.release_all();
                                if let Err(e) = ctrl_send
                                    .send(&ControlMessage::Leave {
                                        edge,
                                        normalized_pos,
                                    })
                                    .await
                                {
                                    break Err(e);
                                }
                            }
                            Ok(None) => {}
                            Err(e) => warn!(error = %e, "Failed injecting relative mouse motion"),
                        }
                    }
                    DatagramMessage::MouseScroll { dx_120, dy_120, .. } => {
                        if let Err(e) = injector.scroll(dx_120, dy_120) {
                            warn!(error = %e, "Failed injecting scroll");
                        }
                    }
                }
            }

            ctrl = ctrl_recv.recv() => {
                let msg = match ctrl {
                    Ok(m) => m,
                    Err(e) => break Err(e),
                };
                match msg {
                    ControlMessage::Enter { edge, normalized_pos } => {
                        focused = true;
                        last_seq = None;
                        if let Err(e) = injector.enter(edge, normalized_pos) {
                            warn!(error = %e, "Failed positioning cursor on Enter");
                        }
                    }
                    ControlMessage::Leave { .. } | ControlMessage::ReleaseAll => {
                        focused = false;
                        let _ = injector.release_all();
                    }
                    ControlMessage::Key { usage, pressed } => {
                        if focused {
                            if let Err(e) = injector.key(usage, pressed) {
                                warn!(?usage, error = %e, "Failed injecting key");
                            }
                        }
                    }
                    ControlMessage::Button { button, pressed } => {
                        if focused {
                            if let Err(e) = injector.button(button, pressed) {
                                warn!(?button, error = %e, "Failed injecting mouse button");
                            }
                        }
                    }
                    ControlMessage::ModifierSync { .. } => {}
                    ControlMessage::Ping { ts_us } => {
                        let _ = ctrl_send.send(&ControlMessage::Pong { ts_us }).await;
                    }
                    _ => {}
                }
            }

            remote_clip = recv_clipboard_on(&conn) => {
                match remote_clip {
                    Ok(payload) => {
                        if config.clipboard {
                            if let Some(ref clip) = clipboard {
                                debug!(format = ?payload.format, len = payload.data.len(), "Applying clipboard from server");
                                if let Err(e) = clip.backend.set(payload) {
                                    warn!(error = %e, "Failed setting local clipboard");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "Remote clipboard uni-stream loop ended");
                    }
                }
            }

            local_clip = async {
                match clipboard.as_mut() {
                    Some(c) => c.local_changes.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(payload) = local_clip {
                    if config.clipboard {
                        let c = conn.clone();
                        tokio::spawn(async move {
                            if let Err(e) = send_clipboard_on(&c, &payload).await {
                                debug!(error = %e, "Failed sending local clipboard to server");
                            }
                        });
                    }
                }
            }
        }
    };

    // Safety guarantee: NEVER leave any key or button stuck when disconnecting!
    let _ = injector.release_all();
    session_result
}

/// Runs the native Glide client with automatic LAN discovery and seamless reconnect.
pub async fn run_native_client(config: ClientConfig) -> Result<()> {
    glide_backend::init_platform();
    let mut injector = glide_backend::create_injector()?;
    let mut clipboard = if config.clipboard {
        match glide_backend::start_clipboard() {
            Ok(c) => Some(c),
            Err(e) => {
                warn!(error = %e, "Clipboard backend unavailable; continuing without clipboard sync");
                None
            }
        }
    } else {
        None
    };

    let mut backoff = Duration::from_millis(500);
    loop {
        match resolve_server_addr(&config).await {
            Ok((addr, _)) => {
                match run_client_session(&config, addr, injector.as_mut(), &mut clipboard).await {
                    Ok(()) => {
                        info!("Session ended cleanly");
                        backoff = Duration::from_millis(500);
                    }
                    Err(e) => {
                        warn!(error = %e, "Disconnected from server; reconnecting shortly...");
                        let _ = injector.release_all();
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Could not resolve server; retrying...");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(3));
    }
}
