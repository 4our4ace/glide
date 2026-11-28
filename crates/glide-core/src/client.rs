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

use crate::config::{
    compile_key_remaps, current_unix_secs, default_known_servers_path, record_known_server,
    ClientConfig, KnownServer, DEFAULT_QUIC_PORT,
};
use crate::dnd::{
    collect_local_drag_files, next_drag_id, spawn_send_drag_files, EarlyDragBuffer,
    IncomingDragState,
};
use crate::resilience::{HealthProbe, ReconnectBackoff, SleepDetector};
use crate::runtime::{save_received_file, RuntimeCommand, RuntimeState};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use glide_backend::dnd::{DndBackend, NoopDndBackend};
use glide_backend::lifecycle::{
    NetworkChangeMonitor, ShutdownInhibitorGuard, SystemLifecycleEvent, SystemLifecycleMonitor,
};
use glide_backend::os_integration::resolve_receive_dir;
use glide_backend::{ClipboardBackend, ClipboardHandle, InjectorEvent, InputInjector};
use glide_hid::MouseButton;
use glide_net::discovery::{
    discover_servers, local_ipv4_addresses_async, DiscoveredServer, PassiveDiscoverySession,
};
use glide_net::tls::{normalize_fingerprint, TrustMode};
use glide_net::{
    close_and_flush_conn, connect_client, recv_bulk_stream_with_atomic_limit, send_clipboard_on,
    send_file_on_with_limit, sha256_bytes_async, Connection,
};
use glide_proto::{
    BulkTransfer, ClipboardPayload, ControlMessage, DatagramMessage, FilePayload,
    LAZY_CLIPBOARD_THRESHOLD, PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Waker};
use std::time::{Duration, Instant};
use tokio::net::lookup_host;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct NoopClipboardBackend;
impl ClipboardBackend for NoopClipboardBackend {
    fn set(&self, _payload: ClipboardPayload) -> Result<()> {
        Ok(())
    }
}

struct SharedClipboardBackend(Arc<dyn ClipboardBackend>);
impl ClipboardBackend for SharedClipboardBackend {
    fn set(&self, payload: ClipboardPayload) -> Result<()> {
        self.0.set(payload)
    }
}

/// Lifts `ClipboardHandle::backend` into an `Arc<dyn ClipboardBackend>` once so background
/// clipboard-apply tasks can run `spawn_blocking` across reconnects while keeping `&mut Option<ClipboardHandle>`
/// valid even if a session is cancelled externally.
fn share_clipboard_backend(
    clipboard: &mut Option<ClipboardHandle>,
) -> Option<Arc<dyn ClipboardBackend>> {
    let handle = clipboard.as_mut()?;
    let orig = std::mem::replace(&mut handle.backend, Box::new(NoopClipboardBackend));
    let shared: Arc<dyn ClipboardBackend> = Arc::from(orig);
    handle.backend = Box::new(SharedClipboardBackend(Arc::clone(&shared)));
    Some(shared)
}

/// Non-blockingly polls one already-buffered QUIC datagram using a noop waker so post-hiccup
/// motion bursts can be drained and coalesced into a single `injector.move_relative()` call.
fn try_read_datagram_now(conn: &Connection) -> Option<Result<Bytes>> {
    let mut fut = std::pin::pin!(conn.read_datagram());
    let mut cx = TaskContext::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(b)) => Some(Ok(b)),
        Poll::Ready(Err(e)) => Some(Err(anyhow!("datagram stream closed: {e}"))),
        Poll::Pending => None,
    }
}

fn accept_motion_seq(last_seq: &mut Option<u32>, seq: u32) -> bool {
    if let Some(prev) = *last_seq {
        let diff = seq.wrapping_sub(prev) as i32;
        if diff < -32 {
            return false;
        }
        if diff > 0 {
            *last_seq = Some(seq);
        }
    } else {
        *last_seq = Some(seq);
    }
    true
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const DNS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
const ROAM_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(900);
const MAX_DATAGRAM_BURST_DRAIN: usize = 64;
const DRAG_MOVE_MIN_INTERVAL: Duration = Duration::from_millis(16);
const SLOW_INJECT_WARN_THRESHOLD: Duration = Duration::from_millis(15);
const LAG_WARN_MIN_INTERVAL: Duration = Duration::from_secs(2);

enum ResolveOutcome {
    Addr(SocketAddr),
    OsPaused { shutting_down: bool },
    Shutdown,
}

enum RoamLookup {
    Found(SocketAddr),
    NotFound,
    OsPaused { shutting_down: bool },
    Shutdown,
}

enum WaitOutcome {
    Retry,
    FastRetry,
    OsPaused { shutting_down: bool },
    Shutdown,
}

enum PauseOutcome {
    Resumed,
    Shutdown,
}

enum SessionOutcome {
    Shutdown,
    OsPaused { shutting_down: bool },
    Disconnected { established: bool, err: anyhow::Error },
}

enum SessionExit {
    Shutdown,
    OsPaused {
        shutting_down: bool,
        guard: ShutdownInhibitorGuard,
    },
    Error(anyhow::Error),
}

async fn recv_cmd_opt(
    cmd_rx: &mut Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Option<RuntimeCommand> {
    match cmd_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_lifecycle_event(
    lifecycle: &mut Option<&mut SystemLifecycleMonitor>,
) -> SystemLifecycleEvent {
    match lifecycle.as_mut() {
        Some(mon) => match mon.recv().await {
            Some(ev) => ev,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

async fn next_injector_event(
    injector_events: &mut Option<mpsc::UnboundedReceiver<InjectorEvent>>,
) -> InjectorEvent {
    match injector_events.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(ev) => ev,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

async fn next_network_change(net_monitor: &mut Option<&mut NetworkChangeMonitor>) {
    match net_monitor.as_mut() {
        Some(mon) => {
            if mon.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

async fn next_passive_beacon(
    session: &mut Option<PassiveDiscoverySession>,
) -> Option<DiscoveredServer> {
    match session.as_mut() {
        Some(s) => s.recv_beacon().await,
        None => std::future::pending().await,
    }
}

fn sync_client_probe_timer(
    health_probe: &HealthProbe,
    probe_sleep: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    probe_deadline_armed: &mut Option<Instant>,
) {
    let next = health_probe.deadline();
    if *probe_deadline_armed != next {
        *probe_deadline_armed = next;
        *probe_sleep = next.map(|deadline| {
            Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
                deadline,
            )))
        });
    }
}

fn arm_client_probe_if_idle(
    health_probe: &mut HealthProbe,
    ctrl_out_tx: &mpsc::UnboundedSender<ControlMessage>,
    probe_sleep: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    probe_deadline_armed: &mut Option<Instant>,
) -> bool {
    if health_probe.is_pending() {
        return true;
    }
    let ts_us = health_probe.start_probe(Instant::now());
    if ctrl_out_tx.send(ControlMessage::Ping { ts_us }).is_err() {
        return false;
    }
    sync_client_probe_timer(health_probe, probe_sleep, probe_deadline_armed);
    true
}

/// Applies non-fatal runtime commands received while disconnected/connecting and returns
/// `true` if the client should shut down immediately.
fn apply_pre_session_cmd(
    cmd: Option<RuntimeCommand>,
    config: &mut ClientConfig,
    injector: Option<&mut dyn InputInjector>,
) -> bool {
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
            unfocus_on_leave,
            clipboard,
            drag_drop_files,
            key_remap,
        }) => {
            config.sensitivity = sensitivity;
            config.scroll_speed = scroll_speed;
            config.invert_scroll = invert_scroll;
            config.auto_dpi_scale = auto_dpi_scale;
            config.edge_threshold_px = edge_threshold_px;
            config.dead_corner_px = dead_corner_px;
            config.sync_lock_keys = sync_lock_keys;
            config.unfocus_on_leave = unfocus_on_leave;
            config.clipboard = clipboard;
            config.drag_drop_files = drag_drop_files;
            config.key_remap = key_remap;
            if let Some(inj) = injector {
                inj.set_unfocus_on_leave(unfocus_on_leave);
            }
            false
        }
        Some(RuntimeCommand::UpdateMaxFileBytes(limit)) => {
            config.max_file_bytes = Some(limit.min(u32::MAX as usize));
            false
        }
        Some(RuntimeCommand::UpdateReceiveDir(dir)) => {
            config.receive_dir = dir;
            false
        }
        Some(RuntimeCommand::MovePeerEdge { edge, .. }) => {
            config.edge = edge;
            false
        }
        Some(_) => false,
    }
}

/// Parks the client reconnect supervisor while the OS is sleeping or shutting down with
/// zero timers or polling, resuming immediately on [`SystemLifecycleEvent::Resumed`] or
/// exiting on [`RuntimeCommand::Shutdown`].
#[allow(clippy::too_many_arguments)]
async fn wait_while_os_paused(
    mut shutting_down: bool,
    config: &mut ClientConfig,
    injector: &mut dyn InputInjector,
    injector_events: &mut Option<mpsc::UnboundedReceiver<InjectorEvent>>,
    runtime: Option<&RuntimeState>,
    lifecycle: &mut Option<&mut SystemLifecycleMonitor>,
    net_monitor: &mut Option<&mut NetworkChangeMonitor>,
    cmd_rx: &mut Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> PauseOutcome {
    injector.discard_saved_focus();
    let _ = injector.release_all();
    let _ = injector.show_cursor();
    let update_paused_status = |rt: Option<&RuntimeState>, is_shutdown: bool| {
        if let Some(rt) = rt {
            rt.update(|s| {
                s.status = if is_shutdown {
                    "Paused (OS shutting down)".into()
                } else {
                    "Paused (OS sleeping)".into()
                };
                s.active_target = "Local".into();
                s.remote_fingerprint = None;
                s.rtt_ms = None;
            });
        }
    };
    update_paused_status(runtime, shutting_down);

    loop {
        let Some(mon) = lifecycle.as_mut() else {
            return PauseOutcome::Resumed;
        };
        tokio::select! {
            cmd = recv_cmd_opt(cmd_rx) => {
                if apply_pre_session_cmd(cmd, config, Some(&mut *injector)) {
                    return PauseOutcome::Shutdown;
                }
            }
            inj_ev = next_injector_event(injector_events) => {
                if let InjectorEvent::GeometryChanged(g) = inj_ev {
                    injector.apply_geometry(g.clone());
                    if let Some(rt) = runtime {
                        rt.update(|s| s.geometry = g);
                    }
                }
            }
            _ = next_network_change(net_monitor) => {}
            ev = mon.recv() => {
                match ev {
                    Some(SystemLifecycleEvent::Resumed) => {
                        info!("OS resumed; resuming Glide client reconnects");
                        return PauseOutcome::Resumed;
                    }
                    Some(SystemLifecycleEvent::PreparingToShutdown(guard)) => {
                        if !shutting_down {
                            shutting_down = true;
                            update_paused_status(runtime, true);
                        }
                        drop(guard);
                    }
                    Some(SystemLifecycleEvent::PreparingToSleep(guard)) => {
                        drop(guard);
                    }
                    None => {
                        warn!("OS lifecycle monitor ended while paused; resuming Glide client");
                        return PauseOutcome::Resumed;
                    }
                }
            }
        }
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
    normalize_fingerprint(&raw)
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
/// roams between Wi-Fi and Ethernet and acquires a new LAN IP). Interruptible by `Shutdown`
/// and OS sleep/shutdown lifecycle events.
async fn discover_roamed_server_by_fingerprint(
    config: &mut ClientConfig,
    expected_norm_fp: &str,
    lifecycle: &mut Option<&mut SystemLifecycleMonitor>,
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
                if apply_pre_session_cmd(cmd, config, None) {
                    return RoamLookup::Shutdown;
                }
            }
            ev = next_lifecycle_event(lifecycle) => {
                match ev {
                    SystemLifecycleEvent::PreparingToShutdown(guard) => {
                        drop(guard);
                        return RoamLookup::OsPaused { shutting_down: true };
                    }
                    SystemLifecycleEvent::PreparingToSleep(guard) => {
                        drop(guard);
                        return RoamLookup::OsPaused { shutting_down: false };
                    }
                    SystemLifecycleEvent::Resumed => {}
                }
            }
            res = &mut discover_fut => {
                let found = res.unwrap_or_default();
                if let Some(srv) = found
                    .into_iter()
                    .find(|s| normalize_fingerprint(&s.fingerprint).as_deref() == Some(expected_norm_fp))
                {
                    return RoamLookup::Found(srv.addr);
                }
                return RoamLookup::NotFound;
            }
        }
    }
}

/// Resolves the target server socket address either from explicit `config.server`
/// or by passively listening for a running Glide server on the local network (0.00 Hz idle:
/// sends one probe on start and re-probes strictly on OS resume, network-change notifications,
/// or explicit refresh commands while awaiting `MAGIC_ANNOUNCE` beacons).
#[allow(clippy::too_many_arguments)]
async fn resolve_server_addr(
    config: &mut ClientConfig,
    injector: &mut dyn InputInjector,
    injector_events: &mut Option<mpsc::UnboundedReceiver<InjectorEvent>>,
    last_local_ips: &mut Vec<String>,
    runtime: Option<&RuntimeState>,
    lifecycle: &mut Option<&mut SystemLifecycleMonitor>,
    net_monitor: &mut Option<&mut NetworkChangeMonitor>,
    cmd_rx: &mut Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<ResolveOutcome> {
    if let Some(raw) = config.server.clone() {
        let resolve_fut = resolve_explicit_addr(&raw);
        tokio::pin!(resolve_fut);
        loop {
            tokio::select! {
                biased;
                cmd = recv_cmd_opt(cmd_rx) => {
                    if apply_pre_session_cmd(cmd, config, Some(&mut *injector)) {
                        return Ok(ResolveOutcome::Shutdown);
                    }
                }
                ev = next_lifecycle_event(lifecycle) => {
                    match ev {
                        SystemLifecycleEvent::PreparingToShutdown(guard) => {
                            drop(guard);
                            return Ok(ResolveOutcome::OsPaused { shutting_down: true });
                        }
                        SystemLifecycleEvent::PreparingToSleep(guard) => {
                            drop(guard);
                            return Ok(ResolveOutcome::OsPaused { shutting_down: false });
                        }
                        SystemLifecycleEvent::Resumed => {}
                    }
                }
                inj_ev = next_injector_event(injector_events) => {
                    if let InjectorEvent::GeometryChanged(g) = inj_ev {
                        injector.apply_geometry(g.clone());
                        if let Some(rt) = runtime {
                            rt.update(|s| s.geometry = g);
                        }
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
        "Searching LAN for a running Glide server (event-driven passive discovery)..."
    );
    let server_name_filter = config.server_name.clone();
    let mut passive = PassiveDiscoverySession::start(config.discovery_port).await?;

    loop {
        tokio::select! {
            cmd = recv_cmd_opt(cmd_rx) => {
                let is_refresh = matches!(cmd, Some(RuntimeCommand::RefreshStats));
                if apply_pre_session_cmd(cmd, config, Some(&mut *injector)) {
                    return Ok(ResolveOutcome::Shutdown);
                }
                if is_refresh {
                    passive.probe_once().await;
                }
            }
            ev = next_lifecycle_event(lifecycle) => {
                match ev {
                    SystemLifecycleEvent::PreparingToShutdown(guard) => {
                        drop(guard);
                        return Ok(ResolveOutcome::OsPaused { shutting_down: true });
                    }
                    SystemLifecycleEvent::PreparingToSleep(guard) => {
                        drop(guard);
                        return Ok(ResolveOutcome::OsPaused { shutting_down: false });
                    }
                    SystemLifecycleEvent::Resumed => {
                        let fresh_ips = passive.refresh_and_probe().await;
                        if fresh_ips != *last_local_ips {
                            *last_local_ips = fresh_ips.clone();
                            if let Some(rt) = runtime {
                                rt.update(|s| s.local_ips = fresh_ips);
                            }
                        }
                    }
                }
            }
            inj_ev = next_injector_event(injector_events) => {
                match inj_ev {
                    InjectorEvent::GeometryChanged(g) => {
                        injector.apply_geometry(g.clone());
                        if let Some(rt) = runtime {
                            rt.update(|s| s.geometry = g);
                        }
                    }
                    InjectorEvent::SystemResumed => {
                        let fresh_ips = passive.refresh_and_probe().await;
                        if fresh_ips != *last_local_ips {
                            *last_local_ips = fresh_ips.clone();
                            if let Some(rt) = runtime {
                                rt.update(|s| s.local_ips = fresh_ips);
                            }
                        }
                    }
                    InjectorEvent::SystemSuspended | InjectorEvent::LocalCursorActivity => {}
                }
            }
            _ = next_network_change(net_monitor) => {
                let fresh_ips = passive.refresh_and_probe().await;
                if fresh_ips != *last_local_ips {
                    *last_local_ips = fresh_ips.clone();
                    if let Some(rt) = runtime {
                        rt.update(|s| s.local_ips = fresh_ips);
                    }
                }
            }
            maybe_srv = passive.recv_beacon() => {
                let Some(srv) = maybe_srv else {
                    return Err(anyhow!("passive discovery socket closed"));
                };
                if server_name_filter
                    .as_deref()
                    .is_some_and(|f| !srv.name.eq_ignore_ascii_case(f))
                {
                    continue;
                }
                info!(
                    name = %srv.name,
                    os = %srv.os,
                    addr = %srv.addr,
                    fingerprint = %srv.fingerprint,
                    "Auto-discovered Glide server on LAN"
                );
                return Ok(ResolveOutcome::Addr(srv.addr));
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

/// Runs a single connected client session until disconnect or error (using a no-op DnD backend).
pub async fn run_client_session(
    config: &mut ClientConfig,
    server_addr: SocketAddr,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    runtime: Option<&RuntimeState>,
    cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    run_client_session_with_dnd(
        config,
        server_addr,
        injector,
        clipboard,
        Arc::new(NoopDndBackend),
        None,
        runtime,
        cmd_rx,
    )
    .await
}

/// Runs a single connected client session with an explicit cross-edge drag-and-drop backend.
#[allow(clippy::too_many_arguments)]
pub async fn run_client_session_with_dnd(
    config: &mut ClientConfig,
    server_addr: SocketAddr,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    dnd: Arc<dyn DndBackend>,
    receive_dir: Option<PathBuf>,
    runtime: Option<&RuntimeState>,
    cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    let shared_clip_backend = share_clipboard_backend(clipboard);
    let mut injector_events = injector.take_event_rx();
    match run_client_session_tracked(
        config,
        server_addr,
        injector,
        &mut injector_events,
        clipboard,
        shared_clip_backend,
        dnd,
        receive_dir,
        runtime,
        cmd_rx,
        None,
        None,
    )
    .await
    {
        SessionOutcome::Shutdown | SessionOutcome::OsPaused { .. } => Ok(()),
        SessionOutcome::Disconnected { err, .. } => Err(err),
    }
}

/// Internal session runner that also reports whether the handshake completed successfully so
/// the reconnect supervisor can reset backoff immediately after any previously healthy session,
/// and surfaces OS sleep/shutdown pauses after flushing QUIC `CONNECTION_CLOSE`.
#[allow(clippy::too_many_arguments)]
async fn run_client_session_tracked(
    config: &mut ClientConfig,
    server_addr: SocketAddr,
    injector: &mut dyn InputInjector,
    injector_events: &mut Option<mpsc::UnboundedReceiver<InjectorEvent>>,
    clipboard: &mut Option<ClipboardHandle>,
    shared_clip_backend: Option<Arc<dyn ClipboardBackend>>,
    dnd: Arc<dyn DndBackend>,
    receive_dir: Option<PathBuf>,
    runtime: Option<&RuntimeState>,
    mut cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
    mut lifecycle: Option<&mut SystemLifecycleMonitor>,
    mut net_monitor: Option<&mut NetworkChangeMonitor>,
) -> SessionOutcome {
    injector.set_unfocus_on_leave(config.unfocus_on_leave);
    let trust = build_trust_mode(config);
    let mut session = {
        let connect_fut = tokio::time::timeout(CONNECT_TIMEOUT, connect_client(server_addr, trust));
        tokio::pin!(connect_fut);
        loop {
            tokio::select! {
                cmd = recv_cmd_opt(&mut cmd_rx) => {
                    if apply_pre_session_cmd(cmd, config, Some(&mut *injector)) {
                        return SessionOutcome::Shutdown;
                    }
                }
                ev = next_lifecycle_event(&mut lifecycle) => {
                    match ev {
                        SystemLifecycleEvent::PreparingToShutdown(guard) => {
                            drop(guard);
                            return SessionOutcome::OsPaused { shutting_down: true };
                        }
                        SystemLifecycleEvent::PreparingToSleep(guard) => {
                            drop(guard);
                            return SessionOutcome::OsPaused { shutting_down: false };
                        }
                        SystemLifecycleEvent::Resumed => {}
                    }
                }
                res = &mut connect_fut => {
                    match res {
                        Ok(Ok(s)) => break s,
                        Ok(Err(e)) => {
                            return SessionOutcome::Disconnected {
                                established: false,
                                err: e,
                            };
                        }
                        Err(_) => {
                            return SessionOutcome::Disconnected {
                                established: false,
                                err: anyhow!("timed out connecting to Glide server {server_addr}"),
                            };
                        }
                    }
                }
            }
        }
    };

    let mut client_geom = injector.desktop_geometry().unwrap_or_default();
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
        close_and_flush_conn(&session.conn, session.endpoint.as_ref(), 0, b"hello failed").await;
        return SessionOutcome::Disconnected {
            established: false,
            err: e,
        };
    }

    let welcome = {
        let welcome_fut = tokio::time::timeout(Duration::from_secs(5), session.ctrl_recv.recv());
        tokio::pin!(welcome_fut);
        loop {
            tokio::select! {
                cmd = recv_cmd_opt(&mut cmd_rx) => {
                    if apply_pre_session_cmd(cmd, config, Some(&mut *injector)) {
                        close_and_flush_conn(
                            &session.conn,
                            session.endpoint.as_ref(),
                            0,
                            b"client shutdown",
                        )
                        .await;
                        return SessionOutcome::Shutdown;
                    }
                }
                ev = next_lifecycle_event(&mut lifecycle) => {
                    match ev {
                        SystemLifecycleEvent::PreparingToShutdown(guard) => {
                            close_and_flush_conn(
                                &session.conn,
                                session.endpoint.as_ref(),
                                0,
                                b"os shutdown",
                            )
                            .await;
                            drop(guard);
                            return SessionOutcome::OsPaused { shutting_down: true };
                        }
                        SystemLifecycleEvent::PreparingToSleep(guard) => {
                            close_and_flush_conn(
                                &session.conn,
                                session.endpoint.as_ref(),
                                0,
                                b"os sleep",
                            )
                            .await;
                            drop(guard);
                            return SessionOutcome::OsPaused { shutting_down: false };
                        }
                        SystemLifecycleEvent::Resumed => {}
                    }
                }
                res = &mut welcome_fut => {
                    match res {
                        Ok(Ok(w)) => break w,
                        Ok(Err(e)) => {
                            session.conn.close(0u32.into(), b"welcome failed");
                            return SessionOutcome::Disconnected {
                                established: false,
                                err: e,
                            };
                        }
                        Err(_) => {
                            session.conn.close(0u32.into(), b"welcome timeout");
                            return SessionOutcome::Disconnected {
                                established: false,
                                err: anyhow!("timed out waiting for server Welcome"),
                            };
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
            return SessionOutcome::Disconnected {
                established: false,
                err: anyhow!("server rejected connection: {reason}"),
            };
        }
        other => {
            session.conn.close(0u32.into(), b"bad handshake");
            return SessionOutcome::Disconnected {
                established: false,
                err: anyhow!("unexpected handshake response: {other:?}"),
            };
        }
    };

    let mut last_local_ips = local_ipv4_addresses_async().await;
    let peer_fp = session
        .peer_fingerprint()
        .or_else(|| config.trusted_fingerprint.clone());
    let peer_fp_norm = peer_fp.as_deref().and_then(normalize_fingerprint);

    if let Some(store_path) = config.known_servers_path.clone() {
        let entry = KnownServer {
            name: server_name.clone(),
            address: server_addr.to_string(),
            fingerprint: peer_fp.clone(),
            edge: config.edge,
            last_connected_unix: current_unix_secs(),
        };
        match tokio::task::spawn_blocking(move || record_known_server(&store_path, entry)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(error = %e, "Failed saving known server profile"),
            Err(e) => warn!(error = %e, "Known server profile save task failed"),
        }
    }

    let initial_rtt_ms = session.conn.rtt().as_secs_f32() * 1000.0;
    if let Some(rt) = runtime {
        rt.update(|s| {
            s.mode = "client".into();
            s.status = format!("Connected to {server_name}");
            s.active_target = server_name.clone();
            s.local_addr = Some(server_addr.to_string());
            s.local_ips = last_local_ips.clone();
            s.remote_fingerprint = peer_fp;
            s.geometry = client_geom.clone();
            s.rtt_ms = Some(initial_rtt_ms);
            s.rtt_history.push(initial_rtt_ms);
            if s.rtt_history.len() > 24 {
                s.rtt_history.remove(0);
            }
            s.last_error = None;
        });
    }

    let mut return_edge = config.edge.opposite();
    dnd.set_enabled(config.drag_drop_files);
    dnd.set_active_edges(&[return_edge], config.dead_corner_px);
    dnd.set_drag_armed(false);

    let mut key_remaps = compile_key_remaps(&config.key_remap);
    let mut focused = false;
    let mut last_seq: Option<u32> = None;
    let mut pending_offer_hash: Option<[u8; 32]> = None;
    let mut local_clip_cache: HashMap<[u8; 32], ClipboardPayload> = HashMap::new();
    let mut incoming_drag: Option<IncomingDragState> = None;
    let mut early_drag_files = EarlyDragBuffer::default();
    let mut client_lmb_down = false;
    let mut last_drag_move_at: Option<Instant> = None;
    let leave_gen = Arc::new(AtomicU64::new(0));

    // Strictly listen-only LAN discovery listener (emits ZERO probes, 0.00 Hz idle) so if
    // the server hard-powers-off and later reboots, its startup announcement immediately triggers
    // an on-demand Ping probe that detects the dead QUIC session and reconnects automatically.
    let mut session_beacon_rx = if config.discovery_port != 0 {
        PassiveDiscoverySession::listen_only(config.discovery_port)
            .await
            .ok()
    } else {
        None
    };

    // One-shot health probe timer: armed ONLY while an explicit Ping probe is in flight,
    // dropped to `None` when idle so 0 timers remain registered in Tokio's timer wheel.
    let mut probe_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut probe_deadline_armed: Option<Instant> = None;

    let mut health_probe = HealthProbe::default();
    let (ip_tx, mut ip_rx) = mpsc::unbounded_channel::<(u64, Vec<String>, bool)>();
    let mut ip_check_gen: u64 = 0;

    let mut last_lost_packets: u64 = 0;
    let mut last_congestion_events: u64 = 0;
    let mut last_lag_warn_at: Option<Instant> = None;
    let mut last_slow_inject_warn: Option<Instant> = None;

    // Dedicated serial clipboard-apply task so OS clipboard locks (e.g. OpenClipboard / PNG decode)
    // stay strictly in order on Tokio's blocking pool without ever stalling read_datagram().
    let (clip_apply_tx, _clip_apply_guard) = match shared_clip_backend {
        Some(backend) => {
            let (tx, mut rx) = mpsc::unbounded_channel::<ClipboardPayload>();
            let guard = AbortOnDrop(tokio::spawn(async move {
                while let Some(payload) = rx.recv().await {
                    let b = Arc::clone(&backend);
                    match tokio::task::spawn_blocking(move || b.set(payload)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => warn!(error = %e, "Failed setting local clipboard"),
                        Err(e) => warn!(error = %e, "Client clipboard apply task failed"),
                    }
                }
            }));
            (Some(tx), Some(guard))
        }
        None => (None, None),
    };

    let conn = session.conn;
    let client_endpoint = session.endpoint;

    // Dedicated control stream reader task so `tokio::select!` never cancels a partially-read
    // framed control message during 1000Hz `read_datagram()` traffic.
    let (ctrl_in_tx, mut ctrl_in_rx) = mpsc::unbounded_channel::<Result<ControlMessage>>();
    let mut ctrl_recv = session.ctrl_recv;
    let _ctrl_reader_guard = AbortOnDrop(tokio::spawn(async move {
        loop {
            match ctrl_recv.recv().await {
                Ok(msg) => {
                    if ctrl_in_tx.send(Ok(msg)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = ctrl_in_tx.send(Err(e));
                    break;
                }
            }
        }
    }));

    // Dedicated control stream writer task so the client select! loop never stalls awaiting
    // `ctrl_send.send().await` under QUIC flow-control pressure or bulk transfers.
    let (ctrl_out_tx, mut ctrl_out_rx) = mpsc::unbounded_channel::<ControlMessage>();
    let mut ctrl_send = session.ctrl_send;
    let ctrl_writer_conn = conn.clone();
    let _ctrl_writer_guard = AbortOnDrop(tokio::spawn(async move {
        while let Some(msg) = ctrl_out_rx.recv().await {
            if let Err(e) = ctrl_send.send(&msg).await {
                debug!(error = %e, "Client control stream writer ended");
                if ctrl_writer_conn.close_reason().is_none() {
                    ctrl_writer_conn.close(1u32.into(), b"control send failed");
                }
                break;
            }
        }
    }));

    // Receive unidirectional bulk streams (clipboard images / files / drag files) in a dedicated
    // task so `tokio::select!` branches never cancel an in-flight `read_exact` and silently drop
    // the transfer, while enforcing a live-updated `AtomicUsize` file size limit.
    let max_file_bytes = Arc::new(AtomicUsize::new(config.resolved_max_file_bytes()));
    let (bulk_tx, mut bulk_rx) = mpsc::unbounded_channel::<BulkTransfer>();
    let bulk_conn = conn.clone();
    let bulk_limit = Arc::clone(&max_file_bytes);
    let _bulk_guard = AbortOnDrop(tokio::spawn(async move {
        let sem = Arc::new(tokio::sync::Semaphore::new(4));
        loop {
            let uni = match bulk_conn.accept_uni().await {
                Ok(u) => u,
                Err(e) => {
                    debug!(error = %e, "Remote bulk uni-stream loop ended");
                    break;
                }
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
                        let _ = tx.send(transfer);
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed receiving bulk transfer from server");
                    }
                }
            });
        }
    }));
    let mut bulk_open = true;

    let inject_scroll = |injector: &mut dyn InputInjector,
                         invert_scroll: bool,
                         scroll_speed: f32,
                         dx_120: i16,
                         dy_120: i16| {
        let sign = if invert_scroll { -1.0 } else { 1.0 };
        let mult = scroll_speed.clamp(0.1, 5.0) * sign;
        let sdx = ((dx_120 as f32) * mult)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        let sdy = ((dy_120 as f32) * mult)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        if let Err(e) = injector.scroll(sdx, sdy) {
            warn!(error = %e, "Failed injecting scroll");
        }
    };

    let session_exit: SessionExit = loop {
        let dpi_mult = if config.auto_dpi_scale {
            (client_scale / server_scale).clamp(0.25, 4.0)
        } else {
            1.0
        };
        let eff_sens = (config.sensitivity * dpi_mult).clamp(0.05, 10.0);

        tokio::select! {
            ev = next_lifecycle_event(&mut lifecycle) => {
                match ev {
                    SystemLifecycleEvent::PreparingToShutdown(guard) => {
                        info!("OS preparing to shut down; releasing client input and flushing QUIC CONNECTION_CLOSE");
                        break SessionExit::OsPaused {
                            shutting_down: true,
                            guard,
                        };
                    }
                    SystemLifecycleEvent::PreparingToSleep(guard) => {
                        info!("OS preparing to sleep; releasing client input and flushing QUIC CONNECTION_CLOSE");
                        break SessionExit::OsPaused {
                            shutting_down: false,
                            guard,
                        };
                    }
                    SystemLifecycleEvent::Resumed => {
                        let now = Instant::now();
                        info!("OS lifecycle monitor reported resume while connected; refreshing geometry & probing server");
                        injector.discard_saved_focus();
                        if focused {
                            focused = false;
                            let _ = injector.release_all();
                            let _ = injector.show_cursor();
                        }
                        if let Some(ref mut sess) = session_beacon_rx {
                            let _ = sess.refresh_interfaces_only().await;
                        }
                        let fresh_geom = injector.desktop_geometry().ok();
                        ip_check_gen = ip_check_gen.wrapping_add(1);
                        let gen = ip_check_gen;
                        let tx = ip_tx.clone();
                        tokio::spawn(async move {
                            let ips = local_ipv4_addresses_async().await;
                            let _ = tx.send((gen, ips, true));
                        });
                        if let Some(ref g) = fresh_geom {
                            client_geom = g.clone();
                            client_scale = g.primary_scale();
                        }
                        if let Some(rt) = runtime {
                            rt.update(|s| {
                                if let Some(g) = fresh_geom {
                                    s.geometry = g;
                                }
                                s.active_target = server_name.clone();
                            });
                        }
                        let ts_us = health_probe.start_probe(now);
                        if ctrl_out_tx.send(ControlMessage::Ping { ts_us }).is_err() {
                            break SessionExit::Error(anyhow!(
                                "server unreachable after resume: control writer closed"
                            ));
                        }
                        sync_client_probe_timer(
                            &health_probe,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        );
                    }
                }
            }

            inj_ev = next_injector_event(injector_events) => {
                match inj_ev {
                    InjectorEvent::SystemSuspended => {
                        focused = false;
                        injector.discard_saved_focus();
                        let _ = injector.release_all();
                        let _ = injector.show_cursor();
                        if let Some(rt) = runtime {
                            rt.update(|s| s.active_target = server_name.clone());
                        }
                    }
                    InjectorEvent::SystemResumed => {
                        let now = Instant::now();
                        info!("Client backend reported wake/resume; refreshing geometry & LAN IPs and probing server");
                        injector.discard_saved_focus();
                        if focused {
                            focused = false;
                            let _ = injector.release_all();
                            let _ = injector.show_cursor();
                        }
                        if let Some(ref mut sess) = session_beacon_rx {
                            let _ = sess.refresh_interfaces_only().await;
                        }
                        let fresh_geom = injector.desktop_geometry().ok();
                        ip_check_gen = ip_check_gen.wrapping_add(1);
                        let gen = ip_check_gen;
                        let tx = ip_tx.clone();
                        tokio::spawn(async move {
                            let ips = local_ipv4_addresses_async().await;
                            let _ = tx.send((gen, ips, true));
                        });
                        if let Some(ref g) = fresh_geom {
                            client_geom = g.clone();
                            client_scale = g.primary_scale();
                        }
                        if let Some(rt) = runtime {
                            rt.update(|s| {
                                if let Some(g) = fresh_geom {
                                    s.geometry = g;
                                }
                                s.active_target = server_name.clone();
                            });
                        }
                        let ts_us = health_probe.start_probe(now);
                        if ctrl_out_tx.send(ControlMessage::Ping { ts_us }).is_err() {
                            break SessionExit::Error(anyhow!(
                                "server unreachable after resume: control writer closed"
                            ));
                        }
                        sync_client_probe_timer(
                            &health_probe,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        );
                    }
                    InjectorEvent::GeometryChanged(new_geom) => {
                        injector.apply_geometry(new_geom.clone());
                        client_geom = new_geom.clone();
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
                    InjectorEvent::LocalCursorActivity => {
                        if !focused && injector.is_cursor_hidden() {
                            let _ = injector.show_cursor();
                        }
                    }
                }
            }

            _ = next_network_change(&mut net_monitor) => {
                debug!("OS network change notification on client; refreshing LAN IPs and probing server");
                if let Some(ref mut sess) = session_beacon_rx {
                    let _ = sess.refresh_interfaces_only().await;
                }
                ip_check_gen = ip_check_gen.wrapping_add(1);
                let gen = ip_check_gen;
                let tx = ip_tx.clone();
                tokio::spawn(async move {
                    let ips = local_ipv4_addresses_async().await;
                    let _ = tx.send((gen, ips, true));
                });
                let ts_us = health_probe.start_probe(Instant::now());
                if ctrl_out_tx.send(ControlMessage::Ping { ts_us }).is_err() {
                    break SessionExit::Error(anyhow!(
                        "server unreachable after network change: control writer closed"
                    ));
                }
                sync_client_probe_timer(
                    &health_probe,
                    &mut probe_sleep,
                    &mut probe_deadline_armed,
                );
            }

            Some(srv) = next_passive_beacon(&mut session_beacon_rx) => {
                let matches_server = peer_fp_norm
                    .as_deref()
                    .is_some_and(|fp| normalize_fingerprint(&srv.fingerprint).as_deref() == Some(fp))
                    || srv.name.eq_ignore_ascii_case(&server_name)
                    || srv.addr == server_addr;
                if matches_server {
                    debug!(
                        server = %srv.name,
                        addr = %srv.addr,
                        "Heard LAN announcement from connected server; probing session liveness"
                    );
                    if !arm_client_probe_if_idle(
                        &mut health_probe,
                        &ctrl_out_tx,
                        &mut probe_sleep,
                        &mut probe_deadline_armed,
                    ) {
                        break SessionExit::Error(anyhow!(
                            "server unreachable after LAN announcement: control writer closed"
                        ));
                    }
                }
            }

            _ = async {
                if let Some(s) = probe_sleep.as_mut() {
                    s.as_mut().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                probe_sleep = None;
                let fired_deadline = probe_deadline_armed.take();
                let now = fired_deadline.map_or_else(Instant::now, |d| Instant::now().max(d));
                if conn.close_reason().is_some() {
                    break SessionExit::Error(anyhow!("server QUIC connection closed"));
                }
                if health_probe.is_timed_out(now) {
                    conn.close(1u32.into(), b"health probe timeout");
                    break SessionExit::Error(anyhow!(
                        "server did not respond to health ping"
                    ));
                }
                sync_client_probe_timer(
                    &health_probe,
                    &mut probe_sleep,
                    &mut probe_deadline_armed,
                );
            }

            Some((gen, fresh_ips, from_wake)) = ip_rx.recv() => {
                if gen != ip_check_gen {
                    continue;
                }
                if fresh_ips != last_local_ips {
                    if !from_wake {
                        info!(
                            old = ?last_local_ips,
                            new = ?fresh_ips,
                            "Client LAN interfaces changed; probing server health"
                        );
                    }
                    last_local_ips = fresh_ips.clone();
                    if let Some(rt) = runtime {
                        rt.update(|s| s.local_ips = fresh_ips);
                    }
                    if !from_wake {
                        let ts_us = health_probe.start_probe(Instant::now());
                        if ctrl_out_tx.send(ControlMessage::Ping { ts_us }).is_err() {
                            break SessionExit::Error(anyhow!(
                                "server unreachable after network change: control writer closed"
                            ));
                        }
                        sync_client_probe_timer(
                            &health_probe,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        );
                    }
                }
            }

            cmd = recv_cmd_opt(&mut cmd_rx) => {
                let Some(cmd) = cmd else {
                    info!("Client command channel closed; shutting down session");
                    break SessionExit::Shutdown;
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
                        unfocus_on_leave,
                        clipboard: clip_enabled,
                        drag_drop_files,
                        key_remap,
                    } => {
                        config.sensitivity = sensitivity;
                        config.scroll_speed = scroll_speed;
                        config.invert_scroll = invert_scroll;
                        config.auto_dpi_scale = auto_dpi_scale;
                        config.edge_threshold_px = edge_threshold_px;
                        config.dead_corner_px = dead_corner_px;
                        config.sync_lock_keys = sync_lock_keys;
                        config.unfocus_on_leave = unfocus_on_leave;
                        config.clipboard = clip_enabled;
                        config.drag_drop_files = drag_drop_files;
                        key_remaps = compile_key_remaps(&key_remap);
                        config.key_remap = key_remap;
                        injector.set_unfocus_on_leave(unfocus_on_leave);
                        dnd.set_enabled(drag_drop_files);
                        dnd.set_active_edges(&[return_edge], dead_corner_px);
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
                        } else {
                            let byte_len = data.len();
                            let c = conn.clone();
                            let rt_opt = runtime.cloned();
                            tokio::spawn(async move {
                                let sha256 = sha256_bytes_async(data.clone()).await;
                                let payload = FilePayload {
                                    filename: filename.clone(),
                                    sha256,
                                    data,
                                };
                                match send_file_on_with_limit(&c, &payload, limit).await {
                                    Ok(()) => {
                                        if let Some(rt) = rt_opt {
                                            rt.record_file("sent", filename, byte_len, None);
                                        }
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "Failed beaming file to server");
                                    }
                                }
                            });
                            if !arm_client_probe_if_idle(
                                &mut health_probe,
                                &ctrl_out_tx,
                                &mut probe_sleep,
                                &mut probe_deadline_armed,
                            ) {
                                break SessionExit::Error(anyhow!(
                                    "server unreachable during SendFile: control writer closed"
                                ));
                            }
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
                        if let Some(rt) = runtime {
                            if let Some(payload) = rt.find_clipboard_payload(id) {
                                let c = conn.clone();
                                tokio::spawn(async move {
                                    let _ = send_clipboard_on(&c, &payload).await;
                                });
                                if !arm_client_probe_if_idle(
                                    &mut health_probe,
                                    &ctrl_out_tx,
                                    &mut probe_sleep,
                                    &mut probe_deadline_armed,
                                ) {
                                    break SessionExit::Error(anyhow!(
                                        "server unreachable during ResendClipboard: control writer closed"
                                    ));
                                }
                            }
                        }
                    }
                    RuntimeCommand::MovePeerEdge { edge, .. } => {
                        config.edge = edge;
                        return_edge = edge.opposite();
                        dnd.set_active_edges(&[return_edge], config.dead_corner_px);
                        let _ = ctrl_out_tx.send(ControlMessage::SetEdge {
                            edge_on_server: edge,
                        });
                        if !arm_client_probe_if_idle(
                            &mut health_probe,
                            &ctrl_out_tx,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        ) {
                            break SessionExit::Error(anyhow!(
                                "server unreachable during MovePeerEdge: control writer closed"
                            ));
                        }
                        info!(?edge, "Client moved its own edge live");
                    }
                    RuntimeCommand::RefreshStats => {
                        let ts_us = health_probe.start_probe(Instant::now());
                        if ctrl_out_tx.send(ControlMessage::Ping { ts_us }).is_err() {
                            break SessionExit::Error(anyhow!(
                                "server unreachable during stats refresh: control writer closed"
                            ));
                        }
                        sync_client_probe_timer(
                            &health_probe,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        );
                    }
                    RuntimeCommand::Shutdown => {
                        info!("Client received graceful shutdown command");
                        break SessionExit::Shutdown;
                    }
                    _ => {}
                }
            }

            dgram = conn.read_datagram() => {
                let bytes = match dgram {
                    Ok(b) => b,
                    Err(e) => break SessionExit::Error(anyhow!("datagram stream closed: {e}")),
                };
                health_probe.mark_alive();
                sync_client_probe_timer(
                    &health_probe,
                    &mut probe_sleep,
                    &mut probe_deadline_armed,
                );
                if !focused {
                    continue;
                }
                let Ok(msg) = DatagramMessage::decode(&bytes) else {
                    continue;
                };
                match msg {
                    DatagramMessage::MouseMove { seq, dx, dy } => {
                        let mut sum_dx = 0.0f32;
                        let mut sum_dy = 0.0f32;
                        let mut has_move = false;
                        if accept_motion_seq(&mut last_seq, seq) {
                            sum_dx += dx;
                            sum_dy += dy;
                            has_move = true;
                        }

                        // Drain up to 64 additional already-buffered datagrams non-blockingly so a
                        // post-hiccup burst coalesces into a single `injector.move_relative()` call.
                        let mut trailing_scroll: Option<(i16, i16)> = None;
                        let mut dgram_err: Option<anyhow::Error> = None;
                        for _ in 0..MAX_DATAGRAM_BURST_DRAIN {
                            match try_read_datagram_now(&conn) {
                                Some(Ok(extra_bytes)) => {
                                    let Ok(extra_msg) = DatagramMessage::decode(&extra_bytes) else {
                                        continue;
                                    };
                                    match extra_msg {
                                        DatagramMessage::MouseMove { seq, dx, dy } => {
                                            if accept_motion_seq(&mut last_seq, seq) {
                                                sum_dx += dx;
                                                sum_dy += dy;
                                                has_move = true;
                                            }
                                        }
                                        DatagramMessage::MouseScroll { dx_120, dy_120, .. } => {
                                            trailing_scroll = Some((dx_120, dy_120));
                                            break;
                                        }
                                    }
                                }
                                Some(Err(e)) => {
                                    dgram_err = Some(e);
                                    break;
                                }
                                None => break,
                            }
                        }

                        if has_move {
                            let sdx = sum_dx * eff_sens;
                            let sdy = sum_dy * eff_sens;
                            let inj_started = Instant::now();
                            let move_res = injector.move_relative(
                                sdx,
                                sdy,
                                Some(return_edge),
                                config.edge_threshold_px,
                                config.dead_corner_px,
                            );
                            let inj_elapsed = inj_started.elapsed();
                            if inj_elapsed > SLOW_INJECT_WARN_THRESHOLD
                                && last_slow_inject_warn.is_none_or(|t| {
                                    inj_started.saturating_duration_since(t)
                                        >= LAG_WARN_MIN_INTERVAL
                                })
                            {
                                last_slow_inject_warn = Some(inj_started);
                                warn!(
                                    elapsed_ms = format_args!("{:.1}", inj_elapsed.as_secs_f32() * 1000.0),
                                    dx = sdx,
                                    dy = sdy,
                                    "Client mouse injection stall detected (>15ms)"
                                );
                            }

                            match move_res {
                                Ok(Some((edge, normalized_pos))) => {
                                    debug!(?edge, normalized_pos, "Cursor crossed return edge; handing focus back to server");
                                    // If dragging an uncommitted Server -> Client file back across the
                                    // return edge, cancel cleanly without saving or dropping.
                                    if let Some(inc) = incoming_drag.take() {
                                        if !inc.drop_requested {
                                            debug!(drag_id = inc.drag_id, "Cancelled uncommitted incoming drag on return-edge exit");
                                            early_drag_files.remove_drag(inc.drag_id);
                                            inc.cancel(Arc::clone(&dnd));
                                        } else {
                                            incoming_drag = Some(inc);
                                        }
                                    }
                                    last_drag_move_at = None;

                                    // Check whether the user is dragging local files from this Client
                                    // across the return edge back onto the Server!
                                    let staged_paths = if config.drag_drop_files && client_lmb_down {
                                        dnd.take_staged_files(edge)
                                    } else {
                                        None
                                    };
                                    let limit = max_file_bytes.load(Ordering::Relaxed);
                                    focused = false;
                                    client_lmb_down = false;
                                    let expected_gen =
                                        leave_gen.fetch_add(1, Ordering::AcqRel).wrapping_add(1);

                                    if staged_paths.is_some() {
                                        // Cancel local OS drag off Tokio's async worker before releasing
                                        // virtual LMB so Explorer/Nautilus cancels instead of dropping locally.
                                        let dnd_c = Arc::clone(&dnd);
                                        let _ = tokio::task::spawn_blocking(move || {
                                            dnd_c.cancel_local_drag();
                                        })
                                        .await;
                                    } else {
                                        dnd.set_drag_armed(false);
                                    }

                                    let (cx, cy) = injector.cursor_norm();
                                    let _ = injector.release_all();
                                    let _ = injector.leave();
                                    if let Some(rt) = runtime {
                                        rt.update(|s| s.active_target = server_name.clone());
                                    }
                                    if ctrl_out_tx
                                        .send(ControlMessage::Leave {
                                            edge,
                                            normalized_pos,
                                            cursor_norm_x: cx,
                                            cursor_norm_y: cy,
                                        })
                                        .is_err()
                                    {
                                        break SessionExit::Error(anyhow!("control writer closed during Leave"));
                                    }
                                    if !arm_client_probe_if_idle(
                                        &mut health_probe,
                                        &ctrl_out_tx,
                                        &mut probe_sleep,
                                        &mut probe_deadline_armed,
                                    ) {
                                        break SessionExit::Error(anyhow!("control writer closed during Leave probe"));
                                    }

                                    if let Some(paths) = staged_paths {
                                        let conn_c = conn.clone();
                                        let ctrl_tx_c = ctrl_out_tx.clone();
                                        let rt_c = runtime.cloned();
                                        let leave_gen_c = Arc::clone(&leave_gen);
                                        tokio::spawn(async move {
                                            let collected = tokio::task::spawn_blocking(move || {
                                                collect_local_drag_files(&paths, limit)
                                            })
                                            .await
                                            .ok();
                                            let Some((infos, send_list)) =
                                                collected.filter(|(i, _)| !i.is_empty())
                                            else {
                                                return;
                                            };
                                            if leave_gen_c.load(Ordering::Acquire) != expected_gen
                                                || conn_c.close_reason().is_some()
                                            {
                                                debug!("Aborting stale client DragStart because focus returned or connection closed");
                                                return;
                                            }
                                            let drag_id = next_drag_id();
                                            if ctrl_tx_c
                                                .send(ControlMessage::DragStart {
                                                    drag_id,
                                                    files: infos,
                                                })
                                                .is_err()
                                            {
                                                return;
                                            }
                                            spawn_send_drag_files(
                                                conn_c, drag_id, send_list, limit, rt_c,
                                            );
                                        });
                                    }
                                }
                                Ok(None) => {
                                    if incoming_drag.is_some() {
                                        let now = Instant::now();
                                        if last_drag_move_at.is_none_or(|t| {
                                            now.saturating_duration_since(t) >= DRAG_MOVE_MIN_INTERVAL
                                        }) {
                                            last_drag_move_at = Some(now);
                                            let (nx, ny) = injector.cursor_norm();
                                            let (px, py) = client_geom.denormalize_point(nx, ny);
                                            dnd.on_incoming_drag_move(px, py);
                                        }
                                    }
                                }
                                Err(e) => warn!(error = %e, "Failed injecting relative mouse motion"),
                            }
                        }

                        if focused {
                            if let Some((dx_120, dy_120)) = trailing_scroll {
                                inject_scroll(
                                    injector,
                                    config.invert_scroll,
                                    config.scroll_speed,
                                    dx_120,
                                    dy_120,
                                );
                            }
                        }
                        if let Some(err) = dgram_err {
                            break SessionExit::Error(err);
                        }
                    }
                    DatagramMessage::MouseScroll { dx_120, dy_120, .. } => {
                        inject_scroll(
                            injector,
                            config.invert_scroll,
                            config.scroll_speed,
                            dx_120,
                            dy_120,
                        );
                    }
                }
            }

            ctrl = ctrl_in_rx.recv() => {
                let msg = match ctrl {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => break SessionExit::Error(e),
                    None => break SessionExit::Error(anyhow!("control stream closed")),
                };
                health_probe.mark_alive();
                sync_client_probe_timer(
                    &health_probe,
                    &mut probe_sleep,
                    &mut probe_deadline_armed,
                );
                match msg {
                    ControlMessage::Enter { edge, normalized_pos, lock_state } => {
                        leave_gen.fetch_add(1, Ordering::AcqRel);
                        focused = true;
                        last_seq = None;
                        client_lmb_down = false;
                        dnd.set_drag_armed(false);
                        if let Err(e) = injector.enter(edge, normalized_pos) {
                            warn!(error = %e, "Failed positioning cursor on Enter");
                        }
                        if config.sync_lock_keys {
                            let _ = injector.sync_lock_state(lock_state);
                        }
                        // Fetch any lazily offered large clipboard item now that user entered this screen!
                        if config.clipboard {
                            if let Some(sha256) = pending_offer_hash.take() {
                                let _ = ctrl_out_tx
                                    .send(ControlMessage::ClipboardRequest { sha256 });
                            }
                        }
                        if let Some(rt) = runtime {
                            rt.update(|s| s.active_target = "Local (Controlling)".into());
                        }
                    }
                    ControlMessage::EnterAt { norm_x, norm_y, lock_state } => {
                        leave_gen.fetch_add(1, Ordering::AcqRel);
                        focused = true;
                        last_seq = None;
                        client_lmb_down = false;
                        dnd.set_drag_armed(false);
                        if let Err(e) = injector.enter_at(norm_x, norm_y) {
                            warn!(error = %e, "Failed positioning cursor on EnterAt");
                        }
                        if config.sync_lock_keys {
                            let _ = injector.sync_lock_state(lock_state);
                        }
                        if config.clipboard {
                            if let Some(sha256) = pending_offer_hash.take() {
                                let _ = ctrl_out_tx
                                    .send(ControlMessage::ClipboardRequest { sha256 });
                            }
                        }
                        if let Some(rt) = runtime {
                            rt.update(|s| s.active_target = "Local (Controlling)".into());
                        }
                    }
                    ControlMessage::ReleaseAll => {
                        let _ = injector.release_all();
                    }
                    ControlMessage::Leave { .. } => {
                        leave_gen.fetch_add(1, Ordering::AcqRel);
                        focused = false;
                        client_lmb_down = false;
                        last_drag_move_at = None;
                        dnd.set_drag_armed(false);
                        if let Some(inc) = incoming_drag.take() {
                            if !inc.drop_requested {
                                early_drag_files.remove_drag(inc.drag_id);
                                inc.cancel(Arc::clone(&dnd));
                            } else {
                                incoming_drag = Some(inc);
                            }
                        }
                        let _ = injector.release_all();
                        let _ = injector.leave();
                        if let Some(rt) = runtime {
                            rt.update(|s| s.active_target = server_name.clone());
                        }
                    }
                    ControlMessage::DragStart { drag_id, files } => {
                        if config.drag_drop_files {
                            debug!(drag_id, count = files.len(), "Incoming cross-edge drag from server");
                            last_drag_move_at = None;
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
                                let (nx, ny) = injector.cursor_norm();
                                let (px, py) = client_geom.denormalize_point(nx, ny);
                                dnd.on_incoming_drag_start(&inc.filenames, px, py);
                                inc.maybe_start_live_staging(
                                    receive_dir.clone(),
                                    config.receive_dir.clone(),
                                    Arc::clone(&dnd),
                                );
                                incoming_drag = Some(inc);
                            }
                        }
                    }
                    ControlMessage::DragDrop { drag_id } => {
                        if let Some(ref mut inc) = incoming_drag {
                            if inc.request_drop(drag_id) && inc.is_ready_to_commit() {
                                last_drag_move_at = None;
                                if let Some(done) = incoming_drag.take() {
                                    done.commit(
                                        receive_dir.clone(),
                                        config.receive_dir.clone(),
                                        Arc::clone(&dnd),
                                        runtime.cloned(),
                                    );
                                }
                            }
                        }
                    }
                    ControlMessage::DragCancel { drag_id } => {
                        early_drag_files.remove_drag(drag_id);
                        if incoming_drag.as_ref().is_some_and(|i| i.drag_id == drag_id) {
                            debug!(drag_id, "Cancelled incoming cross-edge drag from server");
                            last_drag_move_at = None;
                            if let Some(inc) = incoming_drag.take() {
                                inc.cancel(Arc::clone(&dnd));
                            }
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
                        dnd.set_active_edges(&[return_edge], config.dead_corner_px);
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
                            if button == MouseButton::Left {
                                if pressed {
                                    client_lmb_down = true;
                                    if config.drag_drop_files && incoming_drag.is_none() {
                                        let (nx, ny) = injector.cursor_norm();
                                        let (px, py) = client_geom.denormalize_point(nx, ny);
                                        let started_on_edge =
                                            client_geom.distance_to_edge(return_edge, px, py) <= 4;
                                        if !started_on_edge {
                                            dnd.set_drag_armed(true);
                                        }
                                    }
                                } else {
                                    client_lmb_down = false;
                                    dnd.set_drag_armed(false);
                                }
                            }
                        }
                    }
                    ControlMessage::ClipboardOffer { sha256, .. } => {
                        if config.clipboard {
                            if focused {
                                let _ = ctrl_out_tx
                                    .send(ControlMessage::ClipboardRequest { sha256 });
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
                        let _ = ctrl_out_tx.send(ControlMessage::Pong { ts_us });
                    }
                    ControlMessage::Pong { .. } => {
                        let now = Instant::now();
                        let rtt_ms = conn.rtt().as_secs_f32() * 1000.0;
                        let stats = conn.stats();
                        let lost_delta = stats.path.lost_packets.saturating_sub(last_lost_packets);
                        let cong_delta = stats
                            .path
                            .congestion_events
                            .saturating_sub(last_congestion_events);
                        last_lost_packets = stats.path.lost_packets;
                        last_congestion_events = stats.path.congestion_events;
                        if (lost_delta > 0 || cong_delta > 0)
                            && rtt_ms > 30.0
                            && last_lag_warn_at.is_none_or(|t| {
                                now.saturating_duration_since(t) >= LAG_WARN_MIN_INTERVAL
                            })
                        {
                            last_lag_warn_at = Some(now);
                            warn!(
                                server = %server_name,
                                rtt_ms = format_args!("{rtt_ms:.1}"),
                                lost_packets_delta = lost_delta,
                                congestion_events_delta = cong_delta,
                                "Client QUIC lag spike detected (packet loss/congestion with RTT > 30ms)"
                            );
                        }
                        if let Some(rt) = runtime {
                            rt.record_rtt(rtt_ms);
                        }
                    }
                    _ => {}
                }
            }

            remote_bulk = bulk_rx.recv(), if bulk_open => {
                match remote_bulk {
                    Some(BulkTransfer::Clipboard(payload)) => {
                        health_probe.mark_alive();
                        sync_client_probe_timer(
                            &health_probe,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        );
                        if config.clipboard {
                            if let Some(rt) = runtime {
                                rt.record_clipboard("received", &payload);
                            }
                            if let Some(ref tx) = clip_apply_tx {
                                debug!(format = ?payload.format, len = payload.data.len(), "Queued server clipboard for serial background apply");
                                let _ = tx.send(payload);
                            }
                        }
                    }
                    Some(BulkTransfer::File(file)) => {
                        health_probe.mark_alive();
                        sync_client_probe_timer(
                            &health_probe,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        );
                        let filename = file.filename;
                        let data = file.data;
                        let byte_len = data.len();
                        let rt_clone = runtime.cloned();
                        let rdir_override = receive_dir.clone();
                        let cfg_receive_dir = config.receive_dir.clone();
                        tokio::spawn(async move {
                            let fname_for_save = filename.clone();
                            let saved = tokio::task::spawn_blocking(move || {
                                let dir_res = match rdir_override {
                                    Some(d) => Ok(d),
                                    None => resolve_receive_dir(cfg_receive_dir.as_deref()),
                                };
                                dir_res
                                    .and_then(|dir| save_received_file(&dir, &fname_for_save, &data))
                            })
                            .await;
                            match saved {
                                Ok(Ok(path)) => {
                                    info!(file = %filename, path = %path.display(), bytes = byte_len, "Received file from server");
                                    if let Some(rt) = rt_clone {
                                        rt.record_file("received", filename, byte_len, Some(path));
                                    }
                                }
                                Ok(Err(e)) => warn!(error = %e, "Failed saving received file"),
                                Err(e) => warn!(error = %e, "Received file save task failed"),
                            }
                        });
                    }
                    Some(BulkTransfer::DragFile(drag_file)) => {
                        health_probe.mark_alive();
                        sync_client_probe_timer(
                            &health_probe,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        );
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
                                        last_drag_move_at = None;
                                        if let Some(done) = incoming_drag.take() {
                                            done.commit(
                                                receive_dir.clone(),
                                                config.receive_dir.clone(),
                                                Arc::clone(&dnd),
                                                runtime.cloned(),
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
                    None => {
                        bulk_open = false;
                        if conn.close_reason().is_some() {
                            break SessionExit::Error(anyhow!("server QUIC connection closed"));
                        }
                        debug!("Remote bulk uni-stream loop ended");
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
                            let _ = ctrl_out_tx.send(ControlMessage::ClipboardOffer {
                                format: payload.format,
                                sha256: payload.sha256,
                                byte_len: payload.data.len() as u32,
                            });
                        } else {
                            let c = conn.clone();
                            tokio::spawn(async move {
                                if let Err(e) = send_clipboard_on(&c, &payload).await {
                                    debug!(error = %e, "Failed sending local clipboard to server");
                                }
                            });
                        }
                        if !arm_client_probe_if_idle(
                            &mut health_probe,
                            &ctrl_out_tx,
                            &mut probe_sleep,
                            &mut probe_deadline_armed,
                        ) {
                            break SessionExit::Error(anyhow!(
                                "server unreachable during local clipboard send: control writer closed"
                            ));
                        }
                    }
                }
            }
        }
    };

    if let Some(inc) = incoming_drag.take() {
        if !inc.drop_requested {
            inc.cancel(Arc::clone(&dnd));
        }
    }
    drop(_clip_apply_guard);
    drop(_ctrl_reader_guard);
    drop(_ctrl_writer_guard);
    drop(_bulk_guard);

    match session_exit {
        SessionExit::Shutdown => {
            close_and_flush_conn(&conn, client_endpoint.as_ref(), 0, b"client shutdown").await;
            dnd.set_drag_armed(false);
            injector.discard_saved_focus();
            let _ = injector.release_all();
            let _ = injector.show_cursor();
            SessionOutcome::Shutdown
        }
        SessionExit::OsPaused {
            shutting_down,
            guard,
        } => {
            let reason: &[u8] = if shutting_down {
                b"os shutdown"
            } else {
                b"os sleep"
            };
            close_and_flush_conn(&conn, client_endpoint.as_ref(), 0, reason).await;
            dnd.set_drag_armed(false);
            injector.discard_saved_focus();
            let _ = injector.release_all();
            let _ = injector.show_cursor();
            // Drop the OS delay inhibitor ONLY AFTER QUIC CONNECTION_CLOSE has been flushed
            // onto the wire and local keys/cursor have been restored.
            drop(guard);
            SessionOutcome::OsPaused { shutting_down }
        }
        SessionExit::Error(err) => {
            if conn.close_reason().is_none() {
                close_and_flush_conn(&conn, client_endpoint.as_ref(), 0, b"client disconnected")
                    .await;
            }
            dnd.set_drag_armed(false);
            injector.discard_saved_focus();
            let _ = injector.release_all();
            let _ = injector.show_cursor();
            SessionOutcome::Disconnected {
                established: true,
                err,
            }
        }
    }
}

/// Sleeps up to `delay` between reconnect attempts while watching for graceful shutdown,
/// OS lifecycle pause/resume, push-based injector events, OS network-change notifications,
/// and strictly listen-only LAN server announcements (0 probes sent, 0.00 Hz polling).
#[allow(clippy::too_many_arguments)]
async fn wait_reconnect_delay(
    delay: Duration,
    last_failed_addr: Option<SocketAddr>,
    config: &mut ClientConfig,
    injector: &mut dyn InputInjector,
    injector_events: &mut Option<mpsc::UnboundedReceiver<InjectorEvent>>,
    last_local_ips: &mut Vec<String>,
    runtime: Option<&RuntimeState>,
    lifecycle: &mut Option<&mut SystemLifecycleMonitor>,
    net_monitor: &mut Option<&mut NetworkChangeMonitor>,
    cmd_rx: &mut Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> WaitOutcome {
    let sleep_fut = tokio::time::sleep(delay);
    tokio::pin!(sleep_fut);
    let mut passive = if config.server.is_none() && config.discovery_port != 0 {
        PassiveDiscoverySession::listen_only(config.discovery_port)
            .await
            .ok()
    } else {
        None
    };

    loop {
        tokio::select! {
            _ = &mut sleep_fut => return WaitOutcome::Retry,
            cmd = recv_cmd_opt(cmd_rx) => {
                if apply_pre_session_cmd(cmd, config, Some(&mut *injector)) {
                    return WaitOutcome::Shutdown;
                }
            }
            ev = next_lifecycle_event(lifecycle) => {
                match ev {
                    SystemLifecycleEvent::PreparingToShutdown(guard) => {
                        drop(guard);
                        return WaitOutcome::OsPaused { shutting_down: true };
                    }
                    SystemLifecycleEvent::PreparingToSleep(guard) => {
                        drop(guard);
                        return WaitOutcome::OsPaused { shutting_down: false };
                    }
                    SystemLifecycleEvent::Resumed => {
                        info!("OS resumed during reconnect backoff; retrying immediately");
                        injector.discard_saved_focus();
                        let fresh_geom = injector.desktop_geometry().ok();
                        let fresh_ips = local_ipv4_addresses_async().await;
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
                }
            }
            inj_ev = next_injector_event(injector_events) => {
                match inj_ev {
                    InjectorEvent::SystemSuspended => {
                        injector.discard_saved_focus();
                        let _ = injector.release_all();
                        let _ = injector.show_cursor();
                    }
                    InjectorEvent::SystemResumed => {
                        info!("Client backend reported wake during reconnect backoff; retrying immediately");
                        injector.discard_saved_focus();
                        let fresh_geom = injector.desktop_geometry().ok();
                        let fresh_ips = local_ipv4_addresses_async().await;
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
                    InjectorEvent::GeometryChanged(new_geom) => {
                        injector.apply_geometry(new_geom.clone());
                        if let Some(rt) = runtime {
                            rt.update(|s| s.geometry = new_geom);
                        }
                    }
                    InjectorEvent::LocalCursorActivity => {}
                }
            }
            _ = next_network_change(net_monitor) => {
                let fresh_ips = local_ipv4_addresses_async().await;
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
            Some(srv) = next_passive_beacon(&mut passive) => {
                if last_failed_addr != Some(srv.addr)
                    && config
                        .server_name
                        .as_deref()
                        .is_none_or(|f| srv.name.eq_ignore_ascii_case(f))
                {
                    debug!(
                        addr = %srv.addr,
                        "Heard Glide server discovery announcement at new address during backoff; retrying immediately"
                    );
                    return WaitOutcome::FastRetry;
                }
            }
        }
    }
}

/// Runs the Glide client reconnect supervisor with pre-constructed OS (or mock) backends,
/// fast post-wake/roaming backoff reset, fingerprint-verified server roaming recovery,
/// and responsive graceful shutdown.
pub async fn run_client_with_backends(
    config: ClientConfig,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    runtime: Option<RuntimeState>,
    cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    run_client_with_backends_and_dnd(
        config,
        injector,
        clipboard,
        Arc::new(NoopDndBackend),
        None,
        runtime,
        cmd_rx,
    )
    .await
}

/// Runs the Glide client reconnect supervisor with explicit DnD backend and optional receive_dir.
#[allow(clippy::too_many_arguments)]
pub async fn run_client_with_backends_and_dnd(
    config: ClientConfig,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    dnd: Arc<dyn DndBackend>,
    receive_dir: Option<PathBuf>,
    runtime: Option<RuntimeState>,
    cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    run_client_with_backends_and_lifecycle(
        config,
        injector,
        clipboard,
        dnd,
        receive_dir,
        runtime,
        cmd_rx,
        None,
    )
    .await
}

/// Runs the Glide client reconnect supervisor with explicit DnD backend, optional receive_dir,
/// and an OS [`SystemLifecycleMonitor`] for event-driven shutdown/sleep snap-back and auto-resume.
#[allow(clippy::too_many_arguments)]
pub async fn run_client_with_backends_and_lifecycle(
    config: ClientConfig,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    dnd: Arc<dyn DndBackend>,
    receive_dir: Option<PathBuf>,
    runtime: Option<RuntimeState>,
    cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
    lifecycle: Option<SystemLifecycleMonitor>,
) -> Result<()> {
    run_client_with_backends_and_monitors(
        config,
        injector,
        clipboard,
        dnd,
        receive_dir,
        runtime,
        cmd_rx,
        lifecycle,
        None,
    )
    .await
}

/// Runs the Glide client reconnect supervisor with explicit DnD backend, optional receive_dir,
/// OS [`SystemLifecycleMonitor`], and OS [`NetworkChangeMonitor`] (zero periodic polling).
#[allow(clippy::too_many_arguments)]
pub async fn run_client_with_backends_and_monitors(
    mut config: ClientConfig,
    injector: &mut dyn InputInjector,
    clipboard: &mut Option<ClipboardHandle>,
    dnd: Arc<dyn DndBackend>,
    receive_dir: Option<PathBuf>,
    runtime: Option<RuntimeState>,
    mut cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
    mut lifecycle: Option<SystemLifecycleMonitor>,
    mut net_monitor: Option<NetworkChangeMonitor>,
) -> Result<()> {
    injector.set_unfocus_on_leave(config.unfocus_on_leave);
    let mut injector_events = injector.take_event_rx();
    let geom = injector.desktop_geometry().unwrap_or_default();
    let mut last_local_ips = local_ipv4_addresses_async().await;
    let shared_clip_backend = share_clipboard_backend(clipboard);

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
    let mut pending_os_pause: Option<bool> = None;

    loop {
        if let Some(mon) = lifecycle.as_mut() {
            while let Some(ev) = mon.try_recv() {
                match ev {
                    SystemLifecycleEvent::PreparingToShutdown(guard) => {
                        drop(guard);
                        pending_os_pause = Some(true);
                    }
                    SystemLifecycleEvent::PreparingToSleep(guard) => {
                        drop(guard);
                        pending_os_pause = Some(pending_os_pause.unwrap_or(false));
                    }
                    SystemLifecycleEvent::Resumed => {
                        pending_os_pause = None;
                        backoff.reset();
                    }
                }
            }
        }

        if let Some(shutting_down) = pending_os_pause.take() {
            let mut lc_ref = lifecycle.as_mut();
            let mut net_ref = net_monitor.as_mut();
            let mut rx_ref = cmd_rx.as_mut();
            match wait_while_os_paused(
                shutting_down,
                &mut config,
                injector,
                &mut injector_events,
                runtime.as_ref(),
                &mut lc_ref,
                &mut net_ref,
                &mut rx_ref,
            )
            .await
            {
                PauseOutcome::Shutdown => {
                    info!("Client received graceful shutdown while OS-paused");
                    break;
                }
                PauseOutcome::Resumed => {
                    backoff.reset();
                    sleep_detector.reset();
                    if let Some(rx) = injector_events.as_mut() {
                        while let Ok(ev) = rx.try_recv() {
                            if let InjectorEvent::GeometryChanged(g) = ev {
                                injector.apply_geometry(g);
                            }
                        }
                    }
                    injector.discard_saved_focus();
                    let fresh_geom = injector.desktop_geometry().ok();
                    let fresh_ips = local_ipv4_addresses_async().await;
                    last_local_ips = fresh_ips.clone();
                    if let Some(ref rt) = runtime {
                        rt.update(|s| {
                            if let Some(g) = fresh_geom {
                                s.geometry = g;
                            }
                            s.local_ips = fresh_ips;
                        });
                    }
                    continue;
                }
            }
        }

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
            let mut lc_ref = lifecycle.as_mut();
            let mut net_ref = net_monitor.as_mut();
            let mut rx_ref = cmd_rx.as_mut();
            resolve_server_addr(
                &mut config,
                injector,
                &mut injector_events,
                &mut last_local_ips,
                runtime.as_ref(),
                &mut lc_ref,
                &mut net_ref,
                &mut rx_ref,
            )
            .await
        };

        let mut immediate_retry = false;
        let mut last_failed_addr: Option<SocketAddr> = None;

        match target_addr {
            Ok(ResolveOutcome::Shutdown) => {
                info!("Client received graceful shutdown while resolving server");
                break;
            }
            Ok(ResolveOutcome::OsPaused { shutting_down }) => {
                pending_os_pause = Some(shutting_down);
                continue;
            }
            Ok(ResolveOutcome::Addr(addr)) => {
                sleep_detector.reset();
                match run_client_session_tracked(
                    &mut config,
                    addr,
                    injector,
                    &mut injector_events,
                    clipboard,
                    shared_clip_backend.clone(),
                    Arc::clone(&dnd),
                    receive_dir.clone(),
                    runtime.as_ref(),
                    cmd_rx.as_mut(),
                    lifecycle.as_mut(),
                    net_monitor.as_mut(),
                )
                .await
                {
                    SessionOutcome::Shutdown => {
                        info!("Session ended cleanly");
                        break;
                    }
                    SessionOutcome::OsPaused { shutting_down } => {
                        pending_os_pause = Some(shutting_down);
                        continue;
                    }
                    SessionOutcome::Disconnected {
                        established,
                        err: e,
                    } => {
                        warn!(
                            error = %e,
                            %addr,
                            established,
                            "Disconnected from server; reconnecting shortly..."
                        );
                        if established {
                            backoff.reset();
                            sleep_detector.reset();
                        } else {
                            last_failed_addr = Some(addr);
                            if sleep_detector.check().is_some() {
                                backoff.reset();
                            }
                        }

                        if let Some(ref rt) = runtime {
                            rt.update(|s| {
                                s.status = "Reconnecting...".into();
                                s.remote_fingerprint = None;
                                s.last_error = Some(e.to_string());
                            });
                        }
                        injector.discard_saved_focus();
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
                                    let mut lc_ref = lifecycle.as_mut();
                                    let mut rx_ref = cmd_rx.as_mut();
                                    match discover_roamed_server_by_fingerprint(
                                        &mut config,
                                        &expected_fp,
                                        &mut lc_ref,
                                        &mut rx_ref,
                                    )
                                    .await
                                    {
                                        RoamLookup::Shutdown => {
                                            info!("Client received shutdown during roaming discovery");
                                            break;
                                        }
                                        RoamLookup::OsPaused { shutting_down } => {
                                            pending_os_pause = Some(shutting_down);
                                            continue;
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
                        let mut lc_ref = lifecycle.as_mut();
                        let mut rx_ref = cmd_rx.as_mut();
                        match discover_roamed_server_by_fingerprint(
                            &mut config,
                            &expected_fp,
                            &mut lc_ref,
                            &mut rx_ref,
                        )
                        .await
                        {
                            RoamLookup::Shutdown => {
                                info!("Client received shutdown during roaming discovery");
                                break;
                            }
                            RoamLookup::OsPaused { shutting_down } => {
                                pending_os_pause = Some(shutting_down);
                                continue;
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
        let mut os_resumed = false;
        if let Some(rx) = injector_events.as_mut() {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    InjectorEvent::SystemResumed => os_resumed = true,
                    InjectorEvent::GeometryChanged(g) => {
                        injector.apply_geometry(g.clone());
                        if let Some(ref rt) = runtime {
                            rt.update(|s| s.geometry = g);
                        }
                    }
                    InjectorEvent::SystemSuspended | InjectorEvent::LocalCursorActivity => {}
                }
            }
        }
        if os_resumed {
            injector.discard_saved_focus();
        }
        let fresh_ips = local_ipv4_addresses_async().await;
        let ips_changed = fresh_ips != last_local_ips;
        if os_resumed || ips_changed {
            last_local_ips = fresh_ips.clone();
            if let Some(ref rt) = runtime {
                rt.update(|s| s.local_ips = fresh_ips);
            }
            backoff.reset();
        }

        let delay = backoff.next_delay();
        let mut lc_ref = lifecycle.as_mut();
        let mut net_ref = net_monitor.as_mut();
        let mut rx_ref = cmd_rx.as_mut();
        match wait_reconnect_delay(
            delay,
            last_failed_addr,
            &mut config,
            injector,
            &mut injector_events,
            &mut last_local_ips,
            runtime.as_ref(),
            &mut lc_ref,
            &mut net_ref,
            &mut rx_ref,
        )
        .await
        {
            WaitOutcome::Shutdown => {
                info!("Client received graceful shutdown command during reconnect backoff");
                break;
            }
            WaitOutcome::OsPaused { shutting_down } => {
                pending_os_pause = Some(shutting_down);
            }
            WaitOutcome::FastRetry => {
                backoff.reset();
            }
            WaitOutcome::Retry => {}
        }
    }

    injector.discard_saved_focus();
    let _ = injector.release_all();
    let _ = injector.show_cursor();
    if let Some(ref rt) = runtime {
        rt.update(|s| {
            s.mode = "stopped".into();
            s.status = "Idle".into();
            s.active_target = "Local".into();
            s.remote_fingerprint = None;
            s.rtt_ms = None;
            s.rtt_history.clear();
        });
    }
    Ok(())
}

/// Runs the native Glide client with automatic LAN discovery, reconnect, and optional GUI hooks.
pub async fn run_native_client_with_runtime(
    mut config: ClientConfig,
    runtime: Option<RuntimeState>,
    cmd_rx: Option<mpsc::UnboundedReceiver<RuntimeCommand>>,
) -> Result<()> {
    glide_backend::init_platform();
    if config.known_servers_path.is_none() {
        config.known_servers_path = Some(default_known_servers_path());
    }
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

    let dnd = glide_backend::start_dnd();
    let lifecycle = SystemLifecycleMonitor::start();
    let net_monitor = NetworkChangeMonitor::start();
    run_client_with_backends_and_monitors(
        config,
        injector.as_mut(),
        &mut clipboard,
        dnd,
        None,
        runtime,
        cmd_rx,
        Some(lifecycle),
        Some(net_monitor),
    )
    .await
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
    use std::sync::atomic::{AtomicBool, Ordering};
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
        let event_tx = injector.event_tx.clone();
        let mut injector_events = injector.take_event_rx();
        let mut config = ClientConfig {
            server: Some("127.0.0.1:24800".into()),
            ..Default::default()
        };
        let mut ips = local_ipv4_addresses_async().await;
        let mut lifecycle: Option<&mut SystemLifecycleMonitor> = None;
        let mut net_monitor: Option<&mut NetworkChangeMonitor> = None;
        let mut cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>> = None;

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let _ = event_tx.send(InjectorEvent::SystemResumed);
        });

        let started = Instant::now();
        let outcome = wait_reconnect_delay(
            Duration::from_secs(3),
            None,
            &mut config,
            &mut injector,
            &mut injector_events,
            &mut ips,
            None,
            &mut lifecycle,
            &mut net_monitor,
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
        let mut injector_events = injector.take_event_rx();
        let mut config = ClientConfig {
            server: Some("127.0.0.1:24800".into()),
            ..Default::default()
        };
        let mut ips = local_ipv4_addresses_async().await;
        let mut lifecycle: Option<&mut SystemLifecycleMonitor> = None;
        let mut net_monitor: Option<&mut NetworkChangeMonitor> = None;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut cmd_rx = Some(&mut rx);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let _ = tx.send(RuntimeCommand::Shutdown);
        });

        let started = Instant::now();
        let outcome = wait_reconnect_delay(
            Duration::from_secs(3),
            None,
            &mut config,
            &mut injector,
            &mut injector_events,
            &mut ips,
            None,
            &mut lifecycle,
            &mut net_monitor,
            &mut cmd_rx,
        )
        .await;

        assert!(matches!(outcome, WaitOutcome::Shutdown));
        assert!(started.elapsed() < Duration::from_millis(400));
    }

    #[tokio::test]
    async fn wait_reconnect_delay_ignores_beacons_from_last_failed_addr() {
        let port = 24895;
        let quic_port = 24815;
        let fp = format!("{}44", "44:".repeat(31));
        let failed_addr: SocketAddr = format!("127.0.0.1:{quic_port}").parse().unwrap();

        let (mut injector, _) = MockInjector::new(test_geom());
        let mut injector_events = injector.take_event_rx();
        let mut config = ClientConfig {
            server: None,
            discovery_port: port,
            ..Default::default()
        };
        let mut ips = local_ipv4_addresses_async().await;
        let mut lifecycle: Option<&mut SystemLifecycleMonitor> = None;
        let mut net_monitor: Option<&mut NetworkChangeMonitor> = None;
        let mut cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>> = None;

        // Emit a beacon from the exact same loopback address that just failed during backoff.
        let beacon_payload = serde_json::to_vec(&glide_net::discovery::ServerBeacon {
            magic: "GLIDE_BEACON_V1".into(),
            name: "failing-srv".into(),
            quic_port,
            fingerprint: fp,
            os: "linux-x11".into(),
            ips: vec!["127.0.0.1".into()],
        })
        .unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            if let Ok(sock) = tokio::net::UdpSocket::bind("127.0.0.1:0").await {
                let _ = sock.send_to(&beacon_payload, ("127.0.0.1", port)).await;
            }
        });

        let started = Instant::now();
        let outcome = wait_reconnect_delay(
            Duration::from_millis(220),
            Some(failed_addr),
            &mut config,
            &mut injector,
            &mut injector_events,
            &mut ips,
            None,
            &mut lifecycle,
            &mut net_monitor,
            &mut cmd_rx,
        )
        .await;

        assert!(
            matches!(outcome, WaitOutcome::Retry),
            "beacons from last_failed_addr must not short-circuit reconnect backoff"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(180),
            "backoff should wait full delay instead of spinning, elapsed={:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn wait_while_os_paused_releases_duplicate_inhibitors_and_resumes_on_event() {
        let (mut injector, _) = MockInjector::new(test_geom());
        let mut injector_events = injector.take_event_rx();
        let mut config = ClientConfig::default();
        let rt = RuntimeState::new();
        let (mut monitor, lc_tx) = SystemLifecycleMonitor::from_channel();
        let mut lc_ref = Some(&mut monitor);
        let mut net_ref: Option<&mut NetworkChangeMonitor> = None;
        let mut cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>> = None;

        let dup_released = Arc::new(AtomicBool::new(false));
        let dup_released_c = Arc::clone(&dup_released);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let _ = lc_tx.send(SystemLifecycleEvent::PreparingToShutdown(
                ShutdownInhibitorGuard::with_on_drop(move || {
                    dup_released_c.store(true, Ordering::SeqCst);
                }),
            ));
            tokio::time::sleep(Duration::from_millis(40)).await;
            let _ = lc_tx.send(SystemLifecycleEvent::Resumed);
        });

        let started = Instant::now();
        let outcome = wait_while_os_paused(
            false,
            &mut config,
            &mut injector,
            &mut injector_events,
            Some(&rt),
            &mut lc_ref,
            &mut net_ref,
            &mut cmd_rx,
        )
        .await;

        assert!(matches!(outcome, PauseOutcome::Resumed));
        assert!(dup_released.load(Ordering::SeqCst));
        assert_eq!(rt.snapshot().status, "Paused (OS shutting down)");
        assert!(started.elapsed() < Duration::from_millis(600));
    }

    #[tokio::test]
    async fn wait_while_os_paused_exits_immediately_on_shutdown_command() {
        let (mut injector, _) = MockInjector::new(test_geom());
        let mut injector_events = injector.take_event_rx();
        let mut config = ClientConfig::default();
        let (mut monitor, _lc_tx) = SystemLifecycleMonitor::from_channel();
        let mut lc_ref = Some(&mut monitor);
        let mut net_ref: Option<&mut NetworkChangeMonitor> = None;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut cmd_ref = Some(&mut cmd_rx);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = cmd_tx.send(RuntimeCommand::Shutdown);
        });

        let started = Instant::now();
        let outcome = wait_while_os_paused(
            false,
            &mut config,
            &mut injector,
            &mut injector_events,
            None,
            &mut lc_ref,
            &mut net_ref,
            &mut cmd_ref,
        )
        .await;

        assert!(matches!(outcome, PauseOutcome::Shutdown));
        assert!(started.elapsed() < Duration::from_millis(400));
    }

    #[tokio::test]
    async fn wait_while_os_paused_resumes_cleanly_if_monitor_disconnects() {
        let (mut injector, _) = MockInjector::new(test_geom());
        let mut injector_events = injector.take_event_rx();
        let mut config = ClientConfig::default();
        let (mut monitor, lc_tx) = SystemLifecycleMonitor::from_channel();
        let mut lc_ref = Some(&mut monitor);
        let mut net_ref: Option<&mut NetworkChangeMonitor> = None;
        let mut cmd_rx: Option<&mut mpsc::UnboundedReceiver<RuntimeCommand>> = None;

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(lc_tx);
        });

        let started = Instant::now();
        let outcome = wait_while_os_paused(
            true,
            &mut config,
            &mut injector,
            &mut injector_events,
            None,
            &mut lc_ref,
            &mut net_ref,
            &mut cmd_rx,
        )
        .await;

        assert!(matches!(outcome, PauseOutcome::Resumed));
        assert!(started.elapsed() < Duration::from_millis(400));
    }
}
