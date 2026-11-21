//! X11 Server input capture using XInput2 raw events (`XI_Raw*`), pointer/keyboard grab,
//! XRandR multi-monitor edge detection, dead-corner guards, edge dwell, and hotkeys.

use crate::geometry::DesktopGeometry;
use crate::linux_x11::{query_desktop_geometry, X11CursorHider};
use crate::{CaptureConfig, CaptureControl, CaptureEvent, CaptureHandle};
use anyhow::{Context, Result};
use glide_hid::{HidUsage, ModifierMask, MouseButton};
use glide_proto::{Edge, LockState};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as XInputExt, EventMask, Fp3232, XIEventMask};
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as XProtoExt, CreateWindowAux,
    Cursor, EventMask as CoreEventMask, GrabMode, Window, WindowClass, CLIENT_MESSAGE_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const CMD_ENTER_REMOTE: u32 = 1;
const CMD_LEAVE_REMOTE: u32 = 2;

#[derive(Debug, Clone, Copy, Default)]
struct EwmhFullscreenAtoms {
    net_active_window: u32,
    net_wm_state: u32,
    net_wm_state_fullscreen: u32,
    net_wm_window_type: u32,
    net_wm_window_type_desktop: u32,
}

enum LeaveTarget {
    Edge(Edge, f32),
    Point(f32, f32),
}

struct SharedState {
    active_edges: Mutex<Vec<Edge>>,
    geometry: Mutex<DesktopGeometry>,
    remote_mode: AtomicBool,
    locked: AtomicBool,
    auto_lock_fullscreen: AtomicBool,
    pin_media_keys_local: AtomicBool,
    edge_threshold_px: AtomicI32,
    dead_corner_px: AtomicI32,
    edge_dwell_ms: AtomicU64,
    pending_leave: Mutex<Option<LeaveTarget>>,
    last_local_norm: Mutex<(f32, f32)>,
}

struct X11CaptureControl {
    conn: Arc<RustConnection>,
    ctrl_win: Window,
    wake_atom: u32,
    shared: Arc<SharedState>,
}

impl X11CaptureControl {
    fn send_cmd(&self, cmd: u32) -> Result<()> {
        let ev = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: self.ctrl_win,
            type_: self.wake_atom,
            data: ClientMessageData::from([cmd, 0, 0, 0, 0]),
        };
        self.conn
            .send_event(false, self.ctrl_win, CoreEventMask::NO_EVENT, ev)
            .context("failed to send X11 wake ClientMessage")?;
        self.conn.flush().context("failed to flush X11 wake")?;
        Ok(())
    }
}

impl CaptureControl for X11CaptureControl {
    fn set_active_edges(&self, edges: &[Edge]) {
        *self.shared.active_edges.lock() = edges.to_vec();
    }

    fn update_tuning(
        &self,
        edge_threshold_px: i32,
        dead_corner_px: i32,
        edge_dwell_ms: u64,
        auto_lock_fullscreen: bool,
        locked: bool,
    ) {
        self.shared
            .edge_threshold_px
            .store(edge_threshold_px.max(1), Ordering::Relaxed);
        self.shared
            .dead_corner_px
            .store(dead_corner_px.max(0), Ordering::Relaxed);
        self.shared
            .edge_dwell_ms
            .store(edge_dwell_ms, Ordering::Relaxed);
        self.shared
            .auto_lock_fullscreen
            .store(auto_lock_fullscreen, Ordering::Relaxed);
        self.shared.locked.store(locked, Ordering::Relaxed);
    }

    fn set_pin_media_keys_local(&self, enabled: bool) {
        self.shared
            .pin_media_keys_local
            .store(enabled, Ordering::Relaxed);
    }

    fn enter_remote(&self) -> Result<()> {
        self.shared.remote_mode.store(true, Ordering::SeqCst);
        self.send_cmd(CMD_ENTER_REMOTE)
    }

    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()> {
        *self.shared.pending_leave.lock() = Some(LeaveTarget::Edge(return_edge, normalized_pos));
        self.shared.remote_mode.store(false, Ordering::SeqCst);
        self.send_cmd(CMD_LEAVE_REMOTE)
    }

    fn leave_remote_at(&self, norm_x: f32, norm_y: f32) -> Result<()> {
        *self.shared.pending_leave.lock() = Some(LeaveTarget::Point(norm_x, norm_y));
        self.shared.remote_mode.store(false, Ordering::SeqCst);
        self.send_cmd(CMD_LEAVE_REMOTE)
    }

    fn desktop_geometry(&self) -> DesktopGeometry {
        self.shared.geometry.lock().clone()
    }

    fn lock_state(&self) -> LockState {
        if let Ok(cookie) = self.conn.get_keyboard_control() {
            if let Ok(reply) = cookie.reply() {
                return LockState {
                    caps_lock: (reply.led_mask & 1) != 0,
                    num_lock: (reply.led_mask & 2) != 0,
                };
            }
        }
        LockState::default()
    }

    fn last_local_cursor_norm(&self) -> (f32, f32) {
        *self.shared.last_local_norm.lock()
    }
}

fn fp3232_to_f32(v: Fp3232) -> f32 {
    v.integral as f32 + (v.frac as f32 / 4_294_967_296.0)
}

fn extract_raw_xy(mask: &[u32], values: &[Fp3232]) -> (f32, f32) {
    let mut dx = 0.0f32;
    let mut dy = 0.0f32;
    if mask.is_empty() {
        return (0.0, 0.0);
    }
    let m0 = mask[0];
    let mut idx = 0usize;
    if (m0 & 1) != 0 {
        if let Some(&v) = values.get(idx) {
            dx = fp3232_to_f32(v);
        }
        idx += 1;
    }
    if (m0 & 2) != 0 {
        if let Some(&v) = values.get(idx) {
            dy = fp3232_to_f32(v);
        }
    }
    (dx, dy)
}

fn create_blank_cursor(conn: &RustConnection, root: Window) -> Result<Cursor> {
    let pixmap = conn.generate_id()?;
    conn.create_pixmap(1, pixmap, root, 1, 1)?;
    let cursor = conn.generate_id()?;
    conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0)?;
    conn.free_pixmap(pixmap)?;
    Ok(cursor)
}

fn arrow_to_edge(usage: HidUsage) -> Option<Edge> {
    match usage.raw() {
        x if x == HidUsage::keyboard(0x50).raw() => Some(Edge::Left),
        x if x == HidUsage::keyboard(0x4F).raw() => Some(Edge::Right),
        x if x == HidUsage::keyboard(0x52).raw() => Some(Edge::Top),
        x if x == HidUsage::keyboard(0x51).raw() => Some(Edge::Bottom),
        _ => None,
    }
}

fn intern_ewmh_fullscreen_atoms(conn: &RustConnection) -> EwmhFullscreenAtoms {
    let intern = |name: &[u8]| -> u32 {
        conn.intern_atom(false, name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or(0, |r| r.atom)
    };
    EwmhFullscreenAtoms {
        net_active_window: intern(b"_NET_ACTIVE_WINDOW"),
        net_wm_state: intern(b"_NET_WM_STATE"),
        net_wm_state_fullscreen: intern(b"_NET_WM_STATE_FULLSCREEN"),
        net_wm_window_type: intern(b"_NET_WM_WINDOW_TYPE"),
        net_wm_window_type_desktop: intern(b"_NET_WM_WINDOW_TYPE_DESKTOP"),
    }
}

/// Extracts a 32-bit window ID from an X11 `WINDOW` property reply, ignoring `None` (`0`) and `root`.
fn parse_window_id_from_property(format: u8, raw: &[u8], root: Window) -> Option<Window> {
    if format != 32 || raw.len() < 4 {
        return None;
    }
    let win = u32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
    if win == x11rb::NONE || win == root {
        None
    } else {
        Some(win)
    }
}

/// Parses a 32-bit X11 `ATOM[]` property buffer into a list of atom IDs.
fn parse_atoms_from_property(format: u8, raw: &[u8]) -> Vec<u32> {
    if format != 32 {
        return Vec::new();
    }
    raw.chunks_exact(4)
        .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Returns `true` if a `_NET_WM_STATE` atom slice advertises `_NET_WM_STATE_FULLSCREEN`.
fn wm_state_has_fullscreen(atoms: &[u32], fullscreen_atom: u32) -> bool {
    fullscreen_atom != x11rb::NONE && atoms.contains(&fullscreen_atom)
}

/// Returns `true` if a `_NET_WM_WINDOW_TYPE` atom slice marks the window as a desktop shell window.
fn wm_window_type_is_desktop(types: &[u32], desktop_atom: u32) -> bool {
    desktop_atom != x11rb::NONE && types.contains(&desktop_atom)
}

/// Checks whether the currently active X11 window advertises `_NET_WM_STATE_FULLSCREEN`.
///
/// Gracefully returns `false` on window managers that do not support EWMH or when the active
/// window is the root/desktop window.
fn is_active_window_fullscreen(
    conn: &RustConnection,
    root: Window,
    atoms: EwmhFullscreenAtoms,
) -> bool {
    if atoms.net_active_window == x11rb::NONE
        || atoms.net_wm_state == x11rb::NONE
        || atoms.net_wm_state_fullscreen == x11rb::NONE
    {
        return false;
    }

    let Ok(active_cookie) =
        conn.get_property(false, root, atoms.net_active_window, AtomEnum::WINDOW, 0, 1)
    else {
        return false;
    };
    let Ok(active_reply) = active_cookie.reply() else {
        return false;
    };
    let Some(active_win) =
        parse_window_id_from_property(active_reply.format, &active_reply.value, root)
    else {
        return false;
    };

    if atoms.net_wm_window_type != x11rb::NONE && atoms.net_wm_window_type_desktop != x11rb::NONE {
        if let Ok(type_cookie) = conn.get_property(
            false,
            active_win,
            atoms.net_wm_window_type,
            AtomEnum::ATOM,
            0,
            16,
        ) {
            if let Ok(type_reply) = type_cookie.reply() {
                let win_types = parse_atoms_from_property(type_reply.format, &type_reply.value);
                if wm_window_type_is_desktop(&win_types, atoms.net_wm_window_type_desktop) {
                    return false;
                }
            }
        }
    }

    let Ok(state_cookie) =
        conn.get_property(false, active_win, atoms.net_wm_state, AtomEnum::ATOM, 0, 32)
    else {
        return false;
    };
    let Ok(state_reply) = state_cookie.reply() else {
        return false;
    };
    let state_atoms = parse_atoms_from_property(state_reply.format, &state_reply.value);
    wm_state_has_fullscreen(&state_atoms, atoms.net_wm_state_fullscreen)
}

/// Starts the X11 server input capture loop on a dedicated OS thread.
pub fn start_x11_capture(config: CaptureConfig) -> Result<CaptureHandle> {
    let (conn, screen_num) =
        RustConnection::connect(None).context("failed to connect to X11 display")?;
    let conn = Arc::new(conn);
    let root = conn.setup().roots[screen_num].root;

    let xi_ver = conn
        .xinput_xi_query_version(2, 2)
        .context("failed to query XInput2")?
        .reply()
        .context("XInput2 extension unavailable")?;
    debug!(
        major = xi_ver.major_version,
        minor = xi_ver.minor_version,
        "Initialized X11Capture with XInput2"
    );

    let geometry = query_desktop_geometry(&conn, screen_num)?;

    let ctrl_win = conn.generate_id()?;
    conn.create_window(
        0,
        ctrl_win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?;
    conn.map_window(ctrl_win)?;

    let wake_atom = conn
        .intern_atom(false, b"GLIDE_CAPTURE_WAKE")?
        .reply()?
        .atom;
    let ewmh_atoms = intern_ewmh_fullscreen_atoms(&conn);
    let blank_cursor = create_blank_cursor(&conn, root)?;

    let raw_mask = XIEventMask::RAW_MOTION
        | XIEventMask::RAW_KEY_PRESS
        | XIEventMask::RAW_KEY_RELEASE
        | XIEventMask::RAW_BUTTON_PRESS
        | XIEventMask::RAW_BUTTON_RELEASE;
    let event_mask = EventMask {
        deviceid: 1, // XIAllMasterDevices
        mask: vec![raw_mask],
    };
    conn.xinput_xi_select_events(root, &[event_mask])?
        .check()
        .context("XInput2 xi_select_events failed")?;
    conn.flush()?;

    let shared = Arc::new(SharedState {
        active_edges: Mutex::new(Vec::new()),
        geometry: Mutex::new(geometry),
        remote_mode: AtomicBool::new(false),
        locked: AtomicBool::new(false),
        auto_lock_fullscreen: AtomicBool::new(config.auto_lock_fullscreen),
        pin_media_keys_local: AtomicBool::new(config.pin_media_keys_local),
        edge_threshold_px: AtomicI32::new(config.edge_threshold_px.max(1)),
        dead_corner_px: AtomicI32::new(config.dead_corner_px.max(0)),
        edge_dwell_ms: AtomicU64::new(config.edge_dwell_ms),
        pending_leave: Mutex::new(None),
        last_local_norm: Mutex::new((0.5, 0.5)),
    });

    let (tx, rx) = mpsc::unbounded_channel();

    let thread_conn = Arc::clone(&conn);
    let thread_shared = Arc::clone(&shared);
    thread::Builder::new()
        .name("glide-x11-capture".into())
        .spawn(move || {
            run_capture_loop(
                thread_conn,
                screen_num,
                root,
                ctrl_win,
                blank_cursor,
                wake_atom,
                ewmh_atoms,
                config,
                thread_shared,
                tx,
            );
        })
        .context("failed to spawn glide-x11-capture thread")?;

    let control = X11CaptureControl {
        conn,
        ctrl_win,
        wake_atom,
        shared,
    };

    Ok(CaptureHandle {
        control: Box::new(control),
        events: rx,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_capture_loop(
    conn: Arc<RustConnection>,
    screen_num: usize,
    root: Window,
    ctrl_win: Window,
    blank_cursor: Cursor,
    wake_atom: u32,
    ewmh_atoms: EwmhFullscreenAtoms,
    config: CaptureConfig,
    shared: Arc<SharedState>,
    tx: mpsc::UnboundedSender<CaptureEvent>,
) {
    let mut grabbed = false;
    let mut cursor_hider = X11CursorHider::init(&conn);
    let mut edge_push_edge: Option<Edge> = None;
    let mut edge_push_accum: f32 = 0.0;
    let mut edge_dwell_start: Option<Instant> = None;
    let mut last_geom_refresh = Instant::now();
    let mut last_local_query = Instant::now();
    let mut mods = ModifierMask::EMPTY;

    let do_grab = |conn: &RustConnection,
                   shared: &SharedState,
                   grabbed: &mut bool,
                   cursor_hider: &mut X11CursorHider| {
        if *grabbed {
            return;
        }
        let pointer_mask = CoreEventMask::BUTTON_PRESS
            | CoreEventMask::BUTTON_RELEASE
            | CoreEventMask::POINTER_MOTION;
        let _ = conn.grab_pointer(
            false,
            ctrl_win,
            pointer_mask,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            blank_cursor,
            x11rb::CURRENT_TIME,
        );
        let _ = conn.grab_keyboard(
            false,
            ctrl_win,
            x11rb::CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        );
        let (cx, cy) = shared.geometry.lock().primary_center();
        let _ = conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, cx as i16, cy as i16);
        cursor_hider.hide(conn, root);
        let _ = conn.flush();
        *grabbed = true;
        debug!("X11Capture grabbed pointer & keyboard (remote mode)");
    };

    let do_ungrab = |conn: &RustConnection,
                     shared: &SharedState,
                     grabbed: &mut bool,
                     cursor_hider: &mut X11CursorHider,
                     target: Option<LeaveTarget>| {
        if *grabbed {
            let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
            let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
            *grabbed = false;
        }
        if let Some(t) = target {
            let geom = shared.geometry.lock().clone();
            let (x, y) = match t {
                LeaveTarget::Edge(edge, norm) => geom.point_on_edge(edge, norm, 6),
                LeaveTarget::Point(nx, ny) => geom.denormalize_point(nx, ny),
            };
            let _ = conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, x as i16, y as i16);
        }
        cursor_hider.show(conn, root);
        let _ = conn.flush();
        debug!("X11Capture ungrabbed pointer & keyboard (local mode)");
    };

    loop {
        let ev = match conn.wait_for_event() {
            Ok(ev) => ev,
            Err(e) => {
                warn!(error = %e, "X11 capture connection closed");
                break;
            }
        };

        if last_geom_refresh.elapsed() >= Duration::from_secs(5) {
            if let Ok(geom) = query_desktop_geometry(&conn, screen_num) {
                *shared.geometry.lock() = geom;
            }
            last_geom_refresh = Instant::now();
        }

        match ev {
            Event::ClientMessage(cm) if cm.type_ == wake_atom => {
                let cmd = cm.data.as_data32()[0];
                match cmd {
                    CMD_ENTER_REMOTE => do_grab(&conn, &shared, &mut grabbed, &mut cursor_hider),
                    CMD_LEAVE_REMOTE => {
                        let leave = shared.pending_leave.lock().take();
                        edge_push_edge = None;
                        edge_push_accum = 0.0;
                        edge_dwell_start = None;
                        do_ungrab(&conn, &shared, &mut grabbed, &mut cursor_hider, leave);
                    }
                    _ => {}
                }
            }
            Event::XinputRawMotion(rm) => {
                let (dx, dy) = extract_raw_xy(&rm.valuator_mask, &rm.axisvalues_raw);
                if dx == 0.0 && dy == 0.0 {
                    continue;
                }
                if shared.remote_mode.load(Ordering::Acquire) {
                    if !grabbed {
                        do_grab(&conn, &shared, &mut grabbed, &mut cursor_hider);
                    }
                    let _ = tx.send(CaptureEvent::MouseDelta { dx, dy });
                } else {
                    if shared.locked.load(Ordering::Relaxed) {
                        continue;
                    }
                    let edges = shared.active_edges.lock().clone();
                    if edges.is_empty() {
                        continue;
                    }
                    if last_local_query.elapsed() < Duration::from_millis(2)
                        && edge_push_edge.is_none()
                    {
                        continue;
                    }
                    last_local_query = Instant::now();

                    let Ok(cookie) = conn.query_pointer(root) else {
                        continue;
                    };
                    let Ok(ptr) = cookie.reply() else {
                        continue;
                    };
                    let geom = shared.geometry.lock().clone();
                    *shared.last_local_norm.lock() =
                        geom.normalize_point(ptr.root_x as i32, ptr.root_y as i32);

                    let threshold = shared.edge_threshold_px.load(Ordering::Relaxed).max(1) as f32;
                    let dead_corner = shared.dead_corner_px.load(Ordering::Relaxed).max(0);
                    let dwell_ms = shared.edge_dwell_ms.load(Ordering::Relaxed);

                    let mut crossed = None;
                    for &edge in &edges {
                        let outward = match edge {
                            Edge::Left => -dx,
                            Edge::Right => dx,
                            Edge::Top => -dy,
                            Edge::Bottom => dy,
                        };
                        if outward > 0.0 {
                            if let Some(norm) = geom.hit_outer_edge_guarded(
                                edge,
                                ptr.root_x as i32,
                                ptr.root_y as i32,
                                2,
                                dead_corner,
                            ) {
                                // Only query EWMH _NET_WM_STATE_FULLSCREEN when the cursor actually
                                // touches an active outer edge (zero 1000Hz mid-screen overhead).
                                if shared.auto_lock_fullscreen.load(Ordering::Relaxed)
                                    && is_active_window_fullscreen(&conn, root, ewmh_atoms)
                                {
                                    edge_push_edge = None;
                                    edge_push_accum = 0.0;
                                    edge_dwell_start = None;
                                    break;
                                }
                                if edge_push_edge == Some(edge) {
                                    edge_push_accum += outward;
                                } else {
                                    edge_push_edge = Some(edge);
                                    edge_push_accum = outward;
                                    edge_dwell_start = Some(Instant::now());
                                }
                                let dwell_ok = dwell_ms == 0
                                    || edge_dwell_start.is_some_and(|t| {
                                        t.elapsed() >= Duration::from_millis(dwell_ms)
                                    });
                                if edge_push_accum >= threshold && dwell_ok {
                                    crossed = Some((edge, norm));
                                }
                                break;
                            }
                        }
                    }

                    if let Some((edge, normalized_pos)) = crossed {
                        edge_push_edge = None;
                        edge_push_accum = 0.0;
                        edge_dwell_start = None;
                        shared.remote_mode.store(true, Ordering::SeqCst);
                        do_grab(&conn, &shared, &mut grabbed, &mut cursor_hider);
                        let _ = tx.send(CaptureEvent::EdgeCrossed {
                            edge,
                            normalized_pos,
                        });
                    } else if edge_push_edge.is_some() {
                        let still_touching = edge_push_edge.is_some_and(|e| {
                            geom.hit_outer_edge_guarded(
                                e,
                                ptr.root_x as i32,
                                ptr.root_y as i32,
                                3,
                                dead_corner,
                            )
                            .is_some()
                        });
                        if !still_touching {
                            edge_push_edge = None;
                            edge_push_accum = 0.0;
                            edge_dwell_start = None;
                        }
                    }
                }
            }
            Event::XinputRawButtonPress(rb) => {
                if !shared.remote_mode.load(Ordering::Acquire) {
                    continue;
                }
                let btn = rb.detail as u8;
                match btn {
                    4 => {
                        let _ = tx.send(CaptureEvent::MouseScroll {
                            dx_120: 0,
                            dy_120: 120,
                        });
                    }
                    5 => {
                        let _ = tx.send(CaptureEvent::MouseScroll {
                            dx_120: 0,
                            dy_120: -120,
                        });
                    }
                    6 => {
                        let _ = tx.send(CaptureEvent::MouseScroll {
                            dx_120: -120,
                            dy_120: 0,
                        });
                    }
                    7 => {
                        let _ = tx.send(CaptureEvent::MouseScroll {
                            dx_120: 120,
                            dy_120: 0,
                        });
                    }
                    other => {
                        if let Some(mb) = MouseButton::from_x11_button(other) {
                            let _ = tx.send(CaptureEvent::MouseButton {
                                button: mb,
                                pressed: true,
                            });
                        }
                    }
                }
            }
            Event::XinputRawButtonRelease(rb) => {
                if !shared.remote_mode.load(Ordering::Acquire) {
                    continue;
                }
                if let Some(mb) = MouseButton::from_x11_button(rb.detail as u8) {
                    let _ = tx.send(CaptureEvent::MouseButton {
                        button: mb,
                        pressed: false,
                    });
                }
            }
            Event::XinputRawKeyPress(rk) => {
                let kc = rk.detail as u8;
                let Some(usage) = HidUsage::from_x11_keycode(kc) else {
                    trace!(kc, "Unmapped X11 keycode on press");
                    continue;
                };
                mods.update(usage, true);

                if usage == config.escape_key && shared.remote_mode.load(Ordering::Acquire) {
                    shared.remote_mode.store(false, Ordering::SeqCst);
                    do_ungrab(&conn, &shared, &mut grabbed, &mut cursor_hider, None);
                    let _ = tx.send(CaptureEvent::EscapeHotkey);
                    continue;
                }

                // Direct keyboard screen-switch (Ctrl+Alt+Arrow) & Lock toggle (Ctrl+Alt+L)
                if config.enable_hotkeys && mods.ctrl() && mods.contains(ModifierMask::ALT_LEFT) {
                    if let Some(dir) = arrow_to_edge(usage) {
                        let _ = tx.send(CaptureEvent::HotkeySwitch { direction: dir });
                        continue;
                    }
                    if usage == HidUsage::keyboard(0x0F) {
                        // KeyL
                        let _ = tx.send(CaptureEvent::HotkeyToggleLock);
                        continue;
                    }
                }

                if shared.remote_mode.load(Ordering::Acquire)
                    && crate::should_forward_key_in_remote(
                        usage,
                        shared.pin_media_keys_local.load(Ordering::Relaxed),
                    )
                {
                    let _ = tx.send(CaptureEvent::Key {
                        usage,
                        pressed: true,
                    });
                }
            }
            Event::XinputRawKeyRelease(rk) => {
                let kc = rk.detail as u8;
                let Some(usage) = HidUsage::from_x11_keycode(kc) else {
                    continue;
                };
                mods.update(usage, false);
                if !shared.remote_mode.load(Ordering::Acquire) {
                    continue;
                }
                if usage == config.escape_key {
                    continue;
                }
                if !crate::should_forward_key_in_remote(
                    usage,
                    shared.pin_media_keys_local.load(Ordering::Relaxed),
                ) {
                    continue;
                }
                let _ = tx.send(CaptureEvent::Key {
                    usage,
                    pressed: false,
                });
            }
            _ => {}
        }
    }

    if grabbed {
        let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
    }
    cursor_hider.show(&conn, root);
    let _ = conn.flush();
    let _ = xinput::xi_select_events(&*conn, root, &[]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_u32s(vals: &[u32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(vals.len() * 4);
        for v in vals {
            out.extend_from_slice(&v.to_ne_bytes());
        }
        out
    }

    #[test]
    fn parses_net_wm_state_atoms_and_detects_fullscreen() {
        const ATOM_ABOVE: u32 = 301;
        const ATOM_FULLSCREEN: u32 = 302;
        const ATOM_FOCUSED: u32 = 303;

        let raw_fs = encode_u32s(&[ATOM_ABOVE, ATOM_FULLSCREEN, ATOM_FOCUSED]);
        let atoms_fs = parse_atoms_from_property(32, &raw_fs);
        assert_eq!(atoms_fs, vec![ATOM_ABOVE, ATOM_FULLSCREEN, ATOM_FOCUSED]);
        assert!(wm_state_has_fullscreen(&atoms_fs, ATOM_FULLSCREEN));

        let raw_windowed = encode_u32s(&[ATOM_ABOVE, ATOM_FOCUSED]);
        let atoms_win = parse_atoms_from_property(32, &raw_windowed);
        assert!(!wm_state_has_fullscreen(&atoms_win, ATOM_FULLSCREEN));

        // Non-32-bit format or empty/missing EWMH atom gracefully returns false
        assert!(parse_atoms_from_property(8, &raw_fs).is_empty());
        assert!(parse_atoms_from_property(0, &[]).is_empty());
        assert!(!wm_state_has_fullscreen(&atoms_fs, x11rb::NONE));
    }

    #[test]
    fn active_window_parser_ignores_none_root_and_invalid_properties() {
        const ROOT: Window = 0x100;
        const APP_WIN: Window = 0x0240_0007;

        assert_eq!(
            parse_window_id_from_property(32, &encode_u32s(&[APP_WIN]), ROOT),
            Some(APP_WIN)
        );
        // Ignore None (0) and root window
        assert_eq!(
            parse_window_id_from_property(32, &encode_u32s(&[x11rb::NONE]), ROOT),
            None
        );
        assert_eq!(
            parse_window_id_from_property(32, &encode_u32s(&[ROOT]), ROOT),
            None
        );
        // Ignore malformed / missing properties
        assert_eq!(parse_window_id_from_property(0, &[], ROOT), None);
        assert_eq!(parse_window_id_from_property(32, &[1, 2], ROOT), None);
        assert_eq!(
            parse_window_id_from_property(16, &encode_u32s(&[APP_WIN]), ROOT),
            None
        );
    }

    #[test]
    fn desktop_window_type_is_detected_and_excluded() {
        const ATOM_NORMAL: u32 = 401;
        const ATOM_DESKTOP: u32 = 402;

        assert!(wm_window_type_is_desktop(
            &[ATOM_NORMAL, ATOM_DESKTOP],
            ATOM_DESKTOP
        ));
        assert!(!wm_window_type_is_desktop(&[ATOM_NORMAL], ATOM_DESKTOP));
        assert!(!wm_window_type_is_desktop(&[ATOM_DESKTOP], x11rb::NONE));
    }
}
