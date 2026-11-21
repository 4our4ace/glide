//! Linux Wayland server input capture via `org.freedesktop.portal.InputCapture` + `libei` (`reis`).
//!
//! Registers compositor-enforced pointer barriers along active outer screen edges (honoring
//! `dead_corner_px`), receives raw mouse deltas, scroll, buttons, and evdev keycodes over EIS
//! while captured, and calls `InputCapture::Release` with exact cursor coordinates when returning
//! control to the local desktop.

use crate::geometry::{DesktopGeometry, MonitorRect};
use crate::linux_wayland::{
    call_portal_request, close_portal_session, evdev_to_mouse_button, extract_object_path,
    next_portal_token, opt_str, INPUT_CAPTURE_IFACE, PORTAL_DEST, PORTAL_PATH,
};
use crate::{CaptureConfig, CaptureControl, CaptureEvent, CaptureHandle, RemoteMediaKeyTracker};
use anyhow::{anyhow, Context, Result};
use glide_hid::HidUsage;
use glide_proto::{Edge, LockState};
use parking_lot::Mutex;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent, EiEventConverter};
use reis::PendingRequestResult;
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::blocking::{Connection as DbusConnection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

const CAP_KEYBOARD: u32 = 1;
const CAP_POINTER: u32 = 2;

#[derive(Debug, Clone)]
pub(crate) struct BarrierSpec {
    pub id: u32,
    pub edge: Edge,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

/// Computes InputCapture pointer barriers along each active outer edge, respecting `dead_corner_px`.
pub(crate) fn build_edge_barriers(
    geometry: &DesktopGeometry,
    active_edges: &[Edge],
    dead_corner_px: i32,
) -> Vec<BarrierSpec> {
    let mut barriers = Vec::new();
    let (min_x, min_y, max_x, max_y) = geometry.bounds();
    const TOL: i32 = 4;
    let dead = dead_corner_px.max(0);
    let mut next_id = 1u32;

    for &edge in active_edges {
        for m in &geometry.monitors {
            let on_edge = match edge {
                Edge::Left => (m.left() - min_x).abs() <= TOL,
                Edge::Right => (m.right() - max_x).abs() <= TOL,
                Edge::Top => (m.top() - min_y).abs() <= TOL,
                Edge::Bottom => (m.bottom() - max_y).abs() <= TOL,
            };
            if !on_edge {
                continue;
            }
            let (x1, y1, x2, y2) = match edge {
                Edge::Left => {
                    let d = dead.min(m.height / 4);
                    (m.left(), m.top() + d, m.left(), m.bottom() - d)
                }
                Edge::Right => {
                    let d = dead.min(m.height / 4);
                    (m.right(), m.top() + d, m.right(), m.bottom() - d)
                }
                Edge::Top => {
                    let d = dead.min(m.width / 4);
                    (m.left() + d, m.top(), m.right() - d, m.top())
                }
                Edge::Bottom => {
                    let d = dead.min(m.width / 4);
                    (m.left() + d, m.bottom(), m.right() - d, m.bottom())
                }
            };
            if (x2 - x1).abs() >= 2 || (y2 - y1).abs() >= 2 {
                barriers.push(BarrierSpec {
                    id: next_id,
                    edge,
                    x1,
                    y1,
                    x2,
                    y2,
                });
                next_id += 1;
            }
        }
    }
    barriers
}

/// Computes normalized `[0.0, 1.0]` edge coordinate for a barrier activation point.
pub(crate) fn normalize_barrier_hit(
    geometry: &DesktopGeometry,
    edge: Edge,
    x: f64,
    y: f64,
) -> f32 {
    let (min_x, min_y, max_x, max_y) = geometry.bounds();
    match edge {
        Edge::Left | Edge::Right => {
            let span = (max_y - min_y).max(1) as f64;
            ((y - min_y as f64) / span).clamp(0.0, 1.0) as f32
        }
        Edge::Top | Edge::Bottom => {
            let span = (max_x - min_x).max(1) as f64;
            ((x - min_x as f64) / span).clamp(0.0, 1.0) as f32
        }
    }
}

fn parse_zones(results: &HashMap<String, OwnedValue>) -> (DesktopGeometry, u32) {
    let zone_set = results
        .get("zone_set")
        .and_then(|v| v.downcast_ref::<u32>().ok())
        .unwrap_or(0);

    let mut monitors = Vec::new();
    if let Some(zones_val) = results.get("zones") {
        if let Ok(zones) = zones_val.clone().try_into() {
            let raw: Vec<(u32, u32, i32, i32)> = zones;
            for (idx, (w, h, x, y)) in raw.into_iter().enumerate() {
                if w == 0 || h == 0 {
                    continue;
                }
                monitors.push(MonitorRect {
                    name: format!("zone-{idx}"),
                    x,
                    y,
                    width: w as i32,
                    height: h as i32,
                    scale: 1.0,
                    primary: idx == 0,
                });
            }
        }
    }

    let geometry = if monitors.is_empty() {
        crate::linux_wayland::query_desktop_geometry().unwrap_or_else(|_| DesktopGeometry::fallback())
    } else {
        DesktopGeometry::new(monitors)
    };
    (geometry, zone_set)
}

#[derive(Debug)]
enum ControlMsg {
    UpdateBarriers,
    Release { x: f64, y: f64 },
    Shutdown,
}

struct SharedState {
    geometry: Mutex<DesktopGeometry>,
    active_edges: Mutex<Vec<Edge>>,
    barriers: Mutex<Vec<BarrierSpec>>,
    dead_corner_px: Mutex<i32>,
    locked: AtomicBool,
    in_remote: AtomicBool,
    pin_media_keys_local: AtomicBool,
    activation_id: Mutex<Option<u32>>,
    last_local_norm: Mutex<(f32, f32)>,
    zone_set: Mutex<u32>,
}

struct WaylandCaptureControl {
    shared: Arc<SharedState>,
    cmd_tx: std::sync::mpsc::Sender<ControlMsg>,
}

impl CaptureControl for WaylandCaptureControl {
    fn set_active_edges(&self, edges: &[Edge]) {
        *self.shared.active_edges.lock() = edges.to_vec();
        let _ = self.cmd_tx.send(ControlMsg::UpdateBarriers);
    }

    fn update_tuning(
        &self,
        _edge_threshold_px: i32,
        dead_corner_px: i32,
        _edge_dwell_ms: u64,
        _auto_lock_fullscreen: bool,
        locked: bool,
    ) {
        let mut dc = self.shared.dead_corner_px.lock();
        let changed_dc = *dc != dead_corner_px;
        *dc = dead_corner_px;
        drop(dc);
        let prev_locked = self.shared.locked.swap(locked, Ordering::SeqCst);
        if changed_dc || prev_locked != locked {
            let _ = self.cmd_tx.send(ControlMsg::UpdateBarriers);
        }
    }

    fn set_pin_media_keys_local(&self, enabled: bool) {
        self.shared
            .pin_media_keys_local
            .store(enabled, Ordering::Relaxed);
    }

    fn enter_remote(&self) -> Result<()> {
        self.shared.in_remote.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()> {
        self.shared.in_remote.store(false, Ordering::SeqCst);
        let (x, y) = self
            .shared
            .geometry
            .lock()
            .point_on_edge(return_edge, normalized_pos, 4);
        let _ = self.cmd_tx.send(ControlMsg::Release {
            x: x as f64,
            y: y as f64,
        });
        Ok(())
    }

    fn leave_remote_at(&self, norm_x: f32, norm_y: f32) -> Result<()> {
        self.shared.in_remote.store(false, Ordering::SeqCst);
        let (x, y) = self
            .shared
            .geometry
            .lock()
            .denormalize_point(norm_x, norm_y);
        let _ = self.cmd_tx.send(ControlMsg::Release {
            x: x as f64,
            y: y as f64,
        });
        Ok(())
    }

    fn desktop_geometry(&self) -> DesktopGeometry {
        self.shared.geometry.lock().clone()
    }

    fn lock_state(&self) -> LockState {
        LockState::default()
    }

    fn last_local_cursor_norm(&self) -> (f32, f32) {
        *self.shared.last_local_norm.lock()
    }
}

impl Drop for WaylandCaptureControl {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(ControlMsg::Shutdown);
    }
}

fn apply_barriers(
    conn: &DbusConnection,
    proxy: &Proxy<'_>,
    session_path: &OwnedObjectPath,
    shared: &SharedState,
) {
    let locked = shared.locked.load(Ordering::SeqCst);
    let edges = if locked {
        Vec::new()
    } else {
        shared.active_edges.lock().clone()
    };
    let dead = *shared.dead_corner_px.lock();
    let geom = shared.geometry.lock().clone();
    let zone_set = *shared.zone_set.lock();

    let specs = build_edge_barriers(&geom, &edges, dead);
    *shared.barriers.lock() = specs.clone();

    let barrier_dicts: Vec<HashMap<&str, Value<'_>>> = specs
        .iter()
        .map(|b| {
            let mut m: HashMap<&str, Value<'_>> = HashMap::new();
            m.insert("barrier_id", Value::from(b.id));
            m.insert("position", Value::from((b.x1, b.y1, b.x2, b.y2)));
            m
        })
        .collect();

    let tok = next_portal_token("ic_barriers");
    let res = call_portal_request(conn, &tok, Duration::from_secs(10), || {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opt_str(&mut opts, "handle_token", &tok);
        let p: OwnedObjectPath = proxy.call(
            "SetPointerBarriers",
            &(session_path.as_ref(), opts, barrier_dicts, zone_set),
        )?;
        Ok(p)
    });
    if let Err(e) = res {
        warn!(error = %e, "Wayland InputCapture SetPointerBarriers failed");
        return;
    }

    let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
    if specs.is_empty() {
        let _ = proxy.call_noreply("Disable", &(session_path.as_ref(), empty_opts));
    } else if let Err(e) = proxy.call_noreply("Enable", &(session_path.as_ref(), empty_opts)) {
        warn!(error = %e, "Wayland InputCapture Enable failed");
    }
}

/// Starts the native Wayland server capture backend (`org.freedesktop.portal.InputCapture` + EIS).
pub fn start_wayland_capture(config: CaptureConfig) -> Result<CaptureHandle> {
    crate::linux_wayland::on_non_tokio_thread(move || start_wayland_capture_blocking(config))
}

fn start_wayland_capture_blocking(config: CaptureConfig) -> Result<CaptureHandle> {
    let conn = DbusConnection::session()
        .context("failed to connect to session D-Bus for Wayland InputCapture portal")?;
    let proxy = Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, INPUT_CAPTURE_IFACE)
        .context("failed to create InputCapture portal proxy")?;

    // 1. CreateSession
    let create_tok = next_portal_token("ic_create");
    let sess_tok = next_portal_token("ic_sess");
    let create_res = call_portal_request(&conn, &create_tok, Duration::from_secs(120), || {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opt_str(&mut opts, "handle_token", &create_tok);
        opt_str(&mut opts, "session_handle_token", &sess_tok);
        opts.insert("capabilities", Value::from(CAP_KEYBOARD | CAP_POINTER));
        let p: OwnedObjectPath = proxy.call("CreateSession", &("", opts))?;
        Ok(p)
    })
    .context(
        "Wayland InputCapture CreateSession failed (requires GNOME 46+, KDE Plasma 6.1+, or Hyprland with xdg-desktop-portal >= 1.18)",
    )?;

    let session_path = create_res
        .get("session_handle")
        .and_then(extract_object_path)
        .ok_or_else(|| anyhow!("InputCapture CreateSession returned no session_handle"))?;

    // 2. GetZones
    let zones_tok = next_portal_token("ic_zones");
    let zones_res = call_portal_request(&conn, &zones_tok, Duration::from_secs(15), || {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opt_str(&mut opts, "handle_token", &zones_tok);
        let p: OwnedObjectPath = proxy.call("GetZones", &(session_path.as_ref(), opts))?;
        Ok(p)
    });
    let (geometry, zone_set) = match zones_res {
        Ok(r) => parse_zones(&r),
        Err(e) => {
            close_portal_session(&conn, &session_path.as_ref());
            return Err(e).context("Wayland InputCapture GetZones failed");
        }
    };

    // 3. ConnectToEIS (Receiver)
    let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
    let eis_fd: OwnedFd = match proxy.call("ConnectToEIS", &(session_path.as_ref(), empty_opts)) {
        Ok(fd) => fd,
        Err(e) => {
            close_portal_session(&conn, &session_path.as_ref());
            return Err(anyhow!("Wayland InputCapture ConnectToEIS failed: {e}"));
        }
    };
    let std_fd: std::os::unix::io::OwnedFd = eis_fd.into();
    let stream = UnixStream::from(std_fd);
    let ei_ctx = ei::Context::new(stream).context("failed to create EIS receiver context")?;
    let handshake_resp =
        reis::handshake::ei_handshake_blocking(&ei_ctx, "Glide", ei::handshake::ContextType::Receiver)
            .map_err(|e| {
                close_portal_session(&conn, &session_path.as_ref());
                anyhow!("EIS receiver handshake failed: {e}")
            })?;

    let shared = Arc::new(SharedState {
        geometry: Mutex::new(geometry),
        active_edges: Mutex::new(Vec::new()),
        barriers: Mutex::new(Vec::new()),
        dead_corner_px: Mutex::new(config.dead_corner_px),
        locked: AtomicBool::new(false),
        in_remote: AtomicBool::new(false),
        pin_media_keys_local: AtomicBool::new(config.pin_media_keys_local),
        activation_id: Mutex::new(None),
        last_local_norm: Mutex::new((0.5, 0.5)),
        zone_set: Mutex::new(zone_set),
    });

    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ControlMsg>();

    // Thread 1: D-Bus portal signals (Activated, Deactivated, ZonesChanged) + control commands
    let shared_dbus = Arc::clone(&shared);
    let ev_tx_dbus = ev_tx.clone();
    let session_dbus = session_path.clone();
    std::thread::Builder::new()
        .name("glide-wl-ic-dbus".into())
        .spawn(move || {
            let Ok(sig_conn) = DbusConnection::session() else {
                return;
            };
            let rule = match MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(PORTAL_DEST)
                .and_then(|b| b.interface(INPUT_CAPTURE_IFACE))
                .and_then(|b| b.path(PORTAL_PATH))
            {
                Ok(b) => b.build(),
                Err(_) => return,
            };
            let Ok(iter) = MessageIterator::for_match_rule(rule, &sig_conn, Some(32)) else {
                return;
            };

            // Dedicated helper thread for incoming portal signals so control commands execute immediately.
            let shared_sig = Arc::clone(&shared_dbus);
            let session_sig = session_dbus.clone();
            let ev_tx_sig = ev_tx_dbus.clone();
            let _sig_thread = std::thread::Builder::new()
                .name("glide-wl-ic-signals".into())
                .spawn(move || {
                    for msg in iter {
                        let Ok(msg) = msg else {
                            break;
                        };
                        let member = msg
                            .header()
                            .member()
                            .map(|m| m.as_str().to_owned())
                            .unwrap_or_default();
                        let Ok((sig_sess, opts)) =
                            msg.body().deserialize::<(OwnedObjectPath, HashMap<String, OwnedValue>)>()
                        else {
                            continue;
                        };
                        if sig_sess != session_sig {
                            continue;
                        }
                        match member.as_str() {
                            "Activated" => {
                                let act_id = opts
                                    .get("activation_id")
                                    .and_then(|v| v.downcast_ref::<u32>().ok());
                                let barrier_id = opts
                                    .get("barrier_id")
                                    .and_then(|v| v.downcast_ref::<u32>().ok())
                                    .unwrap_or(0);
                                let (cx, cy) = opts
                                    .get("cursor_position")
                                    .and_then(|v| v.clone().try_into().ok())
                                    .unwrap_or((0.0f64, 0.0f64));

                                *shared_sig.activation_id.lock() = act_id;
                                let geom = shared_sig.geometry.lock().clone();
                                *shared_sig.last_local_norm.lock() =
                                    geom.normalize_point(cx.round() as i32, cy.round() as i32);

                                let edge_opt = shared_sig
                                    .barriers
                                    .lock()
                                    .iter()
                                    .find(|b| b.id == barrier_id)
                                    .map(|b| b.edge);

                                if let Some(edge) = edge_opt {
                                    let norm = normalize_barrier_hit(&geom, edge, cx, cy);
                                    shared_sig.in_remote.store(true, Ordering::SeqCst);
                                    let _ = ev_tx_sig.send(CaptureEvent::EdgeCrossed {
                                        edge,
                                        normalized_pos: norm,
                                    });
                                } else {
                                    debug!(barrier_id, "InputCapture Activated with unknown barrier_id");
                                }
                            }
                            "Deactivated" | "Disabled" => {
                                shared_sig.in_remote.store(false, Ordering::SeqCst);
                            }
                            "ZonesChanged" => {
                                if let Ok(p) = Proxy::new(
                                    &sig_conn,
                                    PORTAL_DEST,
                                    PORTAL_PATH,
                                    INPUT_CAPTURE_IFACE,
                                ) {
                                    let tok = next_portal_token("ic_zones");
                                    if let Ok(r) = call_portal_request(
                                        &sig_conn,
                                        &tok,
                                        Duration::from_secs(10),
                                        || {
                                            let mut o: HashMap<&str, Value<'_>> = HashMap::new();
                                            opt_str(&mut o, "handle_token", &tok);
                                            let hp: OwnedObjectPath =
                                                p.call("GetZones", &(session_sig.as_ref(), o))?;
                                            Ok(hp)
                                        },
                                    ) {
                                        let (g, zs) = parse_zones(&r);
                                        *shared_sig.geometry.lock() = g;
                                        *shared_sig.zone_set.lock() = zs;
                                        apply_barriers(&sig_conn, &p, &session_sig, &shared_sig);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                });

            let Ok(ctrl_proxy) = Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, INPUT_CAPTURE_IFACE)
            else {
                close_portal_session(&conn, &session_dbus.as_ref());
                return;
            };

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    ControlMsg::UpdateBarriers => {
                        apply_barriers(&conn, &ctrl_proxy, &session_dbus, &shared_dbus);
                    }
                    ControlMsg::Release { x, y } => {
                        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
                        if let Some(act_id) = *shared_dbus.activation_id.lock() {
                            opts.insert("activation_id", Value::from(act_id));
                        }
                        opts.insert("cursor_position", Value::from((x, y)));
                        if let Err(e) =
                            ctrl_proxy.call_noreply("Release", &(session_dbus.as_ref(), opts))
                        {
                            debug!(error = %e, "Wayland InputCapture Release failed");
                        }
                    }
                    ControlMsg::Shutdown => {
                        let empty: HashMap<&str, Value<'_>> = HashMap::new();
                        let _ =
                            ctrl_proxy.call_noreply("Disable", &(session_dbus.as_ref(), empty));
                        close_portal_session(&conn, &session_dbus.as_ref());
                        break;
                    }
                }
            }
        })?;

    // Thread 2: EIS receiver event loop (raw mouse deltas, buttons, scroll, evdev keys)
    let shared_eis = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("glide-wl-ic-eis".into())
        .spawn(move || {
            let mut converter = EiEventConverter::new(&ei_ctx, handshake_resp);
            let mut held_keys: HashSet<HidUsage> = HashSet::new();
            let mut media_tracker = RemoteMediaKeyTracker::new();
            let mut was_in_remote = false;
            let mut scroll_acc_x: f32 = 0.0;
            let mut scroll_acc_y: f32 = 0.0;

            let fd = ei_ctx.as_raw_fd();
            loop {
                let in_remote_now = shared_eis.in_remote.load(Ordering::Acquire);
                if was_in_remote && !in_remote_now {
                    media_tracker.clear();
                }
                was_in_remote = in_remote_now;

                while let Some(res) = ei_ctx.pending_event() {
                    let req = match res {
                        PendingRequestResult::Request(r) => r,
                        PendingRequestResult::InvalidObject(_) => continue,
                        PendingRequestResult::ParseError(e) => {
                            warn!(error = %e, "EIS receiver parse error");
                            return;
                        }
                    };
                    if let Err(e) = converter.handle_event(req) {
                        warn!(error = %e, "EIS receiver event error");
                        return;
                    }
                }

                while let Some(ev) = converter.next_event() {
                    match ev {
                        EiEvent::SeatAdded(s) => {
                            s.seat.bind_capabilities(
                                DeviceCapability::Pointer
                                    | DeviceCapability::Button
                                    | DeviceCapability::Scroll
                                    | DeviceCapability::Keyboard,
                            );
                            let _ = ei_ctx.flush();
                        }
                        EiEvent::PointerMotion(m) => {
                            if shared_eis.in_remote.load(Ordering::Relaxed)
                                && (m.dx != 0.0 || m.dy != 0.0)
                                && ev_tx
                                    .send(CaptureEvent::MouseDelta { dx: m.dx, dy: m.dy })
                                    .is_err()
                            {
                                return;
                            }
                        }
                        EiEvent::Button(b) => {
                            if shared_eis.in_remote.load(Ordering::Relaxed) {
                                if let Some(button) = evdev_to_mouse_button(b.button) {
                                    let pressed = b.state == ei::button::ButtonState::Press;
                                    if ev_tx
                                        .send(CaptureEvent::MouseButton { button, pressed })
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                        EiEvent::ScrollDiscrete(sc) => {
                            if shared_eis.in_remote.load(Ordering::Relaxed) {
                                let dx_120 = sc.discrete_dx.clamp(i16::MIN as i32, i16::MAX as i32)
                                    as i16;
                                // libei positive = down -> Glide positive = up
                                let dy_120 =
                                    (-sc.discrete_dy).clamp(i16::MIN as i32, i16::MAX as i32)
                                        as i16;
                                if (dx_120 != 0 || dy_120 != 0)
                                    && ev_tx
                                        .send(CaptureEvent::MouseScroll { dx_120, dy_120 })
                                        .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        EiEvent::ScrollDelta(sc) => {
                            if shared_eis.in_remote.load(Ordering::Relaxed) {
                                // Smooth pixel scroll (~15 px per 120 detent); negate Y for Glide's up-positive convention
                                scroll_acc_x += sc.dx * 8.0;
                                scroll_acc_y += -sc.dy * 8.0;
                                let dx_120 = scroll_acc_x.trunc() as i16;
                                let dy_120 = scroll_acc_y.trunc() as i16;
                                scroll_acc_x -= dx_120 as f32;
                                scroll_acc_y -= dy_120 as f32;
                                if (dx_120 != 0 || dy_120 != 0)
                                    && ev_tx
                                        .send(CaptureEvent::MouseScroll { dx_120, dy_120 })
                                        .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        EiEvent::KeyboardKey(k) => {
                            let pressed = k.state == ei::keyboard::KeyState::Press;
                            let Some(usage) = HidUsage::from_evdev(k.key as u16) else {
                                continue;
                            };
                            if pressed {
                                held_keys.insert(usage);
                            } else {
                                held_keys.remove(&usage);
                            }

                            if pressed && usage == config.escape_key {
                                shared_eis.in_remote.store(false, Ordering::SeqCst);
                                media_tracker.clear();
                                was_in_remote = false;
                                if ev_tx.send(CaptureEvent::EscapeHotkey).is_err() {
                                    return;
                                }
                                continue;
                            }

                            if config.enable_hotkeys && pressed {
                                let ctrl = held_keys.contains(&HidUsage::CONTROL_LEFT)
                                    || held_keys.contains(&HidUsage::CONTROL_RIGHT);
                                let lalt = held_keys.contains(&HidUsage::ALT_LEFT);
                                if ctrl && lalt {
                                    let hotkey = match usage {
                                        u if u == HidUsage::keyboard(0x50) => {
                                            Some(CaptureEvent::HotkeySwitch {
                                                direction: Edge::Left,
                                            })
                                        }
                                        u if u == HidUsage::keyboard(0x4F) => {
                                            Some(CaptureEvent::HotkeySwitch {
                                                direction: Edge::Right,
                                            })
                                        }
                                        u if u == HidUsage::keyboard(0x52) => {
                                            Some(CaptureEvent::HotkeySwitch {
                                                direction: Edge::Top,
                                            })
                                        }
                                        u if u == HidUsage::keyboard(0x51) => {
                                            Some(CaptureEvent::HotkeySwitch {
                                                direction: Edge::Bottom,
                                            })
                                        }
                                        u if u == HidUsage::keyboard(0x0F) => {
                                            Some(CaptureEvent::HotkeyToggleLock)
                                        }
                                        _ => None,
                                    };
                                    if let Some(ev) = hotkey {
                                        if ev_tx.send(ev).is_err() {
                                            return;
                                        }
                                        continue;
                                    }
                                }
                            }

                            let in_remote = shared_eis.in_remote.load(Ordering::Acquire);
                            if !in_remote {
                                media_tracker.clear();
                            } else if media_tracker.should_forward(
                                usage,
                                pressed,
                                shared_eis.pin_media_keys_local.load(Ordering::Relaxed),
                            ) && ev_tx
                                .send(CaptureEvent::Key { usage, pressed })
                                .is_err()
                            {
                                return;
                            }
                        }
                        EiEvent::Disconnected(_) => {
                            info!("EIS capture server disconnected");
                            return;
                        }
                        _ => {}
                    }
                }

                // Wait for next readable EIS packet
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut pfd, 1, 250) };
                if rc < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if rc > 0 {
                    match ei_ctx.read() {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                        Err(e) => {
                            warn!(error = %e, "EIS capture read error");
                            break;
                        }
                    }
                }
            }
        })?;

    info!("Wayland InputCapture server backend ready (EIS)");
    Ok(CaptureHandle {
        control: Box::new(WaylandCaptureControl { shared, cmd_tx }),
        events: ev_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barriers_respect_dead_corners_on_all_edges() {
        let geom = DesktopGeometry::new(vec![MonitorRect {
            name: "DP-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }]);

        let barriers = build_edge_barriers(
            &geom,
            &[Edge::Left, Edge::Right, Edge::Top, Edge::Bottom],
            24,
        );
        assert_eq!(barriers.len(), 4);

        assert_eq!(barriers[0].edge, Edge::Left);
        assert_eq!(
            (barriers[0].x1, barriers[0].y1, barriers[0].x2, barriers[0].y2),
            (0, 24, 0, 1056)
        );

        assert_eq!(barriers[1].edge, Edge::Right);
        assert_eq!(
            (barriers[1].x1, barriers[1].y1, barriers[1].x2, barriers[1].y2),
            (1920, 24, 1920, 1056)
        );

        assert_eq!(barriers[2].edge, Edge::Top);
        assert_eq!(
            (barriers[2].x1, barriers[2].y1, barriers[2].x2, barriers[2].y2),
            (24, 0, 1896, 0)
        );

        assert_eq!(barriers[3].edge, Edge::Bottom);
        assert_eq!(
            (barriers[3].x1, barriers[3].y1, barriers[3].x2, barriers[3].y2),
            (24, 1080, 1896, 1080)
        );
    }

    #[test]
    fn barrier_hit_normalization_maps_edges_cleanly() {
        let geom = DesktopGeometry::new(vec![MonitorRect {
            name: "DP-1".into(),
            x: 0,
            y: 0,
            width: 2000,
            height: 1000,
            scale: 1.0,
            primary: true,
        }]);
        assert!((normalize_barrier_hit(&geom, Edge::Right, 2000.0, 500.0) - 0.5).abs() < 1e-4);
        assert!((normalize_barrier_hit(&geom, Edge::Top, 1000.0, 0.0) - 0.5).abs() < 1e-4);
        assert!((normalize_barrier_hit(&geom, Edge::Left, 0.0, 0.0) - 0.0).abs() < 1e-4);
        assert!((normalize_barrier_hit(&geom, Edge::Bottom, 2000.0, 1000.0) - 1.0).abs() < 1e-4);
    }
}
