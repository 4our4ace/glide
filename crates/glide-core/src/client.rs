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
use crate::runtime::{save_received_file, RuntimeCommand, RuntimeState};
use anyhow::{anyhow, Context, Result};
use glide_backend::os_integration::file_receive_dir;
use glide_backend::{ClipboardHandle, InputInjector};
use glide_net::discovery::discover_servers;
use glide_net::tls::TrustMode;
use glide_net::{connect_client, recv_bulk_on, send_clipboard_on, send_file_on, sha256_bytes};
use glide_proto::{
    BulkTransfer, ClipboardPayload, ControlMessage, DatagramMessage, FilePayload,
    LAZY_CLIPBOARD_THRESHOLD, PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::lookup_host;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Resolves the target server socket address either from explicit `config.server`
/// or by auto-discovering a running Glide server on the local network.
async fn resolve_server_addr(config: &ClientConfig) -> Result<(SocketAddr, Option<String>)> {
    if let Some(ref raw) = config.server {
        let trimmed = raw.trim();
        let with_port = if trimmed.contains(':') {
            trimmed.to_string()
        } else {
            format!("{trimmed}:{DEFAULT_QUIC_PORT}")
        };
        // Fast offline-safe path: parse literal IPv4/IPv6 directly without ever touching OS DNS!
        if let Ok(addr) = with_port.parse::<SocketAddr>() {
            return Ok((addr, None));
        }
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
    config: &mut ClientConfig,
    server_addr: SocketAddr,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    runtime: Option<&RuntimeState>,
    mut cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    let trust = build_trust_mode(config);
    let mut session = connect_client(server_addr, trust).await?;

    let client_geom = injector.desktop_geometry().unwrap_or_default();
    let client_scale = client_geom.primary_scale();

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
            return Err(anyhow!("server rejected connection: {reason}"));
        }
        other => {
            return Err(anyhow!("unexpected handshake response: {other:?}"));
        }
    };

    if let Some(rt) = runtime {
        rt.update(|s| {
            s.mode = "client".into();
            s.status = format!("Connected to {server_name}");
            s.active_target = server_name.clone();
            s.local_addr = Some(server_addr.to_string());
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
    ping_interval.tick().await;
    let mut reveal_interval = tokio::time::interval(Duration::from_millis(150));
    reveal_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reveal_interval.tick().await;

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
                if let Some(rt) = runtime {
                    let rtt_ms = conn.rtt().as_secs_f32() * 1000.0;
                    rt.record_rtt(rtt_ms);
                }
            }

            _ = reveal_interval.tick(), if !focused => {
                injector.reveal_if_moved_locally();
            }

            cmd = async {
                match cmd_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(cmd) = cmd {
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
            }

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

    let _ = injector.release_all();
    let _ = injector.show_cursor();
    session_result
}

/// Runs the native Glide client with automatic LAN discovery, reconnect, and optional GUI hooks.
pub async fn run_native_client_with_runtime(
    mut config: ClientConfig,
    runtime: Option<RuntimeState>,
    mut cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    glide_backend::init_platform();
    let mut injector = glide_backend::create_injector_with_kind(config.input_backend)?;
    let geom = injector.desktop_geometry().unwrap_or_default();

    if let Some(ref rt) = runtime {
        rt.update(|s| {
            s.mode = "client".into();
            s.status = "Searching LAN for Glide server...".into();
            s.geometry = geom.clone();
            s.last_error = None;
        });
    }

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

        match resolve_server_addr(&config).await {
            Ok((addr, _)) => {
                match run_client_session(
                    &mut config,
                    addr,
                    injector.as_mut(),
                    &mut clipboard,
                    runtime.as_ref(),
                    cmd_rx.as_mut(),
                )
                .await
                {
                    Ok(()) => {
                        info!("Session ended cleanly");
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(error = %e, "Disconnected from server; reconnecting shortly...");
                        if let Some(ref rt) = runtime {
                            rt.update(|s| {
                                s.status = "Reconnecting...".into();
                                s.last_error = Some(e.to_string());
                            });
                        }
                        let _ = injector.release_all();
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Could not resolve server; retrying...");
                if let Some(ref rt) = runtime {
                    rt.update(|s| s.last_error = Some(e.to_string()));
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(3));
    }
}

/// Convenience CLI entrypoint using native OS backends.
pub async fn run_native_client(config: ClientConfig) -> Result<()> {
    run_native_client_with_runtime(config, None, None).await
}
