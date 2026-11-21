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
use crate::dnd::{
    collect_local_drag_files, next_drag_id, spawn_send_drag_files, EarlyDragBuffer,
    IncomingDragState, OutgoingDragState,
};
use crate::resilience::{HealthProbe, SleepDetector};
use crate::runtime::{save_received_file, PeerSummary, RuntimeCommand, RuntimeState};
use anyhow::Result;
use glide_backend::dnd::{DndBackend, NoopDndBackend};
use glide_backend::os_integration::resolve_receive_dir;
use glide_backend::{CaptureEvent, CaptureHandle, ClipboardBackend, ClipboardHandle};
use glide_hid::{HidUsage, ModifierMask, MouseButton};
use glide_net::discovery::{local_ipv4_addresses_async, spawn_server_beacon_with_refresh};
use glide_net::tls::ServerIdentity;
use glide_net::{
    recv_bulk_stream_with_atomic_limit, send_clipboard_on, send_file_on_with_limit,
    sha256_bytes_async, Connection, GlideServerEndpoint, GlideSession,
};
use glide_proto::{
    BulkTransfer, ClipboardPayload, ControlMessage, DatagramEncoder, DatagramMessage,
    DragFileInfo, Edge, FilePayload, LockState, OsKind, LAZY_CLIPBOARD_THRESHOLD,
    PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};

static NEXT_PEER_ID: AtomicU64 = AtomicU64::new(1);

/// Minimum available QUIC datagram send buffer space (bytes) required before enqueuing
/// another `MouseMove` datagram, preventing stale pointer backlog during Wi-Fi congestion.
const MIN_MOTION_DGRAM_BUFFER_SPACE: usize = 128;

/// Re-arm interval when deferring coalesced mouse deltas due to QUIC datagram backpressure.
const CONGESTION_RETRY_DELAY: Duration = Duration::from_millis(2);

struct ConnectedPeer {
    id: u64,
    name: String,
    edge: Edge,
    os: OsKind,
    conn: Connection,
    ctrl_tx: mpsc::UnboundedSender<ControlMessage>,
    last_cursor_norm: (f32, f32),
    rtt_ms: Option<f32>,
    health_probe: HealthProbe,
    last_lost_packets: u64,
    last_congestion_events: u64,
    last_lag_warn_at: Option<Instant>,
}

struct PendingDragHandoff {
    gen: u64,
    edge: Edge,
    peer_id: u64,
    normalized_pos: f32,
}

struct DragHandoffReady {
    gen: u64,
    peer_id: u64,
    edge: Edge,
    paths: Vec<PathBuf>,
    limit: usize,
}

struct DragFilesCollected {
    drag_id: u64,
    peer_id: u64,
    infos: Vec<DragFileInfo>,
    send_list: Vec<(String, PathBuf)>,
    limit: usize,
}

struct ActiveOutgoingDrag {
    state: OutgoingDragState,
    collecting: bool,
    drop_requested: bool,
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
/// and optional runtime command channel (using a no-op DnD backend).
pub async fn run_server_with_backends(
    config: ServerConfig,
    endpoint: GlideServerEndpoint,
    capture: CaptureHandle,
    clipboard: Option<ClipboardHandle>,
    os_label: &str,
    runtime: Option<RuntimeState>,
    cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    run_server_with_dnd(
        config,
        endpoint,
        capture,
        clipboard,
        Arc::new(NoopDndBackend),
        None,
        os_label,
        runtime,
        cmd_rx,
    )
    .await
}

/// Runs the Glide server with explicit OS/mock backends including cross-edge drag-and-drop.
#[allow(clippy::too_many_arguments)]
pub async fn run_server_with_dnd(
    mut config: ServerConfig,
    endpoint: GlideServerEndpoint,
    capture: CaptureHandle,
    clipboard: Option<ClipboardHandle>,
    dnd: Arc<dyn DndBackend>,
    receive_dir: Option<PathBuf>,
    os_label: &str,
    runtime: Option<RuntimeState>,
    mut cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    let _endpoint_guard = EndpointCloseOnDrop(endpoint.clone());
    let local_addr: SocketAddr = endpoint.local_addr();
    let fingerprint = endpoint.fingerprint().to_string();
    let geom = capture.control.desktop_geometry();
    let server_scale = Arc::new(AtomicU32::new(geom.primary_scale().to_bits()));
    let mut last_local_ips = local_ipv4_addresses_async().await;
    dnd.set_enabled(config.drag_drop_files);
    capture.control.set_drag_drop_files(config.drag_drop_files);
    capture.control.set_dnd_probe(Arc::clone(&dnd));

    // Dedicated serial clipboard applier so OS clipboard locks (e.g. OpenClipboard / DIB->PNG)
    // apply strictly in order on the blocking pool without ever stalling the capture select! loop.
    let (clip_apply_tx, _clip_apply_guard, mut clip_local_rx) = match clipboard {
        Some(handle) => {
            let (tx, mut rx) = mpsc::unbounded_channel::<ClipboardPayload>();
            let backend: Arc<dyn ClipboardBackend> = Arc::from(handle.backend);
            let guard = AbortOnDrop(tokio::spawn(async move {
                while let Some(payload) = rx.recv().await {
                    let b = Arc::clone(&backend);
                    match tokio::task::spawn_blocking(move || b.set(payload)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            warn!(error = %e, "Failed applying client clipboard to server")
                        }
                        Err(e) => warn!(error = %e, "Server clipboard apply task failed"),
                    }
                }
            }));
            (Some(tx), Some(guard), Some(handle.local_changes))
        }
        None => (None, None, None),
    };

    if let Some(ref rt) = runtime {
        rt.update(|s| {
            s.mode = "server".into();
            s.status = "Listening".into();
            s.active_target = "Local".into();
            s.local_addr = Some(local_addr.to_string());
            s.local_ips = last_local_ips.clone();
            s.fingerprint = Some(fingerprint.clone());
            s.remote_fingerprint = None;
            s.geometry = geom.clone();
            s.last_error = None;
        });
    }

    let initial_hotkeys = config.parsed_hotkeys();
    let escape_display = initial_hotkeys
        .escape
        .map(|c| c.format_canonical())
        .unwrap_or_else(|| "disabled".into());
    info!(
        name = %config.name,
        %local_addr,
        %fingerprint,
        escape_key = %escape_display,
        "Glide server listening"
    );

    let beacon_refresh = Arc::new(Notify::new());
    let _beacon_guard = if config.discovery {
        match spawn_server_beacon_with_refresh(
            config.discovery_port,
            config.name.clone(),
            local_addr.port(),
            fingerprint.clone(),
            os_label.to_string(),
            Some(Arc::clone(&beacon_refresh)),
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
    let accept_scale = Arc::clone(&server_scale);
    let accept_max_file_bytes = Arc::clone(&max_file_bytes);
    let _accept_guard = AbortOnDrop(tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept_incoming().await {
            let sname = server_name.clone();
            let tx = accept_tx.clone();
            let scale = f32::from_bits(accept_scale.load(Ordering::Relaxed));
            let limit = Arc::clone(&accept_max_file_bytes);
            tokio::spawn(async move {
                let session = match tokio::time::timeout(
                    Duration::from_secs(5),
                    GlideServerEndpoint::complete_incoming(incoming),
                )
                .await
                {
                    Ok(Ok(session)) => session,
                    Ok(Err(e)) => {
                        warn!(error = %e, "Incoming client QUIC handshake failed");
                        return;
                    }
                    Err(_) => {
                        warn!("Timed out waiting for incoming client QUIC handshake");
                        return;
                    }
                };
                if let Err(e) = handle_incoming_session(session, sname, scale, tx, limit).await {
                    warn!(error = %e, "Incoming client handshake failed");
                }
            });
        }
        debug!("QUIC accept loop ended (server endpoint closed)");
    }));

    let mut peers: HashMap<u64, ConnectedPeer> = HashMap::new();
    let mut edge_to_peer: HashMap<Edge, u64> = HashMap::new();
    let mut active_remote: Option<u64> = None;
    let mut active_edge: Option<Edge> = None;
    let mut locked = false;
    let mut remote_modifiers = ModifierMask::EMPTY;
    let mut local_clip_cache: HashMap<[u8; 32], ClipboardPayload> = HashMap::new();
    let mut outgoing_drag: Option<ActiveOutgoingDrag> = None;
    let mut incoming_drag: Option<IncomingDragState> = None;
    let mut incoming_drag_peer: Option<u64> = None;
    let mut early_drag_files = EarlyDragBuffer::default();
    let mut local_lmb_down = false;

    let (ip_tx, mut ip_rx) = mpsc::unbounded_channel::<(u64, Vec<String>, bool)>();
    let mut ip_check_gen: u64 = 0;
    let mut ip_check_in_flight: usize = 0;

    let (drag_handoff_tx, mut drag_handoff_rx) = mpsc::unbounded_channel::<DragHandoffReady>();
    let mut drag_handoff_gen: u64 = 0;
    let mut pending_drag_handoff: Option<PendingDragHandoff> = None;
    let (drag_collect_tx, mut drag_collect_rx) = mpsc::unbounded_channel::<DragFilesCollected>();

    let mut dgram_enc = DatagramEncoder::new();
    let mut seq: u32 = 0;
    let mut pending_dx: f32 = 0.0;
    let mut pending_dy: f32 = 0.0;
    let mut last_motion_sent: Option<Instant> = None;
    let coalesce_dur = Duration::from_micros(config.motion_coalesce_us);
    let mut coalesce_armed = false;
    let coalesce_sleep = tokio::time::sleep(Duration::from_secs(86_400));
    tokio::pin!(coalesce_sleep);

    let mut ping_interval = tokio::time::interval(Duration::from_millis(500));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping_interval.tick().await;
    let mut sleep_detector = SleepDetector::default();
    let mut last_ip_check = Instant::now();

    let mut capture_events = capture.events;
    let capture_ctrl = capture.control;

    let sync_runtime_peers = |rt: &Option<RuntimeState>,
                              peers: &HashMap<u64, ConnectedPeer>,
                              active_remote: Option<u64>,
                              locked: bool,
                              best_rtt: Option<f32>| {
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
            rt.record_rtt_and_peers(best_rtt, summaries, active_name, locked, status);
        }
    };

    // Flushes any accumulated motion delta if the peer's QUIC datagram send buffer has room.
    // Returns `Some(retry_delay)` when QUIC backpressure forces us to defer and keep coalescing
    // instead of queueing stale pointer datagrams.
    let flush_pending_motion = |peers: &HashMap<u64, ConnectedPeer>,
                                active_remote: Option<u64>,
                                pending_dx: &mut f32,
                                pending_dy: &mut f32,
                                seq: &mut u32,
                                coalesce_armed: &mut bool,
                                last_motion_sent: &mut Option<Instant>,
                                dgram_enc: &mut DatagramEncoder|
     -> Option<Duration> {
        if *pending_dx == 0.0 && *pending_dy == 0.0 {
            *coalesce_armed = false;
            return None;
        }
        if let Some(pid) = active_remote {
            if let Some(peer) = peers.get(&pid) {
                if peer.conn.datagram_send_buffer_space() < MIN_MOTION_DGRAM_BUFFER_SPACE {
                    *coalesce_armed = true;
                    return Some(CONGESTION_RETRY_DELAY);
                }
                *seq = seq.wrapping_add(1);
                let msg = DatagramMessage::MouseMove {
                    seq: *seq,
                    dx: *pending_dx,
                    dy: *pending_dy,
                };
                let _ = peer.conn.send_datagram(dgram_enc.encode(msg));
                *last_motion_sent = Some(Instant::now());
            }
        }
        *pending_dx = 0.0;
        *pending_dy = 0.0;
        *coalesce_armed = false;
        None
    };

    let cancel_outgoing_drag = |outgoing_drag: &mut Option<ActiveOutgoingDrag>,
                                peers: &HashMap<u64, ConnectedPeer>| {
        if let Some(out) = outgoing_drag.take() {
            if !out.collecting {
                if let Some(peer) = peers.get(&out.state.peer_id) {
                    let _ = peer.ctrl_tx.send(ControlMessage::DragCancel {
                        drag_id: out.state.drag_id,
                    });
                }
            }
        }
    };

    let probe_all_peers = |peers: &mut HashMap<u64, ConnectedPeer>, now: Instant| {
        for peer in peers.values_mut() {
            let ts_us = peer.health_probe.start_probe(now);
            let _ = peer.ctrl_tx.send(ControlMessage::Ping { ts_us });
        }
    };

    loop {
        tokio::select! {
            _ = &mut coalesce_sleep, if coalesce_armed => {
                if let Some(retry_delay) = flush_pending_motion(
                    &peers,
                    active_remote,
                    &mut pending_dx,
                    &mut pending_dy,
                    &mut seq,
                    &mut coalesce_armed,
                    &mut last_motion_sent,
                    &mut dgram_enc,
                ) {
                    coalesce_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + retry_delay);
                }
            }

            _ = ping_interval.tick() => {
                let now = Instant::now();

                // 1. Cross-platform sleep/suspend jump detection (> 5s monotonic or wall-clock leap)
                if let Some(jump) = sleep_detector.check() {
                    info!(
                        jump_secs = jump.as_secs_f32(),
                        "Detected sleep/wall-clock jump on server; releasing remote focus, refreshing geometry & LAN IPs, and probing peers"
                    );
                    pending_drag_handoff = None;
                    cancel_outgoing_drag(&mut outgoing_drag, &peers);
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
                        active_edge = None;
                        remote_modifiers = ModifierMask::EMPTY;
                        pending_dx = 0.0;
                        pending_dy = 0.0;
                        coalesce_armed = false;
                        last_motion_sent = None;
                        let (lx, ly) = capture_ctrl.last_local_cursor_norm();
                        let _ = capture_ctrl.leave_remote_at(lx, ly);
                    }
                    let fresh_geom = capture_ctrl.desktop_geometry();
                    server_scale.store(fresh_geom.primary_scale().to_bits(), Ordering::Relaxed);
                    last_ip_check = now;
                    ip_check_gen = ip_check_gen.wrapping_add(1);
                    ip_check_in_flight += 1;
                    let gen = ip_check_gen;
                    let tx = ip_tx.clone();
                    tokio::spawn(async move {
                        let ips = local_ipv4_addresses_async().await;
                        let _ = tx.send((gen, ips, true));
                    });
                    if let Some(ref rt) = runtime {
                        rt.update(|s| {
                            s.geometry = fresh_geom;
                        });
                    }
                    beacon_refresh.notify_one();
                    probe_all_peers(&mut peers, now);
                    sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                } else if now.saturating_duration_since(last_ip_check) >= Duration::from_secs(3) {
                    // 2. Periodic Wi-Fi <-> Ethernet roaming detection (offloaded off select! loop)
                    last_ip_check = now;
                    if ip_check_in_flight == 0 {
                        ip_check_gen = ip_check_gen.wrapping_add(1);
                        ip_check_in_flight += 1;
                        let gen = ip_check_gen;
                        let tx = ip_tx.clone();
                        tokio::spawn(async move {
                            let ips = local_ipv4_addresses_async().await;
                            let _ = tx.send((gen, ips, false));
                        });
                    }
                }

                // 3. Evict any peers whose QUIC connection closed or failed post-resume health ping
                let dead_ids: Vec<u64> = peers
                    .iter()
                    .filter(|(_, p)| {
                        p.conn.close_reason().is_some() || p.health_probe.is_timed_out(now)
                    })
                    .map(|(&id, _)| id)
                    .collect();
                for dead_id in dead_ids {
                    if let Some(peer) = peers.remove(&dead_id) {
                        let timed_out = peer.health_probe.is_timed_out(now);
                        warn!(
                            peer = %peer.name,
                            timed_out,
                            "Evicting unresponsive peer after wake/network change"
                        );
                        if timed_out {
                            peer.conn.close(1u32.into(), b"health probe timeout");
                        }
                        if edge_to_peer.get(&peer.edge) == Some(&dead_id) {
                            edge_to_peer.remove(&peer.edge);
                        }
                        if pending_drag_handoff
                            .as_ref()
                            .is_some_and(|p| p.peer_id == dead_id)
                        {
                            pending_drag_handoff = None;
                        }
                        if outgoing_drag
                            .as_ref()
                            .is_some_and(|o| o.state.peer_id == dead_id)
                        {
                            outgoing_drag = None;
                        }
                        if incoming_drag_peer == Some(dead_id) {
                            incoming_drag_peer = None;
                            early_drag_files.clear();
                            if let Some(inc) = incoming_drag.take() {
                                if !inc.drop_requested {
                                    inc.cancel(Arc::clone(&dnd));
                                } else {
                                    incoming_drag = Some(inc);
                                }
                            }
                        }
                        if active_remote == Some(dead_id) {
                            active_remote = None;
                            active_edge = None;
                            pending_dx = 0.0;
                            pending_dy = 0.0;
                            coalesce_armed = false;
                            last_motion_sent = None;
                            remote_modifiers = ModifierMask::EMPTY;
                            let _ = capture_ctrl.leave_remote(peer.edge, 0.5);
                        }
                        let edges: Vec<Edge> = edge_to_peer.keys().copied().collect();
                        capture_ctrl.set_active_edges(&edges);
                        dnd.set_active_edges(&edges, config.dead_corner_px);
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                    }
                }

                if !peers.is_empty() {
                    let mut best_rtt: Option<f32> = None;
                    for peer in peers.values_mut() {
                        let rtt_ms = peer.conn.rtt().as_secs_f32() * 1000.0;
                        peer.rtt_ms = Some(rtt_ms);
                        best_rtt = Some(match best_rtt {
                            Some(cur) => cur.min(rtt_ms),
                            None => rtt_ms,
                        });
                        let stats = peer.conn.stats();
                        let lost_delta = stats
                            .path
                            .lost_packets
                            .saturating_sub(peer.last_lost_packets);
                        let cong_delta = stats
                            .path
                            .congestion_events
                            .saturating_sub(peer.last_congestion_events);
                        peer.last_lost_packets = stats.path.lost_packets;
                        peer.last_congestion_events = stats.path.congestion_events;
                        if (lost_delta > 0 || cong_delta > 0)
                            && rtt_ms > 30.0
                            && peer
                                .last_lag_warn_at
                                .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(2))
                        {
                            peer.last_lag_warn_at = Some(now);
                            warn!(
                                peer = %peer.name,
                                rtt_ms = format_args!("{rtt_ms:.1}"),
                                lost_packets_delta = lost_delta,
                                congestion_events_delta = cong_delta,
                                "Server QUIC lag spike detected (packet loss/congestion with RTT > 30ms)"
                            );
                        }
                    }
                    sync_runtime_peers(&runtime, &peers, active_remote, locked, best_rtt);
                }
            }

            Some((gen, fresh_ips, from_wake)) = ip_rx.recv() => {
                ip_check_in_flight = ip_check_in_flight.saturating_sub(1);
                if gen != ip_check_gen {
                    continue;
                }
                if fresh_ips != last_local_ips {
                    if !from_wake {
                        info!(
                            old = ?last_local_ips,
                            new = ?fresh_ips,
                            "Server LAN interfaces changed; refreshing discovery beacon and probing peers"
                        );
                    }
                    last_local_ips = fresh_ips.clone();
                    if let Some(ref rt) = runtime {
                        rt.update(|s| {
                            s.local_ips = fresh_ips;
                        });
                    }
                    beacon_refresh.notify_one();
                    if !from_wake {
                        probe_all_peers(&mut peers, Instant::now());
                    }
                }
            }

            Some(ready) = drag_handoff_rx.recv() => {
                let Some(pending) = pending_drag_handoff.as_ref() else {
                    continue;
                };
                if pending.gen != ready.gen
                    || locked
                    || edge_to_peer.get(&ready.edge) != Some(&ready.peer_id)
                {
                    if pending.gen == ready.gen {
                        pending_drag_handoff = None;
                    }
                    continue;
                }
                let normalized_pos = pending.normalized_pos;
                pending_drag_handoff = None;

                if let Some(old_pid) = active_remote.take() {
                    if old_pid != ready.peer_id {
                        if let Some(old_peer) = peers.get(&old_pid) {
                            let _ = old_peer.ctrl_tx.send(ControlMessage::ReleaseAll);
                            let _ = old_peer.ctrl_tx.send(ControlMessage::Leave {
                                edge: old_peer.edge.opposite(),
                                normalized_pos: 0.5,
                                cursor_norm_x: 0.5,
                                cursor_norm_y: 0.5,
                            });
                        }
                    }
                }

                if let Some(peer) = peers.get(&ready.peer_id) {
                    info!(
                        peer = %peer.name,
                        edge = ?ready.edge,
                        normalized_pos,
                        staged_paths = ready.paths.len(),
                        "Switching focus to client for cross-edge drag"
                    );
                    let lock_state = if config.sync_lock_keys {
                        capture_ctrl.lock_state()
                    } else {
                        LockState::default()
                    };
                    active_remote = Some(ready.peer_id);
                    active_edge = Some(ready.edge);
                    remote_modifiers = ModifierMask::EMPTY;
                    pending_dx = 0.0;
                    pending_dy = 0.0;
                    coalesce_armed = false;
                    last_motion_sent = None;
                    let _ = capture_ctrl.enter_remote();
                    let _ = peer.ctrl_tx.send(ControlMessage::Enter {
                        edge: ready.edge.opposite(),
                        normalized_pos,
                        lock_state,
                    });

                    let drag_id = next_drag_id();
                    let peer_id = ready.peer_id;
                    let limit = ready.limit;
                    let paths = ready.paths;
                    outgoing_drag = Some(ActiveOutgoingDrag {
                        state: OutgoingDragState { drag_id, peer_id },
                        collecting: true,
                        drop_requested: false,
                    });
                    let tx = drag_collect_tx.clone();
                    tokio::spawn(async move {
                        let res = tokio::task::spawn_blocking(move || {
                            collect_local_drag_files(&paths, limit)
                        })
                        .await;
                        let (infos, send_list) = match res {
                            Ok(pair) => pair,
                            Err(e) => {
                                warn!(drag_id, error = %e, "Drag file collection task failed");
                                (Vec::new(), Vec::new())
                            }
                        };
                        let _ = tx.send(DragFilesCollected {
                            drag_id,
                            peer_id,
                            infos,
                            send_list,
                            limit,
                        });
                    });
                    sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                }
            }

            Some(collected) = drag_collect_rx.recv() => {
                let matches_active = outgoing_drag.as_ref().is_some_and(|o| {
                    o.collecting
                        && o.state.drag_id == collected.drag_id
                        && o.state.peer_id == collected.peer_id
                });
                if !matches_active {
                    continue;
                }
                if collected.infos.is_empty() {
                    if let Some(out) = outgoing_drag.take() {
                        if out.drop_requested && active_remote == Some(collected.peer_id) {
                            if let Some(peer) = peers.get(&collected.peer_id) {
                                let _ = peer.ctrl_tx.send(ControlMessage::Button {
                                    button: MouseButton::Left,
                                    pressed: false,
                                });
                            }
                        }
                    }
                    continue;
                }
                let drop_requested = outgoing_drag
                    .as_ref()
                    .is_some_and(|o| o.drop_requested);
                if let Some(peer) = peers.get(&collected.peer_id) {
                    let _ = peer.ctrl_tx.send(ControlMessage::DragStart {
                        drag_id: collected.drag_id,
                        files: collected.infos,
                    });
                    spawn_send_drag_files(
                        peer.conn.clone(),
                        collected.drag_id,
                        collected.send_list,
                        collected.limit,
                        runtime.clone(),
                    );
                    if drop_requested {
                        debug!(
                            drag_id = collected.drag_id,
                            peer = %peer.name,
                            "LMB was released while collecting drag files; sending DragDrop"
                        );
                        let _ = peer.ctrl_tx.send(ControlMessage::DragDrop {
                            drag_id: collected.drag_id,
                        });
                        outgoing_drag = None;
                    } else if let Some(ref mut out) = outgoing_drag {
                        out.collecting = false;
                    }
                } else {
                    outgoing_drag = None;
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
                        pin_media_keys_local,
                        clipboard: clip_enabled,
                        drag_drop_files,
                        hotkeys,
                    } => {
                        config.edge_threshold_px = edge_threshold_px;
                        config.dead_corner_px = dead_corner_px;
                        config.edge_dwell_ms = edge_dwell_ms;
                        config.auto_lock_fullscreen = auto_lock_fullscreen;
                        config.sync_lock_keys = sync_lock_keys;
                        config.pin_media_keys_local = pin_media_keys_local;
                        config.clipboard = clip_enabled;
                        config.drag_drop_files = drag_drop_files;
                        config.set_switch_shortcut(hotkeys.switch_mods);
                        config.set_lock_shortcut(hotkeys.lock);
                        config.set_escape_shortcut(hotkeys.escape);
                        locked = new_locked;
                        if locked {
                            pending_drag_handoff = None;
                        }
                        capture_ctrl.update_tuning(
                            edge_threshold_px,
                            dead_corner_px,
                            edge_dwell_ms,
                            auto_lock_fullscreen,
                            locked,
                        );
                        capture_ctrl.set_sync_lock_keys(sync_lock_keys);
                        capture_ctrl.set_drag_drop_files(drag_drop_files);
                        capture_ctrl.set_pin_media_keys_local(pin_media_keys_local);
                        capture_ctrl.set_hotkeys(hotkeys);
                        dnd.set_enabled(drag_drop_files);
                        let edges: Vec<Edge> = edge_to_peer.keys().copied().collect();
                        dnd.set_active_edges(&edges, dead_corner_px);
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                    }
                    RuntimeCommand::SetHotkeysSuspended(suspended) => {
                        capture_ctrl.set_hotkeys_suspended(suspended);
                        debug!(suspended, "Updated capture hotkeys suspended state");
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
                        if locked {
                            pending_drag_handoff = None;
                            dnd.set_drag_armed(false);
                        }
                        info!(locked, "Toggled cursor lock");
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                    }
                    RuntimeCommand::SetLock(new_locked) => {
                        locked = new_locked;
                        capture_ctrl.update_tuning(
                            config.edge_threshold_px,
                            config.dead_corner_px,
                            config.edge_dwell_ms,
                            config.auto_lock_fullscreen,
                            locked,
                        );
                        if locked {
                            pending_drag_handoff = None;
                            dnd.set_drag_armed(false);
                        }
                        info!(locked, "Updated cursor lock state");
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                    }
                    RuntimeCommand::SwitchToEdge(edge)
                    | RuntimeCommand::SwitchTarget(Some(edge)) => {
                        pending_drag_handoff = None;
                        if let Some(&peer_id) = edge_to_peer.get(&edge) {
                            if active_remote == Some(peer_id) {
                                continue;
                            }
                            cancel_outgoing_drag(&mut outgoing_drag, &peers);
                            let was_remote = active_remote.is_some();
                            if let Some(prev_pid) = active_remote {
                                if let Some(prev_peer) = peers.get(&prev_pid) {
                                    let _ = prev_peer.ctrl_tx.send(ControlMessage::ReleaseAll);
                                    let _ = prev_peer.ctrl_tx.send(ControlMessage::Leave {
                                        edge: prev_peer.edge.opposite(),
                                        normalized_pos: 0.5,
                                        cursor_norm_x: 0.5,
                                        cursor_norm_y: 0.5,
                                    });
                                }
                            }
                            if let Some(peer) = peers.get(&peer_id) {
                                let lock_state = if config.sync_lock_keys {
                                    capture_ctrl.lock_state()
                                } else {
                                    LockState::default()
                                };
                                active_remote = Some(peer_id);
                                active_edge = Some(edge);
                                remote_modifiers = ModifierMask::EMPTY;
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                last_motion_sent = None;
                                dnd.set_drag_armed(false);
                                // Only enter remote mode on local -> remote transition so we do not
                                // overwrite the saved local cursor position (e.g. Windows last_local_norm).
                                if !was_remote {
                                    let _ = capture_ctrl.enter_remote();
                                }
                                let (nx, ny) = peer.last_cursor_norm;
                                let _ = peer.ctrl_tx.send(ControlMessage::EnterAt {
                                    norm_x: nx,
                                    norm_y: ny,
                                    lock_state,
                                });
                                sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                            }
                        }
                    }
                    RuntimeCommand::ReturnLocal | RuntimeCommand::SwitchTarget(None) => {
                        pending_drag_handoff = None;
                        cancel_outgoing_drag(&mut outgoing_drag, &peers);
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
                            active_edge = None;
                            remote_modifiers = ModifierMask::EMPTY;
                            pending_dx = 0.0;
                            pending_dy = 0.0;
                            coalesce_armed = false;
                            last_motion_sent = None;
                            let (lx, ly) = capture_ctrl.last_local_cursor_norm();
                            let _ = capture_ctrl.leave_remote_at(lx, ly);
                            sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                        }
                    }
                    RuntimeCommand::SendFile { filename, data } => {
                        let limit = max_file_bytes.load(Ordering::Relaxed);
                        if data.len() > limit {
                            warn!(
                                file = %filename,
                                bytes = data.len(),
                                limit,
                                "Refusing to send file exceeding max_file_bytes limit"
                            );
                        } else if peers.is_empty() {
                            warn!(file = %filename, "Cannot send file: no connected peers");
                        } else {
                            let byte_len = data.len();
                            let conns: Vec<Connection> =
                                peers.values().map(|p| p.conn.clone()).collect();
                            let rt_opt = runtime.clone();
                            tokio::spawn(async move {
                                let sha256 = sha256_bytes_async(data.clone()).await;
                                let payload = FilePayload {
                                    filename: filename.clone(),
                                    sha256,
                                    data,
                                };
                                let mut any_sent = false;
                                for conn in conns {
                                    match send_file_on_with_limit(&conn, &payload, limit).await {
                                        Ok(()) => any_sent = true,
                                        Err(e) => warn!(error = %e, "Failed beaming file to client"),
                                    }
                                }
                                if any_sent {
                                    if let Some(rt) = rt_opt {
                                        rt.record_file("sent", filename, byte_len, None);
                                    }
                                }
                            });
                        }
                    }
                    RuntimeCommand::UpdateMaxFileBytes(limit) => {
                        let effective = limit.min(u32::MAX as usize);
                        config.max_file_bytes = Some(effective);
                        max_file_bytes.store(effective, Ordering::Relaxed);
                    }
                    RuntimeCommand::UpdateReceiveDir(dir) => {
                        config.receive_dir = dir;
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
                                    dnd.set_active_edges(&edges, config.dead_corner_px);
                                    sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
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
                    CaptureEvent::LocalLeftButton { pressed } => {
                        local_lmb_down = pressed;
                        if !pressed {
                            dnd.set_drag_armed(false);
                            if let Some(ref mut inc) = incoming_drag {
                                inc.request_drop_active();
                                if inc.is_ready_to_commit() {
                                    if let Some(done) = incoming_drag.take() {
                                        incoming_drag_peer = None;
                                        done.commit(
                                            receive_dir.clone(),
                                            config.receive_dir.clone(),
                                            Arc::clone(&dnd),
                                            runtime.clone(),
                                        );
                                    }
                                }
                            }
                        }
                    }

                    CaptureEvent::DragStripArmed(armed) => {
                        let allow = armed
                            && config.drag_drop_files
                            && active_remote.is_none()
                            && incoming_drag.is_none()
                            && !locked;
                        dnd.set_drag_armed(allow);
                    }

                    CaptureEvent::EdgeCrossed { edge, normalized_pos } => {
                        if locked {
                            pending_drag_handoff = None;
                            pending_dx = 0.0;
                            pending_dy = 0.0;
                            coalesce_armed = false;
                            last_motion_sent = None;
                            dnd.set_drag_armed(false);
                            let _ = capture_ctrl.leave_remote(edge, normalized_pos);
                            continue;
                        }
                        if let Some(ref mut pending) = pending_drag_handoff {
                            if pending.edge == edge {
                                pending.normalized_pos = normalized_pos;
                                continue;
                            }
                        }
                        pending_drag_handoff = None;

                        // If the user dragged an incoming Client -> Server file back across an edge
                        // before releasing LMB, cancel the incoming drag cleanly.
                        if let Some(inc) = incoming_drag.take() {
                            if !inc.drop_requested {
                                debug!(drag_id = inc.drag_id, "Cancelled incoming drag after crossing back to peer");
                                incoming_drag_peer = None;
                                early_drag_files.remove_drag(inc.drag_id);
                                inc.cancel(Arc::clone(&dnd));
                            } else {
                                incoming_drag = Some(inc);
                            }
                        }
                        if let Some(&peer_id) = edge_to_peer.get(&edge) {
                            if let Some(peer) = peers.get(&peer_id) {
                                let staged_paths = if config.drag_drop_files {
                                    dnd.take_staged_files(edge)
                                } else {
                                    None
                                };

                                if let Some(paths) = staged_paths {
                                    // Cancel local OS drag (e.g. Explorer DoDragDrop / Nautilus XDND)
                                    // on the blocking pool BEFORE `enter_remote()` parks the cursor so
                                    // worker threads and the select! loop never stall for the ~20ms unwind.
                                    let limit = max_file_bytes.load(Ordering::Relaxed);
                                    drag_handoff_gen = drag_handoff_gen.wrapping_add(1);
                                    let gen = drag_handoff_gen;
                                    pending_drag_handoff = Some(PendingDragHandoff {
                                        gen,
                                        edge,
                                        peer_id,
                                        normalized_pos,
                                    });
                                    let dnd_c = Arc::clone(&dnd);
                                    let tx = drag_handoff_tx.clone();
                                    tokio::spawn(async move {
                                        let _ = tokio::task::spawn_blocking(move || {
                                            dnd_c.cancel_local_drag();
                                        })
                                        .await;
                                        let _ = tx.send(DragHandoffReady {
                                            gen,
                                            peer_id,
                                            edge,
                                            paths,
                                            limit,
                                        });
                                    });
                                    continue;
                                }

                                dnd.set_drag_armed(false);
                                if let Some(old_pid) = active_remote.take() {
                                    if old_pid != peer_id {
                                        if let Some(old_peer) = peers.get(&old_pid) {
                                            let _ = old_peer.ctrl_tx.send(ControlMessage::ReleaseAll);
                                            let _ = old_peer.ctrl_tx.send(ControlMessage::Leave {
                                                edge: old_peer.edge.opposite(),
                                                normalized_pos: 0.5,
                                                cursor_norm_x: 0.5,
                                                cursor_norm_y: 0.5,
                                            });
                                        }
                                    }
                                }

                                info!(
                                    peer = %peer.name,
                                    ?edge,
                                    normalized_pos,
                                    "Switching focus to client"
                                );
                                let lock_state = if config.sync_lock_keys {
                                    capture_ctrl.lock_state()
                                } else {
                                    LockState::default()
                                };
                                active_remote = Some(peer_id);
                                active_edge = Some(edge);
                                remote_modifiers = ModifierMask::EMPTY;
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                last_motion_sent = None;
                                let _ = capture_ctrl.enter_remote();
                                let _ = peer.ctrl_tx.send(ControlMessage::Enter {
                                    edge: edge.opposite(),
                                    normalized_pos,
                                    lock_state,
                                });
                                sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                            } else {
                                active_edge = None;
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                last_motion_sent = None;
                                let _ = capture_ctrl.leave_remote(edge, normalized_pos);
                            }
                        } else {
                            pending_dx = 0.0;
                            pending_dy = 0.0;
                            coalesce_armed = false;
                            last_motion_sent = None;
                            dnd.set_drag_armed(false);
                            let _ = capture_ctrl.leave_remote(edge, normalized_pos);
                        }
                    }

                    CaptureEvent::HotkeySwitch { direction } => {
                        pending_drag_handoff = None;
                        if let Some(current_pid) = active_remote {
                            // If pressing opposite direction of current remote edge, return to server with cursor memory!
                            if active_edge.is_some_and(|e| e.opposite() == direction) {
                                cancel_outgoing_drag(&mut outgoing_drag, &peers);
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
                                remote_modifiers = ModifierMask::EMPTY;
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                last_motion_sent = None;
                                let (lx, ly) = capture_ctrl.last_local_cursor_norm();
                                let _ = capture_ctrl.leave_remote_at(lx, ly);
                                sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
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
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                last_motion_sent = None;
                                dnd.set_drag_armed(false);
                                let _ = capture_ctrl.enter_remote();
                                let (nx, ny) = peer.last_cursor_norm;
                                let _ = peer.ctrl_tx.send(ControlMessage::EnterAt {
                                    norm_x: nx,
                                    norm_y: ny,
                                    lock_state,
                                });
                                sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
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
                        if locked {
                            pending_drag_handoff = None;
                            dnd.set_drag_armed(false);
                        }
                        info!(locked, "Toggled cursor lock via shortcut");
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                    }

                    CaptureEvent::MouseDelta { dx, dy } => {
                        if active_remote.is_some() {
                            pending_dx += dx;
                            pending_dy += dy;
                            let leading_edge_ready = config.motion_coalesce_us == 0
                                || last_motion_sent
                                    .is_none_or(|t| t.elapsed() >= coalesce_dur);
                            if leading_edge_ready {
                                if let Some(retry_delay) = flush_pending_motion(
                                    &peers,
                                    active_remote,
                                    &mut pending_dx,
                                    &mut pending_dy,
                                    &mut seq,
                                    &mut coalesce_armed,
                                    &mut last_motion_sent,
                                    &mut dgram_enc,
                                ) {
                                    coalesce_sleep
                                        .as_mut()
                                        .reset(tokio::time::Instant::now() + retry_delay);
                                }
                            } else if !coalesce_armed {
                                let remaining = last_motion_sent
                                    .map(|t| coalesce_dur.saturating_sub(t.elapsed()))
                                    .unwrap_or(coalesce_dur);
                                coalesce_armed = true;
                                coalesce_sleep
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + remaining);
                            }
                        }
                    }

                    CaptureEvent::MouseScroll { dx_120, dy_120 } => {
                        if let Some(retry_delay) = flush_pending_motion(
                            &peers,
                            active_remote,
                            &mut pending_dx,
                            &mut pending_dy,
                            &mut seq,
                            &mut coalesce_armed,
                            &mut last_motion_sent,
                            &mut dgram_enc,
                        ) {
                            coalesce_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + retry_delay);
                        }
                        if let Some(pid) = active_remote {
                            if let Some(peer) = peers.get(&pid) {
                                seq = seq.wrapping_add(1);
                                let _ = peer.conn.send_datagram(dgram_enc.encode(
                                    DatagramMessage::MouseScroll { seq, dx_120, dy_120 },
                                ));
                            }
                        }
                    }

                    CaptureEvent::MouseButton { button, pressed } => {
                        if button == MouseButton::Left {
                            local_lmb_down = pressed;
                        }
                        if let Some(retry_delay) = flush_pending_motion(
                            &peers,
                            active_remote,
                            &mut pending_dx,
                            &mut pending_dy,
                            &mut seq,
                            &mut coalesce_armed,
                            &mut last_motion_sent,
                            &mut dgram_enc,
                        ) {
                            coalesce_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + retry_delay);
                        }
                        if button == MouseButton::Left && !pressed {
                            if let Some(ref mut out) = outgoing_drag {
                                if out.collecting {
                                    out.drop_requested = true;
                                    continue;
                                }
                            }
                            if let Some(out) = outgoing_drag.take() {
                                if let Some(peer) = peers.get(&out.state.peer_id) {
                                    debug!(drag_id = out.state.drag_id, peer = %peer.name, "LMB released on client; sending DragDrop");
                                    let _ = peer.ctrl_tx.send(ControlMessage::DragDrop {
                                        drag_id: out.state.drag_id,
                                    });
                                }
                                continue;
                            }
                        }
                        if let Some(pid) = active_remote {
                            if let Some(peer) = peers.get(&pid) {
                                let _ = peer.ctrl_tx.send(ControlMessage::Button { button, pressed });
                            }
                        }
                    }

                    CaptureEvent::Key { usage, pressed } => {
                        if pressed && usage == HidUsage::keyboard(0x29) && outgoing_drag.is_some() {
                            if let Some(ref out) = outgoing_drag {
                                debug!(drag_id = out.state.drag_id, "Escape pressed during drag; cancelling");
                            }
                            cancel_outgoing_drag(&mut outgoing_drag, &peers);
                            continue;
                        }
                        if let Some(retry_delay) = flush_pending_motion(
                            &peers,
                            active_remote,
                            &mut pending_dx,
                            &mut pending_dy,
                            &mut seq,
                            &mut coalesce_armed,
                            &mut last_motion_sent,
                            &mut dgram_enc,
                        ) {
                            coalesce_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + retry_delay);
                        }
                        if let Some(pid) = active_remote {
                            if let Some(peer) = peers.get(&pid) {
                                remote_modifiers.update(usage, pressed);
                                let _ = peer.ctrl_tx.send(ControlMessage::Key { usage, pressed });
                            }
                        }
                    }

                    CaptureEvent::EscapeHotkey => {
                        pending_drag_handoff = None;
                        cancel_outgoing_drag(&mut outgoing_drag, &peers);
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
                        last_motion_sent = None;
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                    }

                    CaptureEvent::GeometryChanged(new_geom) => {
                        let new_scale = new_geom.primary_scale();
                        server_scale.store(new_scale.to_bits(), Ordering::Relaxed);
                        info!(
                            monitors = new_geom.monitors.len(),
                            primary_scale = new_scale,
                            "Server desktop geometry updated live"
                        );
                        if let Some(ref rt) = runtime {
                            rt.update(|s| {
                                s.geometry = new_geom;
                            });
                        }
                    }

                    CaptureEvent::SystemSuspended => {
                        pending_drag_handoff = None;
                        cancel_outgoing_drag(&mut outgoing_drag, &peers);
                        if let Some(pid) = active_remote.take() {
                            if let Some(peer) = peers.get(&pid) {
                                info!(
                                    peer = %peer.name,
                                    "System suspending/locking; releasing remote control and held keys"
                                );
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
                        remote_modifiers = ModifierMask::EMPTY;
                        pending_dx = 0.0;
                        pending_dy = 0.0;
                        coalesce_armed = false;
                        last_motion_sent = None;
                        let (lx, ly) = capture_ctrl.last_local_cursor_norm();
                        let _ = capture_ctrl.leave_remote_at(lx, ly);
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                    }

                    CaptureEvent::SystemResumed => {
                        let now = Instant::now();
                        sleep_detector.reset();
                        pending_drag_handoff = None;
                        cancel_outgoing_drag(&mut outgoing_drag, &peers);
                        info!("System resumed/unlocked; refreshing server geometry, LAN IPs, discovery beacon, and probing peers");
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
                            active_edge = None;
                            remote_modifiers = ModifierMask::EMPTY;
                            pending_dx = 0.0;
                            pending_dy = 0.0;
                            coalesce_armed = false;
                            last_motion_sent = None;
                            let (lx, ly) = capture_ctrl.last_local_cursor_norm();
                            let _ = capture_ctrl.leave_remote_at(lx, ly);
                        }
                        let fresh_geom = capture_ctrl.desktop_geometry();
                        server_scale.store(fresh_geom.primary_scale().to_bits(), Ordering::Relaxed);
                        last_ip_check = now;
                        ip_check_gen = ip_check_gen.wrapping_add(1);
                        ip_check_in_flight += 1;
                        let gen = ip_check_gen;
                        let tx = ip_tx.clone();
                        tokio::spawn(async move {
                            let ips = local_ipv4_addresses_async().await;
                            let _ = tx.send((gen, ips, true));
                        });
                        if let Some(ref rt) = runtime {
                            rt.update(|s| {
                                s.geometry = fresh_geom;
                            });
                        }
                        beacon_refresh.notify_one();
                        probe_all_peers(&mut peers, now);
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
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
                            if pending_drag_handoff
                                .as_ref()
                                .is_some_and(|p| p.peer_id == old_id)
                            {
                                pending_drag_handoff = None;
                            }
                            if active_remote == Some(old_id) {
                                active_remote = None;
                                remote_modifiers = ModifierMask::EMPTY;
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                last_motion_sent = None;
                                if let Some(edge) = active_edge.take() {
                                    let _ = capture_ctrl.leave_remote(edge, 0.5);
                                }
                            }
                            if outgoing_drag
                                .as_ref()
                                .is_some_and(|o| o.state.peer_id == old_id)
                            {
                                outgoing_drag = None;
                            }
                            if incoming_drag_peer == Some(old_id) {
                                incoming_drag_peer = None;
                                early_drag_files.clear();
                                if let Some(inc) = incoming_drag.take() {
                                    if !inc.drop_requested {
                                        inc.cancel(Arc::clone(&dnd));
                                    } else {
                                        incoming_drag = Some(inc);
                                    }
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
                        dnd.set_active_edges(&edges, config.dead_corner_px);
                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                    }

                    PeerEvent::Control { peer_id, msg } => {
                        if let Some(peer) = peers.get_mut(&peer_id) {
                            peer.health_probe.mark_alive();
                        }
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
                                // Dragging back to the origin PC before releasing LMB cancels the drag!
                                if outgoing_drag
                                    .as_ref()
                                    .is_some_and(|o| o.state.peer_id == peer_id)
                                {
                                    if let Some(ref out) = outgoing_drag {
                                        if let Some(peer) = peers.get(&peer_id) {
                                            debug!(drag_id = out.state.drag_id, peer = %peer.name, "Dragged back to origin; cancelling drag");
                                        }
                                    }
                                    cancel_outgoing_drag(&mut outgoing_drag, &peers);
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
                                        last_motion_sent = None;
                                        remote_modifiers = ModifierMask::EMPTY;
                                        let _ = capture_ctrl.leave_remote(return_edge, normalized_pos);
                                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                                    }
                                }
                            }
                            ControlMessage::DragStart { drag_id, files } => {
                                if config.drag_drop_files {
                                    debug!(peer_id, drag_id, count = files.len(), "Incoming cross-edge drag from client");
                                    if let Some(prev) = incoming_drag.take() {
                                        if !prev.drop_requested {
                                            prev.cancel(Arc::clone(&dnd));
                                        }
                                    }
                                    let limit = max_file_bytes.load(Ordering::Relaxed);
                                    if let Some(mut inc) = IncomingDragState::start(drag_id, files, limit) {
                                        for buffered in early_drag_files.drain_for(drag_id) {
                                            inc.push_file(drag_id, buffered);
                                        }
                                        if !local_lmb_down {
                                            // Physical LMB was already released during edge handoff.
                                            inc.request_drop_active();
                                        }
                                        if inc.is_ready_to_commit() {
                                            incoming_drag = None;
                                            incoming_drag_peer = None;
                                            inc.commit(
                                                receive_dir.clone(),
                                                config.receive_dir.clone(),
                                                Arc::clone(&dnd),
                                                runtime.clone(),
                                            );
                                        } else {
                                            if !inc.drop_requested {
                                                let (nx, ny) = capture_ctrl.last_local_cursor_norm();
                                                let (px, py) = capture_ctrl
                                                    .desktop_geometry()
                                                    .denormalize_point(nx, ny);
                                                dnd.on_incoming_drag_start(&inc.filenames, px, py);
                                                inc.maybe_start_live_staging(
                                                    receive_dir.clone(),
                                                    config.receive_dir.clone(),
                                                    Arc::clone(&dnd),
                                                );
                                            }
                                            incoming_drag = Some(inc);
                                            incoming_drag_peer = Some(peer_id);
                                        }
                                    }
                                }
                            }
                            ControlMessage::DragDrop { drag_id } => {
                                if let Some(ref mut inc) = incoming_drag {
                                    if inc.request_drop(drag_id) && inc.is_ready_to_commit() {
                                        if let Some(done) = incoming_drag.take() {
                                            incoming_drag_peer = None;
                                            done.commit(
                                                receive_dir.clone(),
                                                config.receive_dir.clone(),
                                                Arc::clone(&dnd),
                                                runtime.clone(),
                                            );
                                        }
                                    }
                                }
                            }
                            ControlMessage::DragCancel { drag_id } => {
                                early_drag_files.remove_drag(drag_id);
                                if incoming_drag.as_ref().is_some_and(|i| i.drag_id == drag_id) {
                                    debug!(peer_id, drag_id, "Cancelled incoming cross-edge drag from client");
                                    incoming_drag_peer = None;
                                    if let Some(inc) = incoming_drag.take() {
                                        inc.cancel(Arc::clone(&dnd));
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
                                        dnd.set_active_edges(&edges, config.dead_corner_px);
                                        sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
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
                        if let Some(peer) = peers.get_mut(&peer_id) {
                            peer.health_probe.mark_alive();
                        }
                        match transfer {
                            BulkTransfer::Clipboard(payload) => {
                                if !config.clipboard {
                                    continue;
                                }
                                debug!(peer_id, format = ?payload.format, len = payload.data.len(), "Received clipboard from client");
                                if let Some(ref rt) = runtime {
                                    rt.record_clipboard("received", &payload);
                                }
                                if let Some(ref tx) = clip_apply_tx {
                                    let _ = tx.send(payload.clone());
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
                                let filename = file.filename;
                                let data = file.data;
                                let byte_len = data.len();
                                let rt_clone = runtime.clone();
                                let rdir_override = receive_dir.clone();
                                let cfg_receive_dir = config.receive_dir.clone();
                                tokio::spawn(async move {
                                    let fname_for_save = filename.clone();
                                    let saved = tokio::task::spawn_blocking(move || {
                                        let dir_res = match rdir_override {
                                            Some(d) => Ok(d),
                                            None => resolve_receive_dir(cfg_receive_dir.as_deref()),
                                        };
                                        dir_res.and_then(|dir| {
                                            save_received_file(&dir, &fname_for_save, &data)
                                        })
                                    })
                                    .await;
                                    match saved {
                                        Ok(Ok(path)) => {
                                            info!(file = %filename, path = %path.display(), bytes = byte_len, "Received file from client");
                                            if let Some(rt) = rt_clone {
                                                rt.record_file(
                                                    "received",
                                                    filename,
                                                    byte_len,
                                                    Some(path),
                                                );
                                            }
                                        }
                                        Ok(Err(e)) => {
                                            warn!(error = %e, "Failed saving received file")
                                        }
                                        Err(e) => {
                                            warn!(error = %e, "Received file save task failed")
                                        }
                                    }
                                });
                            }
                            BulkTransfer::DragFile(drag_file) => {
                                if !config.drag_drop_files {
                                    continue;
                                }
                                if incoming_drag
                                    .as_ref()
                                    .is_some_and(|i| i.drag_id == drag_file.drag_id)
                                {
                                    if let Some(ref mut inc) = incoming_drag {
                                        if inc.push_file(drag_file.drag_id, drag_file.file) {
                                            if inc.is_ready_to_commit() {
                                                if let Some(done) = incoming_drag.take() {
                                                    incoming_drag_peer = None;
                                                    done.commit(
                                                        receive_dir.clone(),
                                                        config.receive_dir.clone(),
                                                        Arc::clone(&dnd),
                                                        runtime.clone(),
                                                    );
                                                }
                                            } else {
                                                inc.maybe_start_live_staging(
                                                    receive_dir.clone(),
                                                    config.receive_dir.clone(),
                                                    Arc::clone(&dnd),
                                                );
                                            }
                                        }
                                    }
                                } else {
                                    early_drag_files.push(drag_file.drag_id, drag_file.file);
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
                            dnd.set_active_edges(&edges, config.dead_corner_px);
                            if pending_drag_handoff
                                .as_ref()
                                .is_some_and(|p| p.peer_id == peer_id)
                            {
                                pending_drag_handoff = None;
                            }
                            if outgoing_drag
                                .as_ref()
                                .is_some_and(|o| o.state.peer_id == peer_id)
                            {
                                outgoing_drag = None;
                            }
                            if incoming_drag_peer == Some(peer_id) {
                                incoming_drag_peer = None;
                                early_drag_files.clear();
                                if let Some(inc) = incoming_drag.take() {
                                    if !inc.drop_requested {
                                        inc.cancel(Arc::clone(&dnd));
                                    } else {
                                        incoming_drag = Some(inc);
                                    }
                                }
                            }

                            if active_remote == Some(peer_id) {
                                active_remote = None;
                                active_edge = None;
                                pending_dx = 0.0;
                                pending_dy = 0.0;
                                coalesce_armed = false;
                                last_motion_sent = None;
                                remote_modifiers = ModifierMask::EMPTY;
                                let _ = capture_ctrl.leave_remote(peer.edge, 0.5);
                            }
                            sync_runtime_peers(&runtime, &peers, active_remote, locked, None);
                        }
                    }
                }
            }

            clip_ev = async {
                match clip_local_rx.as_mut() {
                    Some(rx) => rx.recv().await,
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

    if let Some(inc) = incoming_drag.take() {
        if !inc.drop_requested {
            inc.cancel(Arc::clone(&dnd));
        }
    }
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
        let (lx, ly) = capture_ctrl.last_local_cursor_norm();
        let _ = capture_ctrl.leave_remote_at(lx, ly);
    }

    drop(_clip_apply_guard);
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

    let _ctrl_writer_guard = AbortOnDrop(tokio::spawn(async move {
        while let Some(msg) = ctrl_out_rx.recv().await {
            if ctrl_send.send(&msg).await.is_err() {
                break;
            }
        }
    }));

    let bulk_conn = conn.clone();
    let bulk_tx = peer_tx.clone();
    let bulk_limit = Arc::clone(&max_file_bytes);
    let _bulk_guard = AbortOnDrop(tokio::spawn(async move {
        let sem = Arc::new(tokio::sync::Semaphore::new(4));
        loop {
            let uni = match bulk_conn.accept_uni().await {
                Ok(u) => u,
                Err(_) => break,
            };
            let Ok(permit) = sem.clone().acquire_owned().await else {
                break;
            };
            let tx = bulk_tx.clone();
            let limit = Arc::clone(&bulk_limit);
            tokio::spawn(async move {
                let _permit = permit;
                match recv_bulk_stream_with_atomic_limit(uni, &limit).await {
                    Ok(transfer) => {
                        let _ = tx.send(PeerEvent::Bulk { peer_id, transfer });
                    }
                    Err(e) => {
                        warn!(peer_id, error = %e, "Failed receiving bulk stream from client");
                    }
                }
            });
        }
    }));

    if peer_tx
        .send(PeerEvent::Connected {
            peer: ConnectedPeer {
                id: peer_id,
                name: peer_name,
                edge,
                os,
                conn: conn.clone(),
                ctrl_tx: ctrl_out_tx,
                last_cursor_norm: (0.5, 0.5),
                rtt_ms: None,
                health_probe: HealthProbe::default(),
                last_lost_packets: 0,
                last_congestion_events: 0,
                last_lag_warn_at: None,
            },
        })
        .is_err()
    {
        return Ok(());
    }

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
        hotkeys: config.parsed_hotkeys(),
        edge_threshold_px: config.edge_threshold_px,
        dead_corner_px: config.dead_corner_px,
        edge_dwell_ms: config.edge_dwell_ms,
        auto_lock_fullscreen: config.auto_lock_fullscreen,
        sync_lock_keys: config.sync_lock_keys,
        drag_drop_files: config.drag_drop_files,
        pin_media_keys_local: config.pin_media_keys_local,
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

    let dnd = glide_backend::start_dnd();

    let os_label = match glide_backend::current_os_kind() {
        OsKind::Windows => "windows",
        OsKind::LinuxX11 => "linux-x11",
        OsKind::LinuxWayland => "linux-wayland",
        OsKind::MacOs => "macos",
        OsKind::Unknown => "unknown",
    };

    run_server_with_dnd(
        config, endpoint, capture, clipboard, dnd, None, os_label, runtime, cmd_rx,
    )
    .await
}

/// Convenience CLI entrypoint using native OS backends.
pub async fn run_native_server(config: ServerConfig) -> Result<()> {
    run_native_server_with_runtime(config, None, None).await
}
