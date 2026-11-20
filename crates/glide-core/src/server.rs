//! Platform-agnostic Glide Server state machine.
//!
//! Manages multiple connected clients (one per outer screen edge), coalesces
//! high-polling-rate mouse deltas into QUIC datagrams, forwards physical USB HID
//! scancodes over the reliable control stream, and relays bidirectional clipboard
//! updates over independent unidirectional QUIC streams.

use crate::config::ServerConfig;
use anyhow::Result;
use glide_backend::{CaptureEvent, CaptureHandle, ClipboardHandle};
use glide_hid::ModifierMask;
use glide_net::discovery::spawn_server_beacon;
use glide_net::tls::ServerIdentity;
use glide_net::{
    recv_clipboard_on, send_clipboard_on, Connection, GlideServerEndpoint, GlideSession,
};
use glide_proto::{
    ClipboardPayload, ControlMessage, DatagramMessage, Edge, OsKind, PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

static NEXT_PEER_ID: AtomicU64 = AtomicU64::new(1);

struct ConnectedPeer {
    id: u64,
    name: String,
    edge: Edge,
    os: OsKind,
    conn: Connection,
    ctrl_tx: mpsc::UnboundedSender<ControlMessage>,
}

enum PeerEvent {
    Connected {
        peer: ConnectedPeer,
    },
    Control {
        peer_id: u64,
        msg: ControlMessage,
    },
    Clipboard {
        peer_id: u64,
        payload: ClipboardPayload,
    },
    Disconnected {
        peer_id: u64,
        reason: String,
    },
}

/// Runs the Glide server with pre-constructed OS (or mock) backends and a pre-bound QUIC endpoint.
pub async fn run_server_with_backends(
    config: ServerConfig,
    endpoint: GlideServerEndpoint,
    capture: CaptureHandle,
    mut clipboard: Option<ClipboardHandle>,
    os_label: &str,
) -> Result<()> {
    let local_addr: SocketAddr = endpoint.local_addr();
    let fingerprint = endpoint.fingerprint().to_string();

    info!(
        name = %config.name,
        %local_addr,
        %fingerprint,
        escape_key = %config.parsed_escape_key(),
        "Glide server listening"
    );

    let _beacon = if config.discovery {
        match spawn_server_beacon(
            config.discovery_port,
            config.name.clone(),
            local_addr.port(),
            fingerprint.clone(),
            os_label.to_string(),
        ) {
            Ok(h) => {
                info!(
                    port = config.discovery_port,
                    "LAN auto-discovery enabled (clients can connect without an IP)"
                );
                Some(h)
            }
            Err(e) => {
                warn!(error = %e, "LAN auto-discovery failed to bind; static IP mode still active");
                None
            }
        }
    } else {
        None
    };

    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<PeerEvent>();

    // Spawn QUIC accept loop
    let server_name = config.name.clone();
    let accept_tx = peer_tx.clone();
    let accept_handle = tokio::spawn(async move {
        loop {
            match endpoint.accept().await {
                Ok(session) => {
                    let sname = server_name.clone();
                    let tx = accept_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_incoming_session(session, sname, tx).await {
                            warn!(error = %e, "Incoming client handshake failed");
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "QUIC accept loop ended");
                    break;
                }
            }
        }
    });

    let mut peers: HashMap<u64, ConnectedPeer> = HashMap::new();
    let mut edge_to_peer: HashMap<Edge, u64> = HashMap::new();
    let mut active_remote: Option<u64> = None;
    let mut active_edge: Option<Edge> = None;
    let mut remote_modifiers = ModifierMask::EMPTY;

    let mut seq: u32 = 0;
    let mut pending_dx: f32 = 0.0;
    let mut pending_dy: f32 = 0.0;
    let coalesce_dur = Duration::from_micros(config.motion_coalesce_us);
    let mut coalesce_armed = false;
    let coalesce_sleep = tokio::time::sleep(Duration::from_secs(86_400));
    tokio::pin!(coalesce_sleep);

    let mut capture_events = capture.events;
    let capture_ctrl = capture.control;

    let flush_pending_motion = |peers: &HashMap<u64, ConnectedPeer>,
                                active_remote: Option<u64>,
                                pending_dx: &mut f32,
                                pending_dy: &mut f32,
                                seq: &mut u32,
                                coalesce_armed: &mut bool| {
        if *pending_dx == 0.0 && *pending_dy == 0.0 {
            *coalesce_armed = false;
            return;
        }
        if let Some(pid) = active_remote {
            if let Some(peer) = peers.get(&pid) {
                *seq = seq.wrapping_add(1);
                let msg = DatagramMessage::MouseMove {
                    seq: *seq,
                    dx: *pending_dx,
                    dy: *pending_dy,
                };
                let _ = peer.conn.send_datagram(msg.encode());
            }
        }
        *pending_dx = 0.0;
        *pending_dy = 0.0;
        *coalesce_armed = false;
    };

    loop {
        tokio::select! {
            _ = &mut coalesce_sleep, if coalesce_armed => {
                flush_pending_motion(
                    &peers,
                    active_remote,
                    &mut pending_dx,
                    &mut pending_dy,
                    &mut seq,
                    &mut coalesce_armed,
                );
            }

            ev = capture_events.recv() => {
                let Some(ev) = ev else {
                    warn!("OS capture event channel closed; stopping server");
                    break;
                };

                match ev {
                    CaptureEvent::EdgeCrossed { edge, normalized_pos } => {
                        if let Some(&peer_id) = edge_to_peer.get(&edge) {
                            if let Some(peer) = peers.get(&peer_id) {
                                info!(peer = %peer.name, ?edge, normalized_pos, "Switching focus to client");
                                active_remote = Some(peer_id);
                                active_edge = Some(edge);
                                remote_modifiers = ModifierMask::EMPTY;
                                let _ = capture_ctrl.enter_remote();
                                let _ = peer.ctrl_tx.send(ControlMessage::Enter {
                                    edge: edge.opposite(),
                                    normalized_pos,
                                });
                            }
                        } else {
                            // No peer connected on this edge; make sure capture stays local
                            let _ = capture_ctrl.leave_remote(edge, normalized_pos);
                        }
                    }

                    CaptureEvent::MouseDelta { dx, dy } => {
                        if active_remote.is_some() {
                            if config.motion_coalesce_us == 0 {
                                if let Some(pid) = active_remote {
                                    if let Some(peer) = peers.get(&pid) {
                                        seq = seq.wrapping_add(1);
                                        let _ = peer.conn.send_datagram(
                                            DatagramMessage::MouseMove { seq, dx, dy }.encode()
                                        );
                                    }
                                }
                            } else {
                                pending_dx += dx;
                                pending_dy += dy;
                                if !coalesce_armed {
                                    coalesce_armed = true;
                                    coalesce_sleep.as_mut().reset(tokio::time::Instant::now() + coalesce_dur);
                                }
                            }
                        }
                    }

                    CaptureEvent::MouseScroll { dx_120, dy_120 } => {
                        flush_pending_motion(
                            &peers,
                            active_remote,
                            &mut pending_dx,
                            &mut pending_dy,
                            &mut seq,
                            &mut coalesce_armed,
                        );
                        if let Some(pid) = active_remote {
                            if let Some(peer) = peers.get(&pid) {
                                seq = seq.wrapping_add(1);
                                let _ = peer.conn.send_datagram(
                                    DatagramMessage::MouseScroll { seq, dx_120, dy_120 }.encode()
                                );
                            }
                        }
                    }

                    CaptureEvent::MouseButton { button, pressed } => {
                        flush_pending_motion(
                            &peers,
                            active_remote,
                            &mut pending_dx,
                            &mut pending_dy,
                            &mut seq,
                            &mut coalesce_armed,
                        );
                        if let Some(pid) = active_remote {
                            if let Some(peer) = peers.get(&pid) {
                                let _ = peer.ctrl_tx.send(ControlMessage::Button { button, pressed });
                            }
                        }
                    }

                    CaptureEvent::Key { usage, pressed } => {
                        flush_pending_motion(
                            &peers,
                            active_remote,
                            &mut pending_dx,
                            &mut pending_dy,
                            &mut seq,
                            &mut coalesce_armed,
                        );
                        if let Some(pid) = active_remote {
                            if let Some(peer) = peers.get(&pid) {
                                remote_modifiers.update(usage, pressed);
                                let _ = peer.ctrl_tx.send(ControlMessage::Key { usage, pressed });
                            }
                        }
                    }

                    CaptureEvent::EscapeHotkey => {
                        if let Some(pid) = active_remote.take() {
                            if let Some(peer) = peers.get(&pid) {
                                info!(peer = %peer.name, "Escape hotkey pressed; returning focus to server");
                                let _ = peer.ctrl_tx.send(ControlMessage::ReleaseAll);
                                let _ = peer.ctrl_tx.send(ControlMessage::Leave {
                                    edge: peer.edge.opposite(),
                                    normalized_pos: 0.5,
                                });
                            }
                        }
                        if let Some(edge) = active_edge.take() {
                            let _ = capture_ctrl.leave_remote(edge, 0.5);
                        }
                        remote_modifiers = ModifierMask::EMPTY;
                        pending_dx = 0.0;
                        pending_dy = 0.0;
                        coalesce_armed = false;
                    }
                }
            }

            pe = peer_rx.recv() => {
                let Some(pe) = pe else { break };
                match pe {
                    PeerEvent::Connected { peer } => {
                        // Replace any stale peer on the same edge cleanly
                        if let Some(old_id) = edge_to_peer.insert(peer.edge, peer.id) {
                            if let Some(old_peer) = peers.remove(&old_id) {
                                info!(old = %old_peer.name, edge = ?peer.edge, "Replacing previous peer on edge");
                                old_peer.conn.close(0u32.into(), b"replaced");
                            }
                            if active_remote == Some(old_id) {
                                active_remote = None;
                                if let Some(edge) = active_edge.take() {
                                    let _ = capture_ctrl.leave_remote(edge, 0.5);
                                }
                            }
                        }
                        info!(
                            peer = %peer.name,
                            os = ?peer.os,
                            edge = ?peer.edge,
                            remote = %peer.conn.remote_address(),
                            "Client connected"
                        );
                        peers.insert(peer.id, peer);
                        let edges: Vec<Edge> = edge_to_peer.keys().copied().collect();
                        capture_ctrl.set_active_edges(&edges);
                    }

                    PeerEvent::Control { peer_id, msg } => {
                        match msg {
                            ControlMessage::Leave { edge: _, normalized_pos } => {
                                if active_remote == Some(peer_id) {
                                    if let Some(peer) = peers.get(&peer_id) {
                                        debug!(peer = %peer.name, normalized_pos, "Client handed focus back to server");
                                        let return_edge = peer.edge;
                                        active_remote = None;
                                        active_edge = None;
                                        pending_dx = 0.0;
                                        pending_dy = 0.0;
                                        coalesce_armed = false;
                                        remote_modifiers = ModifierMask::EMPTY;
                                        let _ = capture_ctrl.leave_remote(return_edge, normalized_pos);
                                    }
                                }
                            }
                            ControlMessage::Ping { ts_us } => {
                                if let Some(peer) = peers.get(&peer_id) {
                                    let _ = peer.ctrl_tx.send(ControlMessage::Pong { ts_us });
                                }
                            }
                            _ => {}
                        }
                    }

                    PeerEvent::Clipboard { peer_id, payload } => {
                        if !config.clipboard {
                            continue;
                        }
                        debug!(peer_id, format = ?payload.format, len = payload.data.len(), "Received clipboard from client");
                        if let Some(ref clip) = clipboard {
                            if let Err(e) = clip.backend.set(payload.clone()) {
                                warn!(error = %e, "Failed applying client clipboard to server");
                            }
                        }
                        // Relay to any other connected clients
                        for (&other_id, other_peer) in &peers {
                            if other_id != peer_id {
                                let conn = other_peer.conn.clone();
                                let p = payload.clone();
                                tokio::spawn(async move {
                                    let _ = send_clipboard_on(&conn, &p).await;
                                });
                            }
                        }
                    }

                    PeerEvent::Disconnected { peer_id, reason } => {
                        if let Some(peer) = peers.remove(&peer_id) {
                            info!(peer = %peer.name, %reason, "Client disconnected");
                            if edge_to_peer.get(&peer.edge) == Some(&peer_id) {
                                edge_to_peer.remove(&peer.edge);
                            }
                            let edges: Vec<Edge> = edge_to_peer.keys().copied().collect();
                            capture_ctrl.set_active_edges(&edges);

                            // Safety failsafe: immediately return local control if active client vanished!
                            if active_remote == Some(peer_id) {
                                active_remote = None;
                                active_edge = None;
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                remote_modifiers = ModifierMask::EMPTY;
                                let _ = capture_ctrl.leave_remote(peer.edge, 0.5);
                            }
                        }
                    }
                }
            }

            clip_ev = async {
                match clipboard.as_mut() {
                    Some(c) => c.local_changes.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(payload) = clip_ev {
                    if config.clipboard {
                        for peer in peers.values() {
                            let conn = peer.conn.clone();
                            let p = payload.clone();
                            tokio::spawn(async move {
                                if let Err(e) = send_clipboard_on(&conn, &p).await {
                                    debug!(error = %e, "Failed sending clipboard update to client");
                                }
                            });
                        }
                    }
                }
            }
        }
    }

    accept_handle.abort();
    Ok(())
}

async fn handle_incoming_session(
    mut session: GlideSession,
    server_name: String,
    peer_tx: mpsc::UnboundedSender<PeerEvent>,
) -> Result<()> {
    let hello = tokio::time::timeout(Duration::from_secs(5), session.ctrl_recv.recv()).await??;
    let (peer_name, edge, os) = match hello {
        ControlMessage::Hello {
            version,
            peer_name,
            edge_on_server,
            os,
        } => {
            if version != PROTOCOL_VERSION {
                let _ = session
                    .ctrl_send
                    .send(&ControlMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        server_name,
                        accepted: false,
                        reason: format!(
                            "protocol mismatch: server v{PROTOCOL_VERSION}, client v{version}"
                        ),
                    })
                    .await;
                return Ok(());
            }
            (peer_name, edge_on_server, os)
        }
        other => {
            warn!(?other, "Expected Hello from client");
            return Ok(());
        }
    };

    session
        .ctrl_send
        .send(&ControlMessage::Welcome {
            version: PROTOCOL_VERSION,
            server_name,
            accepted: true,
            reason: String::new(),
        })
        .await?;

    let peer_id = NEXT_PEER_ID.fetch_add(1, Ordering::Relaxed);
    let (ctrl_out_tx, mut ctrl_out_rx) = mpsc::unbounded_channel::<ControlMessage>();

    let conn = session.conn;
    let mut ctrl_send = session.ctrl_send;
    let mut ctrl_recv = session.ctrl_recv;

    // Dedicated writer task for this peer's reliable control stream
    tokio::spawn(async move {
        while let Some(msg) = ctrl_out_rx.recv().await {
            if ctrl_send.send(&msg).await.is_err() {
                break;
            }
        }
    });

    // Dedicated clipboard receiver task for this peer's uni streams
    let clip_conn = conn.clone();
    let clip_tx = peer_tx.clone();
    tokio::spawn(async move {
        while let Ok(payload) = recv_clipboard_on(&clip_conn).await {
            if clip_tx
                .send(PeerEvent::Clipboard { peer_id, payload })
                .is_err()
            {
                break;
            }
        }
    });

    // Register peer with central server loop
    let _ = peer_tx.send(PeerEvent::Connected {
        peer: ConnectedPeer {
            id: peer_id,
            name: peer_name,
            edge,
            os,
            conn: conn.clone(),
            ctrl_tx: ctrl_out_tx,
        },
    });

    // Read incoming control messages until disconnect
    let reason = loop {
        match ctrl_recv.recv().await {
            Ok(msg) => {
                if peer_tx.send(PeerEvent::Control { peer_id, msg }).is_err() {
                    break "server shutting down".to_string();
                }
            }
            Err(e) => break e.to_string(),
        }
    };

    let _ = peer_tx.send(PeerEvent::Disconnected { peer_id, reason });
    Ok(())
}

/// Convenience entrypoint using the native OS capture, clipboard, and persistent TLS identity.
pub async fn run_native_server(config: ServerConfig) -> Result<()> {
    glide_backend::init_platform();
    let identity = ServerIdentity::load_or_generate(&config.resolved_identity_dir())?;
    let endpoint = GlideServerEndpoint::bind(config.bind, &identity)?;

    let capture = glide_backend::start_capture(glide_backend::CaptureConfig {
        escape_key: config.parsed_escape_key(),
        edge_threshold_px: config.edge_threshold_px,
    })?;

    let clipboard = if config.clipboard {
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

    let os_label = match glide_backend::current_os_kind() {
        OsKind::Windows => "windows",
        OsKind::LinuxX11 => "linux-x11",
        OsKind::LinuxWayland => "linux-wayland",
        OsKind::MacOs => "macos",
        OsKind::Unknown => "unknown",
    };

    run_server_with_backends(config, endpoint, capture, clipboard, os_label).await
}
