//! Platform-agnostic Glide Client state machine (v0.2).
//!
//! Features:
//! - Zero-config LocalSend-style LAN auto-discovery + automatic reconnect
//! - Live hot-reloadable sensitivity, scroll speed, natural scroll, auto-DPI scale,
//!   dead-corner guard, lock-key sync, and per-client key remapping
//! - Lazy on-demand clipboard fetching for large (> 256 KiB) images
//! - File transfer reception (into `~/Downloads/Glide`) and sending over QUIC bulk streams
//! - Optional Linux `/dev/uinput` hardware-native HID backend
//! - Guaranteed `release_all()` on leave or disconnect

use crate::config::{compile_key_remaps, ClientConfig, DEFAULT_QUIC_PORT};
use crate::resilience::{HealthProbe, ReconnectBackoff, SleepDetector};
use crate::runtime::{save_received_file, RuntimeCommand, RuntimeState};
use anyhow::{anyhow, Context, Result};
use glide_backend::os_integration::file_receive_dir;
use glide_backend::{ClipboardHandle, InputInjector};
use glide_net::discovery::{discover_servers, local_ipv4_addresses};
use glide_net::tls::{normalize_fingerprint, TrustMode};
use glide_net::{connect_client, recv_bulk_on, send_clipboard_on, send_file_on, sha256_bytes};
use glide_proto::{
    BulkTransfer, ClipboardPayload, ControlMessage, DatagramMessage, FilePayload,
    LAZY_CLIPBOARD_THRESHOLD, PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::net::lookup_host;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const DNS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
const ROAM_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(900);

enum ResolveOutcome {
    Addr(SocketAddr),
    Shutdown,
}

enum RoamLookup {
    Found(SocketAddr),
    NotFound,
    Shutdown,
}

enum WaitOutcome {
    Retry,
    FastRetry,
    Shutdown,
}

async fn recv_cmd_opt(
    cmd_rx: &mut Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Option<RuntimeCommand> {
    match cmd_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Applies non-fatal runtime commands received while disconnected/connecting and returns
/// `true` if the client should shut down immediately.
fn apply_pre_session_cmd(cmd: Option<RuntimeCommand>, config: &mut ClientConfig) -> bool {
    match cmd {
        None | Some(RuntimeCommand::Shutdown) => true,
        Some(RuntimeCommand::UpdateClientTuning {
            sensitivity,
            scroll_speed,
            invert_scroll,
            auto_dpi_scale,
            edge_threshold_px,
            dead_corner_px,
            sync_lock_keys,
            clipboard,
            key_remap,
        }) => {
            config.sensitivity = sensitivity;
            config.scroll_speed = scroll_speed;
            config.invert_scroll = invert_scroll;
            config.auto_dpi_scale = auto_dpi_scale;
            config.edge_threshold_px = edge_threshold_px;
            config.dead_corner_px = dead_corner_px;
            config.sync_lock_keys = sync_lock_keys;
            config.clipboard = clipboard;
            config.key_remap = key_remap;
            false
        }
        Some(RuntimeCommand::MovePeerEdge { edge, .. }) => {
            config.edge = edge;
            false
        }
        Some(_) => false,
    }
}

/// Returns the normalized 64-hex-char SHA-256 fingerprint of the expected server, either from
/// `config.trusted_fingerprint` or (when using TOFU) from `config.resolved_tofu_path()`.
fn known_server_fingerprint(config: &ClientConfig) -> Option<String> {
    let raw = if let Some(ref fp) = config.trusted_fingerprint {
        fp.clone()
    } else if !config.insecure_skip_verify {
        std::fs::read_to_string(config.resolved_tofu_path()).ok()?
    } else {
        return None;
    };
    let norm = normalize_fingerprint(&raw);
    (norm.len() == 64).then_some(norm)
}

/// Resolves an explicit `config.server` address without blocking indefinitely.
async fn resolve_explicit_addr(raw: &str) -> Result<SocketAddr> {
    let trimmed = raw.trim();
    // Fast offline-safe paths: parse literal SocketAddr or bare IPv4/IPv6 without touching OS DNS!
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, DEFAULT_QUIC_PORT));
    }
    let with_port = if trimmed.contains(':') {
        trimmed.to_string()
    } else {
        format!("{trimmed}:{DEFAULT_QUIC_PORT}")
    };
    let mut addrs = tokio::time::timeout(DNS_RESOLVE_TIMEOUT, lookup_host(&with_port))
        .await
        .map_err(|_| anyhow!("timed out resolving server address '{with_port}'"))?
        .with_context(|| format!("failed to resolve server address '{with_port}'"))?;
    addrs
        .next()
        .ok_or_else(|| anyhow!("no IP address resolved for '{with_port}'"))
}

/// Searches the local network for a server matching `expected_norm_fp` (used when a server
/// roams between Wi-Fi and Ethernet and acquires a new LAN IP). Interruptible by `Shutdown`.
async fn discover_roamed_server_by_fingerprint(
    config: &mut ClientConfig,
    expected_norm_fp: &str,
    cmd_rx: &mut Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> RoamLookup {
    let discover_fut = discover_servers(
        config.discovery_port,
        ROAM_DISCOVERY_TIMEOUT,
        None,
        false,
    );
    tokio::pin!(discover_fut);
    loop {
        tokio::select! {
            cmd = recv_cmd_opt(cmd_rx) => {
                if apply_pre_session_cmd(cmd, config) {
                    return RoamLookup::Shutdown;
                }
            }
            res = &mut discover_fut => {
                let found = res.unwrap_or_default();
                if let Some(srv) = found
                    .into_iter()
                    .find(|s| normalize_fingerprint(&s.fingerprint) == expected_norm_fp)
                {
                    return RoamLookup::Found(srv.addr);
                }
                return RoamLookup::NotFound;
            }
        }
    }
}

/// Resolves the target server socket address either from explicit `config.server`
/// or by auto-discovering a running Glide server on the local network (checking `cmd_rx`
/// for `RuntimeCommand::Shutdown` so stopping the client never stalls).
async fn resolve_server_addr(
    config: &mut ClientConfig,
    cmd_rx: &mut Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<ResolveOutcome> {
    if let Some(raw) = config.server.clone() {
        let resolve_fut = resolve_explicit_addr(&raw);
        tokio::pin!(resolve_fut);
        loop {
            tokio::select! {
                cmd = recv_cmd_opt(cmd_rx) => {
                    if apply_pre_session_cmd(cmd, config) {
                        return Ok(ResolveOutcome::Shutdown);
                    }
                }
                res = &mut resolve_fut => {
                    return Ok(ResolveOutcome::Addr(res?));
                }
            }
        }
    }

    info!(
        discovery_port = config.discovery_port,
        filter = ?config.server_name,
        "Searching LAN for a running Glide server..."
    );
    let server_name_filter = config.server_name.clone();
    loop {
        let discover_fut = discover_servers(
            config.discovery_port,
            Duration::from_millis(1500),
            server_name_filter.as_deref(),
            true,
        );
        tokio::pin!(discover_fut);
        loop {
            tokio::select! {
                cmd = recv_cmd_opt(cmd_rx) => {
                    if apply_pre_session_cmd(cmd, config) {
                        return Ok(ResolveOutcome::Shutdown);
                    }
                }
                res = &mut discover_fut => {
                    let found = res?;
                    if let Some(srv) = found.into_iter().next() {
                        info!(
                            name = %srv.name,
                            os = %srv.os,
                            addr = %srv.addr,
                            fingerprint = %srv.fingerprint,
                            "Auto-discovered Glide server on LAN"
                        );
                        return Ok(ResolveOutcome::Addr(srv.addr));
                    }
                    break;
                }
            }
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
    config: &mut ClientConfig,
    server_addr: SocketAddr,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    runtime: Option<&RuntimeState>,
    cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    run_client_session_tracked(config, server_addr, injector, clipboard, runtime, cmd_rx)
        .await
        .1
}

/// Internal session runner that also reports whether the handshake completed successfully so
/// the reconnect supervisor can reset backoff immediately after any previously healthy session.
async fn run_client_session_tracked(
    config: &mut ClientConfig,
    server_addr: SocketAddr,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    runtime: Option<&RuntimeState>,
    mut cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> (bool, Result<()>) {
    let trust = build_trust_mode(config);
    let mut session = {
        let connect_fut = tokio::time::timeout(CONNECT_TIMEOUT, connect_client(server_addr, trust));
        tokio::pin!(connect_fut);
        loop {
            tokio::select! {
                cmd = recv_cmd_opt(&mut cmd_rx) => {
                    if apply_pre_session_cmd(cmd, config) {
                        return (false, Ok(()));
                    }
                }
                res = &mut connect_fut => {
                    match res {
                        Ok(Ok(s)) => break s,
                        Ok(Err(e)) => return (false, Err(e)),
                        Err(_) => {
                            return (
                                false,
                                Err(anyhow!("timed out connecting to Glide server {server_addr}")),
                            );
                        }
                    }
                }
            }
        }
    };

    let client_geom = injector.desktop_geometry().unwrap_or_default();
    let mut client_scale = client_geom.primary_scale();

    if let Err(e) = session
        .ctrl_send
        .send(&ControlMessage::Hello {
            version: PROTOCOL_VERSION,
            peer_name: config.name.clone(),
            edge_on_server: config.edge,
            os: injector.os_kind(),
        })
        .await
    {
        session.conn.close(0u32.into(), b"hello failed");
        return (false, Err(e));
    }

    let welcome = {
        let welcome_fut = tokio::time::timeout(Duration::from_secs(5), session.ctrl_recv.recv());
        tokio::pin!(welcome_fut);
        loop {
            tokio::select! {
                cmd = recv_cmd_opt(&mut cmd_rx) => {
                    if apply_pre_session_cmd(cmd, config) {
                        session.conn.close(0u32.into(), b"client shutdown");
                        return (false, Ok(()));
                    }
                }
                res = &mut welcome_fut => {
                    match res {
                        Ok(Ok(w)) => break w,
                        Ok(Err(e)) => {
                            session.conn.close(0u32.into(), b"welcome failed");
                            return (false, Err(e));
                        }
                        Err(_) => {
                            session.conn.close(0u32.into(), b"welcome timeout");
                            return (false, Err(anyhow!("timed out waiting for server Welcome")));
                        }
                    }
                }
            }
        }
    };
    let (server_name, server_scale) = match welcome {
        ControlMessage::Welcome {
            server_name,
            accepted: true,
            server_scale,
            ..
        } => {
            info!(
                server = %server_name,
                addr = %server_addr,
                edge = ?config.edge,
                sensitivity = config.sensitivity,
                server_scale,
                client_scale,
                "Connected to Glide server"
            );
            (server_name, server_scale.max(0.5))
        }
        ControlMessage::Welcome {
            accepted: false,
            reason,
            ..
        } => {
            session.conn.close(0u32.into(), b"rejected");
            return (false, Err(anyhow!("server rejected connection: {reason}")));
        }
        other => {
            session.conn.close(0u32.into(), b"bad handshake");
            return (
                false,
                Err(anyhow!("unexpected handshake response: {other:?}")),
            );
        }
    };

    let mut last_local_ips = local_ipv4_addresses();
    if let Some(rt) = runtime {
        rt.update(|s| {
            s.mode = "client".into();
            s.status = format!("Connected to {server_name}");
            s.active_target = server_name.clone();
            s.local_addr = Some(server_addr.to_string());
            s.local_ips = last_local_ips.clone();
            s.geometry = client_geom.clone();
            s.last_error = None;
        });
    }

    let mut return_edge = config.edge.opposite();
    let mut key_remaps = compile_key_remaps(&config.key_remap);
    let mut focused = false;
    let mut last_seq: Option<u32> = None;
    let mut pending_offer_hash: Option<[u8; 32]> = None;
    let mut local_clip_cache: HashMap<[u8; 32], ClipboardPayload> = HashMap::new();

    let mut ping_interval = tokio::time::interval(Duration::from_millis(500));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping_interval.tick().await;
    let mut reveal_interval = tokio::time::interval(Duration::from_millis(150));
    reveal_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reveal_interval.tick().await;

    let mut sleep_detector = SleepDetector::default();
    let mut health_probe = HealthProbe::default();
    let mut last_ip_check = Instant::now();

    let conn = session.conn;
    let mut ctrl_send = session.ctrl_send;
    let mut ctrl_recv = session.ctrl_recv;

    let session_result: Result<()> = loop {
        let dpi_mult = if config.auto_dpi_scale {
            (client_scale / server_scale).clamp(0.25, 4.0)
        } else {
            1.0
        };
        let eff_sens = (config.sensitivity * dpi_mult).clamp(0.05, 10.0);

        tokio::select! {
            _ = ping_interval.tick() => {
                let now = Instant::now();

                // 1. OS suspend / lock safety: release any held keys and restore local cursor
                if injector.poll_system_suspended() {
                    focused = false;
                    let _ = injector.release_all();
                    let _ = injector.show_cursor();
                    if let Some(rt) = runtime {
                        rt.update(|s| s.active_target = server_name.clone());
                    }
                }

                // 2. Wake / sleep-jump detection: refresh geometry + LAN IPs and actively probe server
                let os_resumed = injector.poll_system_resumed();
                let sleep_jump = sleep_detector.check();
                if os_resumed || sleep_jump.is_some() {
                    info!(
                        os_resumed,
                        ?sleep_jump,
                        "Client detected wake/sleep jump; refreshing geometry & LAN IPs and probing server"
                    );
                    if focused {
                        focused = false;
                        let _ = injector.release_all();
                        let _ = injector.show_cursor();
                    }
                    let fresh_geom = injector.desktop_geometry().ok();
                    let fresh_ips = local_ipv4_addresses();
                    last_local_ips = fresh_ips.clone();
                    last_ip_check = now;
                    if let Some(ref g) = fresh_geom {
                        client_scale = g.primary_scale();
                    }
                    if let Some(rt) = runtime {
                        rt.update(|s| {
                            if let Some(g) = fresh_geom {
                                s.geometry = g;
                            }
                            s.local_ips = fresh_ips;
                            s.active_target = server_name.clone();
                        });
                    }
                    let ts_us = health_probe.start_probe(now);
                    if let Err(e) = ctrl_send.send(&ControlMessage::Ping { ts_us }).await {
                        break Err(anyhow!("server unreachable after resume: {e}"));
                    }
                } else if now.saturating_duration_since(last_ip_check) >= Duration::from_secs(3) {
                    // 3. Wi-Fi <-> Ethernet roaming detection
                    last_ip_check = now;
                    let fresh_ips = local_ipv4_addresses();
                    if fresh_ips != last_local_ips {
                        info!(
                            old = ?last_local_ips,
                            new = ?fresh_ips,
                            "Client LAN interfaces changed; probing server health"
                        );
                        last_local_ips = fresh_ips.clone();
                        if let Some(rt) = runtime {
                            rt.update(|s| s.local_ips = fresh_ips);
                        }
                        let ts_us = health_probe.start_probe(now);
                        if let Err(e) = ctrl_send.send(&ControlMessage::Ping { ts_us }).await {
                            break Err(anyhow!("server unreachable after network change: {e}"));
                        }
                    }
                }

                // 4. Fail fast if QUIC closed or post-resume/roaming health Ping received no reply within 1.5s
                if conn.close_reason().is_some() {
                    break Err(anyhow!("server QUIC connection closed"));
                }
                if health_probe.is_timed_out(now) {
                    conn.close(1u32.into(), b"health probe timeout");
                    break Err(anyhow!("server did not respond to post-resume health ping"));
                }

                // 5. Live monitor hotplug / DPI scaling refresh
                if let Some(new_geom) = injector.poll_geometry_changed() {
                    client_scale = new_geom.primary_scale();
                    info!(
                        monitors = new_geom.monitors.len(),
                        client_scale,
                        "Client desktop geometry updated live"
                    );
                    if let Some(rt) = runtime {
                        rt.update(|s| {
                            s.geometry = new_geom;
                        });
                    }
                }

                if let Some(rt) = runtime {
                    let rtt_ms = conn.rtt().as_secs_f32() * 1000.0;
                    rt.record_rtt(rtt_ms);
                }
            }

            _ = reveal_interval.tick(), if !focused => {
                injector.reveal_if_moved_locally();
            }

            cmd = recv_cmd_opt(&mut cmd_rx) => {
                let Some(cmd) = cmd else {
                    info!("Client command channel closed; shutting down session");
                    break Ok(());
                };
                match cmd {
                    RuntimeCommand::UpdateClientTuning {
                        sensitivity,
                        scroll_speed,
                        invert_scroll,
                        auto_dpi_scale,
                        edge_threshold_px,
                        dead_corner_px,
                        sync_lock_keys,
                        clipboard: clip_enabled,
                        key_remap,
                    } => {
                        config.sensitivity = sensitivity;
                        config.scroll_speed = scroll_speed;
                        config.invert_scroll = invert_scroll;
                        config.auto_dpi_scale = auto_dpi_scale;
                        config.edge_threshold_px = edge_threshold_px;
                        config.dead_corner_px = dead_corner_px;
                        config.sync_lock_keys = sync_lock_keys;
                        config.clipboard = clip_enabled;
                        key_remaps = compile_key_remaps(&key_remap);
                        config.key_remap = key_remap;
                    }
                    RuntimeCommand::SendFile { filename, data } => {
                        let payload = FilePayload {
                            filename: filename.clone(),
                            sha256: sha256_bytes(&data),
                            data,
                        };
                        if let Some(rt) = runtime {
                            rt.record_file("sent", filename, payload.data.len(), None);
                        }
                        let c = conn.clone();
                        tokio::spawn(async move {
                            if let Err(e) = send_file_on(&c, &payload).await {
                                warn!(error = %e, "Failed beaming file to server");
                            }
                        });
                    }
                    RuntimeCommand::ResendClipboard(id) => {
                        if let Some(rt) = runtime {
                            if let Some(payload) = rt.find_clipboard_payload(id) {
                                let c = conn.clone();
                                tokio::spawn(async move {
                                    let _ = send_clipboard_on(&c, &payload).await;
                                });
                            }
                        }
                    }
                    RuntimeCommand::MovePeerEdge { edge, .. } => {
                        config.edge = edge;
                        return_edge = edge.opposite();
                        let _ = ctrl_send
                            .send(&ControlMessage::SetEdge {
                                edge_on_server: edge,
                            })
                            .await;
                        info!(?edge, "Client moved its own edge live");
                    }
                    RuntimeCommand::Shutdown => {
                        info!("Client received graceful shutdown command");
                        break Ok(());
                    }
                    _ => {}
                }
            }

            dgram = conn.read_datagram() => {
                let bytes = match dgram {
                    Ok(b) => b,
                    Err(e) => break Err(anyhow!("datagram stream closed: {e}")),
                };
                health_probe.mark_alive();
                if !focused {
                    continue;
                }
                let Ok(msg) = DatagramMessage::decode(&bytes) else {
                    continue;
                };
                match msg {
                    DatagramMessage::MouseMove { seq, dx, dy } => {
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

                        let sdx = dx * eff_sens;
                        let sdy = dy * eff_sens;
                        match injector.move_relative(
                            sdx,
                            sdy,
                            Some(return_edge),
                            config.edge_threshold_px,
                            config.dead_corner_px,
                        ) {
                            Ok(Some((edge, normalized_pos))) => {
                                debug!(?edge, normalized_pos, "Cursor crossed return edge; handing focus back to server");
                                let (cx, cy) = injector.cursor_norm();
                                focused = false;
                                let _ = injector.release_all();
                                let _ = injector.leave();
                                if let Some(rt) = runtime {
                                    rt.update(|s| s.active_target = server_name.clone());
                                }
                                if let Err(e) = ctrl_send
                                    .send(&ControlMessage::Leave {
                                        edge,
                                        normalized_pos,
                                        cursor_norm_x: cx,
                                        cursor_norm_y: cy,
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
                        let sign = if config.invert_scroll { -1.0 } else { 1.0 };
                        let mult = config.scroll_speed.clamp(0.1, 5.0) * sign;
                        let sdx = ((dx_120 as f32) * mult).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        let sdy = ((dy_120 as f32) * mult).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        if let Err(e) = injector.scroll(sdx, sdy) {
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
                health_probe.mark_alive();
                match msg {
                    ControlMessage::Enter { edge, normalized_pos, lock_state } => {
                        focused = true;
                        last_seq = None;
                        if let Err(e) = injector.enter(edge, normalized_pos) {
                            warn!(error = %e, "Failed positioning cursor on Enter");
                        }
                        if config.sync_lock_keys {
                            let _ = injector.sync_lock_state(lock_state);
                        }
                        // Fetch any lazily offered large clipboard item now that user entered this screen!
                        if config.clipboard {
                            if let Some(sha256) = pending_offer_hash.take() {
                                let _ = ctrl_send
                                    .send(&ControlMessage::ClipboardRequest { sha256 })
                                    .await;
                            }
                        }
                        if let Some(rt) = runtime {
                            rt.update(|s| s.active_target = "Local (Controlling)".into());
                        }
                    }
                    ControlMessage::EnterAt { norm_x, norm_y, lock_state } => {
                        focused = true;
                        last_seq = None;
                        if let Err(e) = injector.enter_at(norm_x, norm_y) {
                            warn!(error = %e, "Failed positioning cursor on EnterAt");
                        }
                        if config.sync_lock_keys {
                            let _ = injector.sync_lock_state(lock_state);
                        }
                        if config.clipboard {
                            if let Some(sha256) = pending_offer_hash.take() {
                                let _ = ctrl_send
                                    .send(&ControlMessage::ClipboardRequest { sha256 })
                                    .await;
                            }
                        }
                        if let Some(rt) = runtime {
                            rt.update(|s| s.active_target = "Local (Controlling)".into());
                        }
                    }
                    ControlMessage::Leave { .. } | ControlMessage::ReleaseAll => {
                        focused = false;
                        let _ = injector.release_all();
                        let _ = injector.leave();
                        if let Some(rt) = runtime {
                            rt.update(|s| s.active_target = server_name.clone());
                        }
                    }
                    ControlMessage::LockSync { lock_state } => {
                        if config.sync_lock_keys {
                            let _ = injector.sync_lock_state(lock_state);
                        }
                    }
                    ControlMessage::SetEdge { edge_on_server } => {
                        config.edge = edge_on_server;
                        return_edge = edge_on_server.opposite();
                        info!(edge = ?edge_on_server, "Server repositioned this client edge live");
                    }
                    ControlMessage::Key { usage, pressed } => {
                        if focused {
                            let mapped = key_remaps
                                .get(&usage.canonicalize())
                                .copied()
                                .unwrap_or(usage);
                            if let Err(e) = injector.key(mapped, pressed) {
                                warn!(?mapped, error = %e, "Failed injecting key");
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
                    ControlMessage::ClipboardOffer { sha256, .. } => {
                        if config.clipboard {
                            if focused {
                                let _ = ctrl_send
                                    .send(&ControlMessage::ClipboardRequest { sha256 })
                                    .await;
                            } else {
                                pending_offer_hash = Some(sha256);
                            }
                        }
                    }
                    ControlMessage::ClipboardRequest { sha256 } => {
                        if let Some(payload) = local_clip_cache.get(&sha256).cloned() {
                            let c = conn.clone();
                            tokio::spawn(async move {
                                let _ = send_clipboard_on(&c, &payload).await;
                            });
                        }
                    }
                    ControlMessage::ModifierSync { .. } => {}
                    ControlMessage::Ping { ts_us } => {
                        let _ = ctrl_send.send(&ControlMessage::Pong { ts_us }).await;
                    }
                    ControlMessage::Pong { .. } => {}
                    _ => {}
                }
            }

            remote_bulk = recv_bulk_on(&conn) => {
                match remote_bulk {
                    Ok(BulkTransfer::Clipboard(payload)) => {
                        health_probe.mark_alive();
                        if config.clipboard {
                            if let Some(rt) = runtime {
                                rt.record_clipboard("received", &payload);
                            }
                            if let Some(ref clip) = clipboard {
                                debug!(format = ?payload.format, len = payload.data.len(), "Applying clipboard from server");
                                if let Err(e) = clip.backend.set(payload) {
                                    warn!(error = %e, "Failed setting local clipboard");
                                }
                            }
                        }
                    }
                    Ok(BulkTransfer::File(file)) => {
                        health_probe.mark_alive();
                        match file_receive_dir()
                            .and_then(|dir| save_received_file(&dir, &file.filename, &file.data))
                        {
                            Ok(saved) => {
                                info!(file = %file.filename, path = %saved.display(), bytes = file.data.len(), "Received file from server");
                                if let Some(rt) = runtime {
                                    rt.record_file("received", file.filename, file.data.len(), Some(saved));
                                }
                            }
                            Err(e) => warn!(error = %e, "Failed saving received file"),
                        }
                    }
                    Err(e) => {
                        if conn.close_reason().is_some() {
                            break Err(anyhow!("server QUIC connection closed: {e}"));
                        }
                        debug!(error = %e, "Remote bulk uni-stream loop ended");
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
                        if let Some(rt) = runtime {
                            rt.record_clipboard("sent", &payload);
                        }
                        if payload.data.len() > LAZY_CLIPBOARD_THRESHOLD {
                            if local_clip_cache.len() > 4 {
                                local_clip_cache.clear();
                            }
                            local_clip_cache.insert(payload.sha256, payload.clone());
                            let _ = ctrl_send
                                .send(&ControlMessage::ClipboardOffer {
                                    format: payload.format,
                                    sha256: payload.sha256,
                                    byte_len: payload.data.len() as u32,
                                })
                                .await;
                        } else {
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
        }
    };

    if session_result.is_ok() {
        conn.close(0u32.into(), b"client shutdown");
    } else if conn.close_reason().is_none() {
        conn.close(0u32.into(), b"client disconnected");
    }
    let _ = injector.release_all();
    let _ = injector.show_cursor();
    (true, session_result)
}

/// Sleeps up to `delay` between reconnect attempts while watching for graceful shutdown,
/// OS wake/sleep jumps, monitor hotplug, or LAN interface changes so wake/roaming retries
/// immediately without waiting for the backoff timer.
async fn wait_reconnect_delay(
    delay: Duration,
    config: &mut ClientConfig,
    injector: &mut dyn InputInjector,
    sleep_detector: &mut SleepDetector,
    last_local_ips: &mut Vec<String>,
    runtime: Option<&RuntimeState>,
    cmd_rx: &mut Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> WaitOutcome {
    let sleep_fut = tokio::time::sleep(delay);
    tokio::pin!(sleep_fut);
    let mut watch_tick = tokio::time::interval(Duration::from_millis(150));
    watch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    watch_tick.tick().await;
    let mut ticks_since_ip_check: u8 = 0;

    loop {
        tokio::select! {
            _ = &mut sleep_fut => return WaitOutcome::Retry,
            cmd = recv_cmd_opt(cmd_rx) => {
                if apply_pre_session_cmd(cmd, config) {
                    return WaitOutcome::Shutdown;
                }
            }
            _ = watch_tick.tick() => {
                if injector.poll_system_suspended() {
                    let _ = injector.release_all();
                    let _ = injector.show_cursor();
                }

                let os_resumed = injector.poll_system_resumed();
                let sleep_jump = sleep_detector.check();
                ticks_since_ip_check = ticks_since_ip_check.saturating_add(1);

                if os_resumed || sleep_jump.is_some() {
                    info!(
                        os_resumed,
                        ?sleep_jump,
                        "Client detected wake/sleep jump during reconnect backoff; retrying immediately"
                    );
                    let fresh_geom = injector.desktop_geometry().ok();
                    let fresh_ips = local_ipv4_addresses();
                    *last_local_ips = fresh_ips.clone();
                    if let Some(rt) = runtime {
                        rt.update(|s| {
                            if let Some(g) = fresh_geom {
                                s.geometry = g;
                            }
                            s.local_ips = fresh_ips;
                        });
                    }
                    return WaitOutcome::FastRetry;
                }

                if let Some(new_geom) = injector.poll_geometry_changed() {
                    if let Some(rt) = runtime {
                        rt.update(|s| s.geometry = new_geom);
                    }
                }

                if ticks_since_ip_check >= 3 {
                    ticks_since_ip_check = 0;
                    let fresh_ips = local_ipv4_addresses();
                    if fresh_ips != *last_local_ips {
                        info!(
                            old = ?last_local_ips,
                            new = ?fresh_ips,
                            "Client LAN interfaces changed during reconnect backoff; retrying immediately"
                        );
                        *last_local_ips = fresh_ips.clone();
                        if let Some(rt) = runtime {
                            rt.update(|s| s.local_ips = fresh_ips);
                        }
                        return WaitOutcome::FastRetry;
                    }
                }
            }
        }
    }
}

/// Runs the Glide client reconnect supervisor with pre-constructed OS (or mock) backends,
/// fast post-wake/roaming backoff reset, fingerprint-verified server roaming recovery,
/// and responsive graceful shutdown.
pub async fn run_client_with_backends(
    mut config: ClientConfig,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    runtime: Option<RuntimeState>,
    mut cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    let geom = injector.desktop_geometry().unwrap_or_default();
    let mut last_local_ips = local_ipv4_addresses();

    if let Some(ref rt) = runtime {
        rt.update(|s| {
            s.mode = "client".into();
            s.status = "Searching LAN for Glide server...".into();
            s.geometry = geom.clone();
            s.local_ips = last_local_ips.clone();
            s.last_error = None;
        });
    }

    let mut backoff = ReconnectBackoff::default();
    let mut sleep_detector = SleepDetector::default();
    let mut roamed_addr: Option<SocketAddr> = None;

    loop {
        let target_addr = if let Some(addr) = roamed_addr {
            if let Some(ref rt) = runtime {
                rt.update(|s| {
                    s.status = format!("Connecting to {addr}...");
                });
            }
            Ok(ResolveOutcome::Addr(addr))
        } else {
            if let Some(ref rt) = runtime {
                rt.update(|s| {
                    s.status = if config.server.is_some() {
                        format!(
                            "Connecting to {}...",
                            config.server.as_deref().unwrap_or("")
                        )
                    } else {
                        "Searching LAN for Glide server...".into()
                    };
                });
            }
            let mut rx_ref = cmd_rx.as_mut();
            resolve_server_addr(&mut config, &mut rx_ref).await
        };

        let mut immediate_retry = false;

        match target_addr {
            Ok(ResolveOutcome::Shutdown) => {
                info!("Client received graceful shutdown while resolving server");
                break;
            }
            Ok(ResolveOutcome::Addr(addr)) => {
                sleep_detector.reset();
                let (established, res) = run_client_session_tracked(
                    &mut config,
                    addr,
                    injector,
                    clipboard,
                    runtime.as_ref(),
                    cmd_rx.as_mut(),
                )
                .await;

                match res {
                    Ok(()) => {
                        info!("Session ended cleanly");
                        break;
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            %addr,
                            established,
                            "Disconnected from server; reconnecting shortly..."
                        );
                        if established {
                            backoff.reset();
                            sleep_detector.reset();
                        } else if sleep_detector.check().is_some() {
                            backoff.reset();
                        }

                        if let Some(ref rt) = runtime {
                            rt.update(|s| {
                                s.status = "Reconnecting...".into();
                                s.last_error = Some(e.to_string());
                            });
                        }
                        let _ = injector.release_all();
                        let _ = injector.show_cursor();

                        // If an explicit (or previously roamed) server address could not be reached
                        // and we know the server's cryptographic fingerprint, check LAN discovery
                        // to follow the server across Wi-Fi <-> Ethernet roaming automatically.
                        if !established {
                            if roamed_addr == Some(addr) {
                                roamed_addr = None;
                            }
                            if config.server.is_some() {
                                if let Some(expected_fp) = known_server_fingerprint(&config) {
                                    let mut rx_ref = cmd_rx.as_mut();
                                    match discover_roamed_server_by_fingerprint(
                                        &mut config,
                                        &expected_fp,
                                        &mut rx_ref,
                                    )
                                    .await
                                    {
                                        RoamLookup::Shutdown => {
                                            info!("Client received shutdown during roaming discovery");
                                            break;
                                        }
                                        RoamLookup::Found(new_addr) if new_addr != addr => {
                                            info!(
                                                old = %addr,
                                                new = %new_addr,
                                                "Discovered pinned Glide server at new LAN address; reconnecting immediately"
                                            );
                                            roamed_addr = Some(new_addr);
                                            backoff.reset();
                                            immediate_retry = true;
                                        }
                                        RoamLookup::Found(_) | RoamLookup::NotFound => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Could not resolve server; retrying...");
                if let Some(ref rt) = runtime {
                    rt.update(|s| s.last_error = Some(e.to_string()));
                }
                if config.server.is_some() {
                    if let Some(expected_fp) = known_server_fingerprint(&config) {
                        let mut rx_ref = cmd_rx.as_mut();
                        match discover_roamed_server_by_fingerprint(
                            &mut config,
                            &expected_fp,
                            &mut rx_ref,
                        )
                        .await
                        {
                            RoamLookup::Shutdown => {
                                info!("Client received shutdown during roaming discovery");
                                break;
                            }
                            RoamLookup::Found(new_addr) => {
                                info!(
                                    new = %new_addr,
                                    "Resolved pinned Glide server via LAN discovery; reconnecting immediately"
                                );
                                roamed_addr = Some(new_addr);
                                backoff.reset();
                                immediate_retry = true;
                            }
                            RoamLookup::NotFound => {}
                        }
                    }
                }
            }
        }

        if immediate_retry {
            continue;
        }

        // Reset backoff immediately if the OS just woke or LAN IPs changed during connect/discover
        let os_resumed = injector.poll_system_resumed();
        let fresh_ips = local_ipv4_addresses();
        let ips_changed = fresh_ips != last_local_ips;
        if os_resumed || ips_changed {
            last_local_ips = fresh_ips.clone();
            if let Some(ref rt) = runtime {
                rt.update(|s| s.local_ips = fresh_ips);
            }
            backoff.reset();
        }

        let delay = backoff.next_delay();
        sleep_detector.reset();
        let mut rx_ref = cmd_rx.as_mut();
        match wait_reconnect_delay(
            delay,
            &mut config,
            injector,
            &mut sleep_detector,
            &mut last_local_ips,
            runtime.as_ref(),
            &mut rx_ref,
        )
        .await
        {
            WaitOutcome::Shutdown => {
                info!("Client received graceful shutdown command during reconnect backoff");
                break;
            }
            WaitOutcome::FastRetry => {
                backoff.reset();
            }
            WaitOutcome::Retry => {}
        }
    }

    let _ = injector.release_all();
    let _ = injector.show_cursor();
    if let Some(ref rt) = runtime {
        rt.update(|s| {
            s.mode = "stopped".into();
            s.status = "Idle".into();
            s.active_target = "Local".into();
            s.rtt_ms = None;
            s.rtt_history.clear();
        });
    }
    Ok(())
}

/// Runs the native Glide client with automatic LAN discovery, reconnect, and optional GUI hooks.
pub async fn run_native_client_with_runtime(
    config: ClientConfig,
    runtime: Option<RuntimeState>,
    cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    glide_backend::init_platform();
    let mut injector = glide_backend::create_injector_with_kind(config.input_backend)?;

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

    run_client_with_backends(config, injector.as_mut(), &mut clipboard, runtime, cmd_rx).await
}

/// Convenience CLI entrypoint using native OS backends.
pub async fn run_native_client(config: ClientConfig) -> Result<()> {
    run_native_client_with_runtime(config, None, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use glide_backend::geometry::{DesktopGeometry, MonitorRect};
    use glide_backend::mock::MockInjector;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn test_geom() -> DesktopGeometry {
        DesktopGeometry::new(vec![MonitorRect {
            name: "Test".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }])
    }

    #[tokio::test]
    async fn resolve_explicit_addr_handles_ipv4_ipv6_without_dns() {
        assert_eq!(
            resolve_explicit_addr("192.168.1.42").await.unwrap(),
            format!("192.168.1.42:{DEFAULT_QUIC_PORT}")
                .parse::<SocketAddr>()
                .unwrap()
        );
        assert_eq!(
            resolve_explicit_addr("192.168.1.42:24999").await.unwrap(),
            "192.168.1.42:24999".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            resolve_explicit_addr("::1").await.unwrap(),
            format!("[::1]:{DEFAULT_QUIC_PORT}")
                .parse::<SocketAddr>()
                .unwrap()
        );
        assert_eq!(
            resolve_explicit_addr("[::1]:24999").await.unwrap(),
            "[::1]:24999".parse::<SocketAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn wait_reconnect_delay_aborts_immediately_on_system_resume() {
        let (mut injector, _) = MockInjector::new(test_geom());
        let resumed = Arc::clone(&injector.resumed_flag);
        let mut config = ClientConfig::default();
        let mut sleep_det = SleepDetector::default();
        let mut ips = local_ipv4_addresses();
        let mut cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>> = None;

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            resumed.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let outcome = wait_reconnect_delay(
            Duration::from_secs(3),
            &mut config,
            &mut injector,
            &mut sleep_det,
            &mut ips,
            None,
            &mut cmd_rx,
        )
        .await;

        assert!(matches!(outcome, WaitOutcome::FastRetry));
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "wake must interrupt 3s backoff immediately, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn wait_reconnect_delay_aborts_immediately_on_shutdown() {
        let (mut injector, _) = MockInjector::new(test_geom());
        let mut config = ClientConfig::default();
        let mut sleep_det = SleepDetector::default();
        let mut ips = local_ipv4_addresses();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut cmd_rx = Some(&mut rx);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let _ = tx.send(RuntimeCommand::Shutdown);
        });

        let started = Instant::now();
        let outcome = wait_reconnect_delay(
            Duration::from_secs(3),
            &mut config,
            &mut injector,
            &mut sleep_det,
            &mut ips,
            None,
            &mut cmd_rx,
        )
        .await;

        assert!(matches!(outcome, WaitOutcome::Shutdown));
        assert!(started.elapsed() < Duration::from_millis(400));
    }
}
