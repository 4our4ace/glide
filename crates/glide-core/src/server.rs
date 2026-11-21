//! Platform-agnostic Glide Server state machine (v0.2).
//!
//! Features:
//! - Multi-edge client routing with normalized mixed-HiDPI coordinates
//! - 1000Hz high-polling-rate mouse delta coalescing (flushed before clicks/keys)
//! - Direct keyboard screen-switching (`Ctrl+Alt+Arrow`) with per-screen cursor memory
//! - Cursor lock toggle (`Ctrl+Alt+L`) & fullscreen auto-lock
//! - Lock-key sync (`CapsLock` / `NumLock`) on screen entry
//! - Hybrid eager (<= 256 KiB) + lazy on-demand (> 256 KiB) bidirectional clipboard
//! - Drag-and-drop / CLI file beaming over independent QUIC bulk streams
//! - Live Ping/Pong RTT telemetry & hot-reloadable tuning via [`RuntimeState`] / [`RuntimeCommand`]

use crate::config::ServerConfig;
use crate::runtime::{save_received_file, PeerSummary, RuntimeCommand, RuntimeState};
use anyhow::Result;
use glide_backend::os_integration::file_receive_dir;
use glide_backend::{CaptureEvent, CaptureHandle, ClipboardHandle};
use glide_hid::ModifierMask;
use glide_net::discovery::spawn_server_beacon;
use glide_net::tls::ServerIdentity;
use glide_net::{
    recv_bulk_on_with_limit, send_clipboard_on, send_file_on_with_limit, sha256_bytes, Connection,
    GlideServerEndpoint, GlideSession,
};
use glide_proto::{
    BulkTransfer, ClipboardPayload, ControlMessage, DatagramMessage, Edge, FilePayload, LockState,
    OsKind, LAZY_CLIPBOARD_THRESHOLD, PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
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
    last_cursor_norm: (f32, f32),
    rtt_ms: Option<f32>,
}

enum PeerEvent {
    Connected {
        peer: ConnectedPeer,
    },
    Control {
        peer_id: u64,
        msg: ControlMessage,
    },
    Bulk {
        peer_id: u64,
        transfer: BulkTransfer,
    },
    Disconnected {
        peer_id: u64,
        reason: String,
    },
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct EndpointCloseOnDrop(GlideServerEndpoint);
impl Drop for EndpointCloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Runs the Glide server with pre-constructed OS (or mock) backends, optional live GUI state,
/// and optional runtime command channel.
pub async fn run_server_with_backends(
    mut config: ServerConfig,
    endpoint: GlideServerEndpoint,
    capture: CaptureHandle,
    mut clipboard: Option<ClipboardHandle>,
    os_label: &str,
    runtime: Option<RuntimeState>,
    mut cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    let _endpoint_guard = EndpointCloseOnDrop(endpoint.clone());
    let local_addr: SocketAddr = endpoint.local_addr();
    let fingerprint = endpoint.fingerprint().to_string();
    let geom = capture.control.desktop_geometry();
    let server_scale = geom.primary_scale();

    if let Some(ref rt) = runtime {
        rt.update(|s| {
            s.mode = "server".into();
            s.status = "Listening".into();
            s.active_target = "Local".into();
            s.local_addr = Some(local_addr.to_string());
            s.fingerprint = Some(fingerprint.clone());
            s.geometry = geom.clone();
            s.last_error = None;
        });
    }

    info!(
        name = %config.name,
        %local_addr,
        %fingerprint,
        escape_key = %config.parsed_escape_key(),
        "Glide server listening"
    );

    let _beacon_guard = if config.discovery {
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
                Some(AbortOnDrop(h))
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
    let max_file_bytes = Arc::new(AtomicUsize::new(config.resolved_max_file_bytes()));

    let server_name = config.name.clone();
    let accept_tx = peer_tx.clone();
    let accept_max_file_bytes = Arc::clone(&max_file_bytes);
    let _accept_guard = AbortOnDrop(tokio::spawn(async move {
        loop {
            match endpoint.accept().await {
                Ok(session) => {
                    let sname = server_name.clone();
                    let tx = accept_tx.clone();
                    let limit = Arc::clone(&accept_max_file_bytes);
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_incoming_session(session, sname, server_scale, tx, limit).await
                        {
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
    }));

    let mut peers: HashMap<u64, ConnectedPeer> = HashMap::new();
    let mut edge_to_peer: HashMap<Edge, u64> = HashMap::new();
    let mut active_remote: Option<u64> = None;
    let mut active_edge: Option<Edge> = None;
    let mut locked = false;
    let mut remote_modifiers = ModifierMask::EMPTY;
    let mut local_clip_cache: HashMap<[u8; 32], ClipboardPayload> = HashMap::new();

    let mut seq: u32 = 0;
    let mut pending_dx: f32 = 0.0;
    let mut pending_dy: f32 = 0.0;
    let coalesce_dur = Duration::from_micros(config.motion_coalesce_us);
    let mut coalesce_armed = false;
    let coalesce_sleep = tokio::time::sleep(Duration::from_secs(86_400));
    tokio::pin!(coalesce_sleep);

    let mut ping_interval = tokio::time::interval(Duration::from_millis(500));
    ping_interval.tick().await;

    let mut capture_events = capture.events;
    let capture_ctrl = capture.control;

    let sync_runtime_peers = |rt: &Option<RuntimeState>,
                              peers: &HashMap<u64, ConnectedPeer>,
                              active_remote: Option<u64>,
                              locked: bool| {
        if let Some(ref rt) = rt {
            let summaries: Vec<PeerSummary> = peers
                .values()
                .map(|p| PeerSummary {
                    name: p.name.clone(),
                    os: p.os,
                    edge: p.edge,
                    addr: p.conn.remote_address().to_string(),
                    rtt_ms: p.rtt_ms,
                })
                .collect();
            let active_name = active_remote
                .and_then(|id| peers.get(&id))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Local".into());
            let status = if summaries.is_empty() {
                "Listening (waiting for client)".into()
            } else {
                format!("Connected ({} peer(s))", summaries.len())
            };
            rt.update(|s| {
                s.peers = summaries;
                s.active_target = active_name;
                s.locked = locked;
                s.status = status;
            });
        }
    };

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

            _ = ping_interval.tick() => {
                if !peers.is_empty() {
                    let mut best_rtt: Option<f32> = None;
                    for peer in peers.values_mut() {
                        let rtt_ms = peer.conn.rtt().as_secs_f32() * 1000.0;
                        peer.rtt_ms = Some(rtt_ms);
                        best_rtt = Some(match best_rtt {
                            Some(cur) => cur.min(rtt_ms),
                            None => rtt_ms,
                        });
                    }
                    if let (Some(ref rt), Some(ms)) = (&runtime, best_rtt) {
                        rt.record_rtt(ms);
                    }
                    sync_runtime_peers(&runtime, &peers, active_remote, locked);
                }
            }

            cmd = async {
                match cmd_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    RuntimeCommand::UpdateServerTuning {
                        edge_threshold_px,
                        dead_corner_px,
                        edge_dwell_ms,
                        auto_lock_fullscreen,
                        locked: new_locked,
                        sync_lock_keys,
                        clipboard: clip_enabled,
                    } => {
                        config.edge_threshold_px = edge_threshold_px;
                        config.dead_corner_px = dead_corner_px;
                        config.edge_dwell_ms = edge_dwell_ms;
                        config.auto_lock_fullscreen = auto_lock_fullscreen;
                        config.sync_lock_keys = sync_lock_keys;
                        config.clipboard = clip_enabled;
                        locked = new_locked;
                        capture_ctrl.update_tuning(
                            edge_threshold_px,
                            dead_corner_px,
                            edge_dwell_ms,
                            auto_lock_fullscreen,
                            locked,
                        );
                        capture_ctrl.set_sync_lock_keys(sync_lock_keys);
                        sync_runtime_peers(&runtime, &peers, active_remote, locked);
                    }
                    RuntimeCommand::ToggleLock => {
                        locked = !locked;
                        capture_ctrl.update_tuning(
                            config.edge_threshold_px,
                            config.dead_corner_px,
                            config.edge_dwell_ms,
                            config.auto_lock_fullscreen,
                            locked,
                        );
                        info!(locked, "Toggled cursor lock");
                        sync_runtime_peers(&runtime, &peers, active_remote, locked);
                    }
                    RuntimeCommand::SwitchToEdge(edge) => {
                        if let Some(&peer_id) = edge_to_peer.get(&edge) {
                            if let Some(peer) = peers.get(&peer_id) {
                                let lock_state = if config.sync_lock_keys {
                                    capture_ctrl.lock_state()
                                } else {
                                    LockState::default()
                                };
                                active_remote = Some(peer_id);
                                active_edge = Some(edge);
                                remote_modifiers = ModifierMask::EMPTY;
                                let _ = capture_ctrl.enter_remote();
                                let (nx, ny) = peer.last_cursor_norm;
                                let _ = peer.ctrl_tx.send(ControlMessage::EnterAt {
                                    norm_x: nx,
                                    norm_y: ny,
                                    lock_state,
                                });
                                sync_runtime_peers(&runtime, &peers, active_remote, locked);
                            }
                        }
                    }
                    RuntimeCommand::ReturnLocal => {
                        if let Some(pid) = active_remote.take() {
                            if let Some(peer) = peers.get(&pid) {
                                let _ = peer.ctrl_tx.send(ControlMessage::ReleaseAll);
                                let _ = peer.ctrl_tx.send(ControlMessage::Leave {
                                    edge: peer.edge.opposite(),
                                    normalized_pos: 0.5,
                                    cursor_norm_x: 0.5,
                                    cursor_norm_y: 0.5,
                                });
                            }
                        }
                        active_edge = None;
                        let (lx, ly) = capture_ctrl.last_local_cursor_norm();
                        let _ = capture_ctrl.leave_remote_at(lx, ly);
                        sync_runtime_peers(&runtime, &peers, active_remote, locked);
                    }
                    RuntimeCommand::SendFile { filename, data } => {
                        let limit = max_file_bytes.load(Ordering::Relaxed);
                        let payload = FilePayload {
                            filename: filename.clone(),
                            sha256: sha256_bytes(&data),
                            data,
                        };
                        if let Some(ref rt) = runtime {
                            rt.record_file("sent", filename, payload.data.len(), None);
                        }
                        for peer in peers.values() {
                            let conn = peer.conn.clone();
                            let p = payload.clone();
                            tokio::spawn(async move {
                                if let Err(e) = send_file_on_with_limit(&conn, &p, limit).await {
                                    warn!(error = %e, "Failed beaming file to client");
                                }
                            });
                        }
                    }
                    RuntimeCommand::UpdateMaxFileBytes(limit) => {
                        let effective = limit.min(u32::MAX as usize);
                        config.max_file_bytes = Some(effective);
                        max_file_bytes.store(effective, Ordering::Relaxed);
                    }
                    RuntimeCommand::ResendClipboard(id) => {
                        if let Some(ref rt) = runtime {
                            if let Some(payload) = rt.find_clipboard_payload(id) {
                                for peer in peers.values() {
                                    let conn = peer.conn.clone();
                                    let p = payload.clone();
                                    tokio::spawn(async move {
                                        let _ = send_clipboard_on(&conn, &p).await;
                                    });
                                }
                            }
                        }
                    }
                    RuntimeCommand::MovePeerEdge { peer_name, edge } => {
                        let target_id = if let Some(ref name) = peer_name {
                            peers
                                .iter()
                                .find(|(_, p)| p.name == *name)
                                .map(|(&id, _)| id)
                        } else {
                            peers.keys().next().copied()
                        };
                        if let Some(pid) = target_id {
                            if let Some(old_e) = peers.get(&pid).map(|p| p.edge) {
                                if old_e != edge {
                                    if let Some(other_id) = edge_to_peer.remove(&edge) {
                                        if other_id != pid {
                                            edge_to_peer.insert(old_e, other_id);
                                            if let Some(other) = peers.get_mut(&other_id) {
                                                other.edge = old_e;
                                                let _ = other.ctrl_tx.send(
                                                    ControlMessage::SetEdge {
                                                        edge_on_server: old_e,
                                                    },
                                                );
                                            }
                                        }
                                    } else {
                                        edge_to_peer.remove(&old_e);
                                    }
                                    edge_to_peer.insert(edge, pid);
                                    if let Some(peer) = peers.get_mut(&pid) {
                                        peer.edge = edge;
                                        let _ = peer.ctrl_tx.send(ControlMessage::SetEdge {
                                            edge_on_server: edge,
                                        });
                                        info!(peer = %peer.name, ?edge, "Repositioned client edge live");
                                    }
                                    if active_remote == Some(pid) {
                                        active_edge = Some(edge);
                                    }
                                    let edges: Vec<Edge> = edge_to_peer.keys().copied().collect();
                                    capture_ctrl.set_active_edges(&edges);
                                    sync_runtime_peers(&runtime, &peers, active_remote, locked);
                                }
                            }
                        }
                    }
                    RuntimeCommand::Shutdown => {
                        info!("Received graceful shutdown command for Glide server");
                        break;
                    }
                    RuntimeCommand::UpdateClientTuning { .. } => {}
                }
            }

            ev = capture_events.recv() => {
                let Some(ev) = ev else {
                    warn!("OS capture event channel closed; stopping server");
                    break;
                };

                match ev {
                    CaptureEvent::EdgeCrossed { edge, normalized_pos } => {
                        if locked {
                            let _ = capture_ctrl.leave_remote(edge, normalized_pos);
                            continue;
                        }
                        if let Some(&peer_id) = edge_to_peer.get(&edge) {
                            if let Some(peer) = peers.get(&peer_id) {
                                info!(peer = %peer.name, ?edge, normalized_pos, "Switching focus to client");
                                let lock_state = if config.sync_lock_keys {
                                    capture_ctrl.lock_state()
                                } else {
                                    LockState::default()
                                };
                                active_remote = Some(peer_id);
                                active_edge = Some(edge);
                                remote_modifiers = ModifierMask::EMPTY;
                                let _ = capture_ctrl.enter_remote();
                                let _ = peer.ctrl_tx.send(ControlMessage::Enter {
                                    edge: edge.opposite(),
                                    normalized_pos,
                                    lock_state,
                                });
                                sync_runtime_peers(&runtime, &peers, active_remote, locked);
                            }
                        } else {
                            let _ = capture_ctrl.leave_remote(edge, normalized_pos);
                        }
                    }

                    CaptureEvent::HotkeySwitch { direction } => {
                        if let Some(current_pid) = active_remote {
                            // If pressing opposite direction of current remote edge, return to server with cursor memory!
                            if active_edge.is_some_and(|e| e.opposite() == direction) {
                                if let Some(peer) = peers.get(&current_pid) {
                                    let _ = peer.ctrl_tx.send(ControlMessage::ReleaseAll);
                                    let _ = peer.ctrl_tx.send(ControlMessage::Leave {
                                        edge: peer.edge.opposite(),
                                        normalized_pos: 0.5,
                                        cursor_norm_x: 0.5,
                                        cursor_norm_y: 0.5,
                                    });
                                }
                                active_remote = None;
                                active_edge = None;
                                let (lx, ly) = capture_ctrl.last_local_cursor_norm();
                                let _ = capture_ctrl.leave_remote_at(lx, ly);
                                sync_runtime_peers(&runtime, &peers, active_remote, locked);
                            }
                        } else if let Some(&peer_id) = edge_to_peer.get(&direction) {
                            if let Some(peer) = peers.get(&peer_id) {
                                info!(peer = %peer.name, ?direction, "Hotkey jump to client (restoring cursor memory)");
                                let lock_state = if config.sync_lock_keys {
                                    capture_ctrl.lock_state()
                                } else {
                                    LockState::default()
                                };
                                active_remote = Some(peer_id);
                                active_edge = Some(direction);
                                remote_modifiers = ModifierMask::EMPTY;
                                let _ = capture_ctrl.enter_remote();
                                let (nx, ny) = peer.last_cursor_norm;
                                let _ = peer.ctrl_tx.send(ControlMessage::EnterAt {
                                    norm_x: nx,
                                    norm_y: ny,
                                    lock_state,
                                });
                                sync_runtime_peers(&runtime, &peers, active_remote, locked);
                            }
                        }
                    }

                    CaptureEvent::HotkeyToggleLock => {
                        locked = !locked;
                        capture_ctrl.update_tuning(
                            config.edge_threshold_px,
                            config.dead_corner_px,
                            config.edge_dwell_ms,
                            config.auto_lock_fullscreen,
                            locked,
                        );
                        info!(locked, "Cursor lock toggled via Ctrl+Alt+L");
                        sync_runtime_peers(&runtime, &peers, active_remote, locked);
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
                                    cursor_norm_x: 0.5,
                                    cursor_norm_y: 0.5,
                                });
                            }
                        }
                        active_edge = None;
                        let (lx, ly) = capture_ctrl.last_local_cursor_norm();
                        let _ = capture_ctrl.leave_remote_at(lx, ly);
                        remote_modifiers = ModifierMask::EMPTY;
                        pending_dx = 0.0;
                        pending_dy = 0.0;
                        coalesce_armed = false;
                        sync_runtime_peers(&runtime, &peers, active_remote, locked);
                    }
                }
            }

            pe = peer_rx.recv() => {
                let Some(pe) = pe else { break };
                match pe {
                    PeerEvent::Connected { peer } => {
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
                        sync_runtime_peers(&runtime, &peers, active_remote, locked);
                    }

                    PeerEvent::Control { peer_id, msg } => {
                        match msg {
                            ControlMessage::Leave {
                                edge: _,
                                normalized_pos,
                                cursor_norm_x,
                                cursor_norm_y,
                            } => {
                                if let Some(peer) = peers.get_mut(&peer_id) {
                                    peer.last_cursor_norm = (cursor_norm_x, cursor_norm_y);
                                }
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
                                        sync_runtime_peers(&runtime, &peers, active_remote, locked);
                                    }
                                }
                            }
                            ControlMessage::ClipboardRequest { sha256 } => {
                                if let (Some(payload), Some(peer)) =
                                    (local_clip_cache.get(&sha256).cloned(), peers.get(&peer_id))
                                {
                                    debug!(peer = %peer.name, len = payload.data.len(), "Streaming lazily requested clipboard payload");
                                    let conn = peer.conn.clone();
                                    tokio::spawn(async move {
                                        let _ = send_clipboard_on(&conn, &payload).await;
                                    });
                                }
                            }
                            ControlMessage::ClipboardOffer { sha256, .. } => {
                                // Server immediately pulls client's offered clipboard if clipboard sync is enabled
                                if config.clipboard {
                                    if let Some(peer) = peers.get(&peer_id) {
                                        let _ = peer.ctrl_tx.send(ControlMessage::ClipboardRequest { sha256 });
                                    }
                                }
                            }
                            ControlMessage::SetEdge { edge_on_server } => {
                                if let Some(old_e) = peers.get(&peer_id).map(|p| p.edge) {
                                    if old_e != edge_on_server {
                                        if let Some(other_id) = edge_to_peer.remove(&edge_on_server) {
                                            if other_id != peer_id {
                                                edge_to_peer.insert(old_e, other_id);
                                                if let Some(other) = peers.get_mut(&other_id) {
                                                    other.edge = old_e;
                                                    let _ = other.ctrl_tx.send(
                                                        ControlMessage::SetEdge {
                                                            edge_on_server: old_e,
                                                        },
                                                    );
                                                }
                                            }
                                        } else {
                                            edge_to_peer.remove(&old_e);
                                        }
                                        edge_to_peer.insert(edge_on_server, peer_id);
                                        if let Some(peer) = peers.get_mut(&peer_id) {
                                            peer.edge = edge_on_server;
                                            info!(peer = %peer.name, edge = ?edge_on_server, "Client updated its own edge live");
                                        }
                                        if active_remote == Some(peer_id) {
                                            active_edge = Some(edge_on_server);
                                        }
                                        let edges: Vec<Edge> =
                                            edge_to_peer.keys().copied().collect();
                                        capture_ctrl.set_active_edges(&edges);
                                        sync_runtime_peers(&runtime, &peers, active_remote, locked);
                                    }
                                }
                            }
                            ControlMessage::Ping { ts_us } => {
                                if let Some(peer) = peers.get(&peer_id) {
                                    let _ = peer.ctrl_tx.send(ControlMessage::Pong { ts_us });
                                }
                            }
                            ControlMessage::Pong { .. } => {}
                            _ => {}
                        }
                    }

                    PeerEvent::Bulk { peer_id, transfer } => {
                        match transfer {
                            BulkTransfer::Clipboard(payload) => {
                                if !config.clipboard {
                                    continue;
                                }
                                debug!(peer_id, format = ?payload.format, len = payload.data.len(), "Received clipboard from client");
                                if let Some(ref rt) = runtime {
                                    rt.record_clipboard("received", &payload);
                                }
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
                            BulkTransfer::File(file) => {
                                match file_receive_dir()
                                    .and_then(|dir| save_received_file(&dir, &file.filename, &file.data))
                                {
                                    Ok(saved) => {
                                        info!(file = %file.filename, path = %saved.display(), bytes = file.data.len(), "Received file from client");
                                        if let Some(ref rt) = runtime {
                                            rt.record_file("received", file.filename, file.data.len(), Some(saved));
                                        }
                                    }
                                    Err(e) => warn!(error = %e, "Failed saving received file"),
                                }
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

                            if active_remote == Some(peer_id) {
                                active_remote = None;
                                active_edge = None;
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                remote_modifiers = ModifierMask::EMPTY;
                                let _ = capture_ctrl.leave_remote(peer.edge, 0.5);
                            }
                            sync_runtime_peers(&runtime, &peers, active_remote, locked);
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
                        if let Some(ref rt) = runtime {
                            rt.record_clipboard("sent", &payload);
                        }
                        if payload.data.len() > LAZY_CLIPBOARD_THRESHOLD {
                            // Lazy offer: send only 37-byte metadata until a client requests it!
                            if local_clip_cache.len() > 4 {
                                local_clip_cache.clear();
                            }
                            local_clip_cache.insert(payload.sha256, payload.clone());
                            let offer = ControlMessage::ClipboardOffer {
                                format: payload.format,
                                sha256: payload.sha256,
                                byte_len: payload.data.len() as u32,
                            };
                            for peer in peers.values() {
                                let _ = peer.ctrl_tx.send(offer.clone());
                            }
                        } else {
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
    }

    drop(_accept_guard);
    drop(_beacon_guard);
    drop(_endpoint_guard);
    if let Some(ref rt) = runtime {
        rt.update(|s| {
            s.mode = "stopped".into();
            s.status = "Idle".into();
            s.active_target = "Local".into();
            s.peers.clear();
        });
    }
    Ok(())
}

async fn handle_incoming_session(
    mut session: GlideSession,
    server_name: String,
    server_scale: f32,
    peer_tx: mpsc::UnboundedSender<PeerEvent>,
    max_file_bytes: Arc<AtomicUsize>,
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
                        server_scale,
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
            server_scale,
        })
        .await?;

    let peer_id = NEXT_PEER_ID.fetch_add(1, Ordering::Relaxed);
    let (ctrl_out_tx, mut ctrl_out_rx) = mpsc::unbounded_channel::<ControlMessage>();

    let conn = session.conn;
    let mut ctrl_send = session.ctrl_send;
    let mut ctrl_recv = session.ctrl_recv;

    tokio::spawn(async move {
        while let Some(msg) = ctrl_out_rx.recv().await {
            if ctrl_send.send(&msg).await.is_err() {
                break;
            }
        }
    });

    let bulk_conn = conn.clone();
    let bulk_tx = peer_tx.clone();
    tokio::spawn(async move {
        loop {
            let limit = max_file_bytes.load(Ordering::Relaxed);
            match recv_bulk_on_with_limit(&bulk_conn, limit).await {
                Ok(transfer) => {
                    if bulk_tx.send(PeerEvent::Bulk { peer_id, transfer }).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    if bulk_conn.close_reason().is_some()
                        || e.to_string().contains("bulk uni stream closed")
                    {
                        break;
                    }
                    warn!(peer_id, error = %e, "Rejected incoming bulk transfer");
                }
            }
        }
    });

    let _ = peer_tx.send(PeerEvent::Connected {
        peer: ConnectedPeer {
            id: peer_id,
            name: peer_name,
            edge,
            os,
            conn: conn.clone(),
            ctrl_tx: ctrl_out_tx,
            last_cursor_norm: (0.5, 0.5),
            rtt_ms: None,
        },
    });

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

/// Runs the native OS Glide server with optional GUI runtime hooks.
pub async fn run_native_server_with_runtime(
    config: ServerConfig,
    runtime: Option<RuntimeState>,
    cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    glide_backend::init_platform();
    let identity = ServerIdentity::load_or_generate(&config.resolved_identity_dir())?;
    let endpoint = GlideServerEndpoint::bind(config.bind, &identity)?;

    let capture = glide_backend::start_capture(glide_backend::CaptureConfig {
        escape_key: config.parsed_escape_key(),
        edge_threshold_px: config.edge_threshold_px,
        dead_corner_px: config.dead_corner_px,
        edge_dwell_ms: config.edge_dwell_ms,
        auto_lock_fullscreen: config.auto_lock_fullscreen,
        enable_hotkeys: config.enable_hotkeys,
        sync_lock_keys: config.sync_lock_keys,
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

    run_server_with_backends(
        config, endpoint, capture, clipboard, os_label, runtime, cmd_rx,
    )
    .await
}

/// Convenience CLI entrypoint using native OS backends.
pub async fn run_native_server(config: ServerConfig) -> Result<()> {
    run_native_server_with_runtime(config, None, None).await
}
