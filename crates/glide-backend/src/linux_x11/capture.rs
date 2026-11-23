//! X11 Server input capture using XInput2 raw events (`XI_Raw*`), pointer/keyboard grab,
//! XRandR multi-monitor edge detection, dead-corner guards, edge dwell, and hotkeys.

use crate::geometry::DesktopGeometry;
use crate::hotkeys::{AtomicHotkeySuppressor, AtomicHotkeys, HotkeyAction, HotkeyMatch};
use crate::linux_x11::{
    query_desktop_geometry, subscribe_randr_events, unsubscribe_randr_events, X11CursorHider,
    X11FocusClearer,
};
use crate::{
    edge_bit, AtomicEdgeMask, AtomicNormPoint, CaptureConfig, CaptureControl, CaptureEvent,
    CaptureHandle, CaptureHotkeys, RemoteMediaKeyTracker, ALL_EDGES,
};
use anyhow::{Context, Result};
use glide_hid::{HidUsage, ModifierMask, MouseButton};
use glide_proto::{Edge, LockState};
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as XInputExt, EventMask, Fp3232, XIEventMask};
use x11rb::protocol::xproto::{
    Allow, AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as XProtoExt,
    CreateWindowAux, Cursor, EventMask as CoreEventMask, GetPropertyReply, GrabMode, GrabStatus,
    Window, WindowClass, CLIENT_MESSAGE_EVENT,
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
    active_edges: AtomicEdgeMask,
    geometry: RwLock<DesktopGeometry>,
    remote_mode: AtomicBool,
    locked: AtomicBool,
    drag_drop_files: AtomicBool,
    auto_lock_fullscreen: AtomicBool,
    pin_media_keys_local: AtomicBool,
    unfocus_on_leave: AtomicBool,
    hotkeys: AtomicHotkeys,
    edge_threshold_px: AtomicI32,
    dead_corner_px: AtomicI32,
    edge_dwell_ms: AtomicU64,
    pending_leave: Mutex<Option<LeaveTarget>>,
    last_local_norm: AtomicNormPoint,
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
        self.shared.active_edges.set(edges);
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

    fn set_drag_drop_files(&self, enabled: bool) {
        self.shared.drag_drop_files.store(enabled, Ordering::Relaxed);
    }

    fn set_pin_media_keys_local(&self, enabled: bool) {
        self.shared
            .pin_media_keys_local
            .store(enabled, Ordering::Relaxed);
    }

    fn set_unfocus_on_leave(&self, enabled: bool) {
        self.shared
            .unfocus_on_leave
            .store(enabled, Ordering::Relaxed);
    }

    fn set_hotkeys(&self, hotkeys: CaptureHotkeys) {
        self.shared.hotkeys.store(hotkeys);
    }

    fn set_hotkeys_suspended(&self, suspended: bool) {
        self.shared.hotkeys.set_suspended(suspended);
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
        self.shared.geometry.read().clone()
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
        self.shared.last_local_norm.load()
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

const FULLSCREEN_CACHE_TTL: Duration = Duration::from_millis(75);

/// Caches `_NET_WM_STATE_FULLSCREEN` queries for 75 ms so edge-panning or edge-dwelling
/// does not flood the X11 server with thousands of synchronous round-trips per second.
#[derive(Debug, Default)]
struct FullscreenCache {
    cached: Option<(Instant, bool)>,
}

impl FullscreenCache {
    fn check_with<F: FnOnce() -> bool>(&mut self, now: Instant, query: F) -> bool {
        if let Some((at, value)) = self.cached {
            if now.saturating_duration_since(at) < FULLSCREEN_CACHE_TTL {
                return value;
            }
        }
        let value = query();
        self.cached = Some((now, value));
        value
    }
}

/// Extracts a 32-bit window ID from an X11 `WINDOW` property reply via `value32()`,
/// ignoring `None` (`0`) and `root`.
fn parse_window_id_from_reply(reply: &GetPropertyReply, root: Window) -> Option<Window> {
    let win = reply.value32()?.next()?;
    if win == x11rb::NONE || win == root {
        None
    } else {
        Some(win)
    }
}

/// Returns `true` if a 32-bit X11 `ATOM[]` property reply contains `needle` via `value32()`.
fn reply_contains_atom(reply: &GetPropertyReply, needle: u32) -> bool {
    needle != x11rb::NONE && reply.value32().is_some_and(|mut it| it.any(|a| a == needle))
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
    let Some(active_win) = parse_window_id_from_reply(&active_reply, root) else {
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
                if reply_contains_atom(&type_reply, atoms.net_wm_window_type_desktop) {
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
    reply_contains_atom(&state_reply, atoms.net_wm_state_fullscreen)
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
    let ctrl_aux =
        CreateWindowAux::new().event_mask(CoreEventMask::KEY_PRESS | CoreEventMask::KEY_RELEASE);
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
        &ctrl_aux,
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
    if subscribe_randr_events(&conn, root) {
        debug!("Subscribed to XRandR screen/output/CRTC change events");
    } else {
        warn!("XRandR event subscription unavailable; using periodic geometry fallback");
    }
    conn.flush()?;

    let shared = Arc::new(SharedState {
        active_edges: AtomicEdgeMask::empty(),
        geometry: RwLock::new(geometry),
        remote_mode: AtomicBool::new(false),
        locked: AtomicBool::new(false),
        drag_drop_files: AtomicBool::new(config.drag_drop_files),
        auto_lock_fullscreen: AtomicBool::new(config.auto_lock_fullscreen),
        pin_media_keys_local: AtomicBool::new(config.pin_media_keys_local),
        unfocus_on_leave: AtomicBool::new(config.unfocus_on_leave),
        hotkeys: AtomicHotkeys::new(config.hotkeys),
        edge_threshold_px: AtomicI32::new(config.edge_threshold_px.max(1)),
        dead_corner_px: AtomicI32::new(config.dead_corner_px.max(0)),
        edge_dwell_ms: AtomicU64::new(config.edge_dwell_ms),
        pending_leave: Mutex::new(None),
        last_local_norm: AtomicNormPoint::new(0.5, 0.5),
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

/// Returns `true` if a grabbed X11 `KeyPress` should be replayed to the local desktop so passive
/// root `XGrabKey` bindings (`gnome-settings-daemon`, `kglobalaccel`, etc.) handle locally-pinned
/// media and brightness keys instead of having them swallowed by our grab.
fn should_replay_x11_key_press(
    keycode: u8,
    remote_mode: bool,
    pin_media_keys_local: bool,
    media_tracker: &RemoteMediaKeyTracker,
) -> bool {
    if !remote_mode || !pin_media_keys_local {
        return false;
    }
    match HidUsage::from_x11_keycode(keycode) {
        Some(usage) => usage.is_media_key() && !media_tracker.is_forwarded(usage),
        None => false,
    }
}

/// Acquires an active synchronous keyboard grab on `ctrl_win` and immediately arms
/// `Allow::SYNC_KEYBOARD` so each subsequent `KeyPress`/`KeyRelease` is delivered to `ctrl_win`
/// (freezing on delivery so we can inspect and either `SYNC_KEYBOARD` or `REPLAY_KEYBOARD`).
fn grab_keyboard_sync(conn: &RustConnection, ctrl_win: Window, kbd_grabbed: &mut bool) {
    if *kbd_grabbed {
        return;
    }
    let ok = conn
        .grab_keyboard(
            false,
            ctrl_win,
            x11rb::CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::SYNC,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|r| r.status == GrabStatus::SUCCESS);
    if ok {
        let _ = conn.allow_events(Allow::SYNC_KEYBOARD, x11rb::CURRENT_TIME);
        let _ = conn.flush();
        *kbd_grabbed = true;
    }
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
    shared: Arc<SharedState>,
    tx: mpsc::UnboundedSender<CaptureEvent>,
) {
    let mut grabbed = false;
    let mut kbd_grabbed = false;
    let mut cursor_hider = X11CursorHider::init(&conn);
    let mut focus_clearer = X11FocusClearer::new();
    // This thread is the sole writer of `shared.geometry`, so keeping a local copy
    // avoids cloning DesktopGeometry on every mouse event.
    let mut local_geom = shared.geometry.read().clone();
    let mut edge_push_edge: Option<Edge> = None;
    let mut edge_push_accum: f32 = 0.0;
    let mut edge_dwell_start: Option<Instant> = None;
    let mut last_geom_refresh = Instant::now();
    let mut last_local_query = Instant::now();
    let mut last_ptr: Option<(i32, i32)> = None;
    let mut raw_travel: f32 = 0.0;
    let mut mods = ModifierMask::EMPTY;
    let hotkey_suppressor = AtomicHotkeySuppressor::new();
    let mut lmb_down = false;
    let mut lmb_started_on_edge = false;
    let mut drag_strip_armed = false;
    let mut media_tracker = RemoteMediaKeyTracker::new();
    let mut fullscreen_cache = FullscreenCache::default();

    let do_grab = |conn: &RustConnection,
                   geom: &DesktopGeometry,
                   grabbed: &mut bool,
                   kbd_grabbed: &mut bool,
                   cursor_hider: &mut X11CursorHider,
                   focus_clearer: &mut X11FocusClearer| {
        if !*grabbed {
            let pointer_mask = CoreEventMask::BUTTON_PRESS
                | CoreEventMask::BUTTON_RELEASE
                | CoreEventMask::POINTER_MOTION;
            // Retry briefly if an X11 XDND source (e.g. Nautilus) is still releasing its pointer
            // grab after our synthetic Escape cancellation.
            for attempt in 0..6 {
                let status = conn
                    .grab_pointer(
                        false,
                        ctrl_win,
                        pointer_mask,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                        x11rb::NONE,
                        blank_cursor,
                        x11rb::CURRENT_TIME,
                    )
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| r.status);
                if status == Some(GrabStatus::SUCCESS) || attempt == 5 {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let (cx, cy) = geom.primary_center();
            let _ = conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, cx as i16, cy as i16);
            cursor_hider.hide(conn, root);
            // Clear focus BEFORE grab_keyboard_sync so the active window receives a genuine
            // FocusOut(NotifyNormal) before keyboard grab freezes focus delivery.
            if shared.unfocus_on_leave.load(Ordering::Relaxed) {
                focus_clearer.clear_focus(conn, root);
            }
            *grabbed = true;
        }
        grab_keyboard_sync(conn, ctrl_win, kbd_grabbed);
        let _ = conn.flush();
        debug!("X11Capture grabbed pointer & sync keyboard (remote mode)");
    };

    let do_ungrab = |conn: &RustConnection,
                     geom: &DesktopGeometry,
                     grabbed: &mut bool,
                     kbd_grabbed: &mut bool,
                     media_tracker: &mut RemoteMediaKeyTracker,
                     cursor_hider: &mut X11CursorHider,
                     target: Option<LeaveTarget>| {
        if *kbd_grabbed {
            // Always thaw any frozen synchronous keyboard grab before ungrabbing!
            let _ = conn.allow_events(Allow::ASYNC_KEYBOARD, x11rb::CURRENT_TIME);
            let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
            *kbd_grabbed = false;
        }
        if *grabbed {
            let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
            *grabbed = false;
        }
        media_tracker.clear();
        if let Some(t) = target {
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

    let refresh_geometry = |conn: &RustConnection,
                            shared: &SharedState,
                            local_geom: &mut DesktopGeometry,
                            grabbed: bool,
                            edge_push_edge: &mut Option<Edge>,
                            edge_push_accum: &mut f32,
                            edge_dwell_start: &mut Option<Instant>,
                            last_geom_refresh: &mut Instant,
                            tx: &mpsc::UnboundedSender<CaptureEvent>| {
        *last_geom_refresh = Instant::now();
        let Ok(new_geom) = query_desktop_geometry(conn, screen_num) else {
            return;
        };
        let changed = {
            let mut geom = shared.geometry.write();
            if *geom != new_geom {
                *geom = new_geom.clone();
                true
            } else {
                false
            }
        };
        if changed {
            *local_geom = new_geom.clone();
            *edge_push_edge = None;
            *edge_push_accum = 0.0;
            *edge_dwell_start = None;
            if grabbed {
                let (cx, cy) = new_geom.primary_center();
                let _ = conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, cx as i16, cy as i16);
                let _ = conn.flush();
            }
            debug!(
                monitors = new_geom.monitors.len(),
                "X11Capture refreshed desktop geometry"
            );
            let _ = tx.send(CaptureEvent::GeometryChanged(new_geom));
        }
    };

    loop {
        let ev = match conn.wait_for_event() {
            Ok(ev) => ev,
            Err(e) => {
                warn!(error = %e, "X11 capture connection closed");
                break;
            }
        };

        // Defer periodic XRandR roundtrips while actively forwarding remote input
        // so 1000Hz remote mouse motion never stutters on a 5s RandR query.
        if !shared.remote_mode.load(Ordering::Acquire)
            && last_geom_refresh.elapsed() >= Duration::from_secs(5)
        {
            refresh_geometry(
                &conn,
                &shared,
                &mut local_geom,
                grabbed,
                &mut edge_push_edge,
                &mut edge_push_accum,
                &mut edge_dwell_start,
                &mut last_geom_refresh,
                &tx,
            );
        }

        match ev {
            Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_) => {
                refresh_geometry(
                    &conn,
                    &shared,
                    &mut local_geom,
                    grabbed,
                    &mut edge_push_edge,
                    &mut edge_push_accum,
                    &mut edge_dwell_start,
                    &mut last_geom_refresh,
                    &tx,
                );
            }
            Event::ClientMessage(cm) if cm.type_ == wake_atom => {
                let cmd = cm.data.as_data32()[0];
                match cmd {
                    CMD_ENTER_REMOTE => {
                        last_ptr = None;
                        raw_travel = 0.0;
                        do_grab(
                            &conn,
                            &local_geom,
                            &mut grabbed,
                            &mut kbd_grabbed,
                            &mut cursor_hider,
                            &mut focus_clearer,
                        );
                    }
                    CMD_LEAVE_REMOTE => {
                        let leave = shared.pending_leave.lock().take();
                        edge_push_edge = None;
                        edge_push_accum = 0.0;
                        edge_dwell_start = None;
                        last_ptr = None;
                        raw_travel = 0.0;
                        do_ungrab(
                            &conn,
                            &local_geom,
                            &mut grabbed,
                            &mut kbd_grabbed,
                            &mut media_tracker,
                            &mut cursor_hider,
                            leave,
                        );
                    }
                    _ => {}
                }
            }
            Event::KeyPress(kp) => {
                if !kbd_grabbed {
                    continue;
                }
                if should_replay_x11_key_press(
                    kp.detail,
                    shared.remote_mode.load(Ordering::Acquire),
                    shared.pin_media_keys_local.load(Ordering::Relaxed),
                    &media_tracker,
                ) {
                    let _ = conn.allow_events(Allow::REPLAY_KEYBOARD, kp.time);
                    let _ = conn.flush();
                    kbd_grabbed = false;
                    grab_keyboard_sync(&conn, ctrl_win, &mut kbd_grabbed);
                } else {
                    let _ = conn.allow_events(Allow::SYNC_KEYBOARD, kp.time);
                    let _ = conn.flush();
                }
            }
            Event::KeyRelease(kr) => {
                if kbd_grabbed {
                    let _ = conn.allow_events(Allow::SYNC_KEYBOARD, kr.time);
                    let _ = conn.flush();
                }
            }
            Event::XinputRawMotion(rm) => {
                let (dx, dy) = extract_raw_xy(&rm.valuator_mask, &rm.axisvalues_raw);
                if dx == 0.0 && dy == 0.0 {
                    continue;
                }
                if shared.remote_mode.load(Ordering::Acquire) {
                    if !grabbed {
                        do_grab(
                            &conn,
                            &local_geom,
                            &mut grabbed,
                            &mut kbd_grabbed,
                            &mut cursor_hider,
                            &mut focus_clearer,
                        );
                    }
                    let _ = tx.send(CaptureEvent::MouseDelta { dx, dy });
                } else {
                    if shared.locked.load(Ordering::Relaxed) {
                        continue;
                    }
                    let edge_mask = shared.active_edges.load();
                    if edge_mask == 0 {
                        if drag_strip_armed {
                            drag_strip_armed = false;
                            let _ = tx.send(CaptureEvent::DragStripArmed(false));
                        }
                        continue;
                    }

                    raw_travel += dx.abs().max(dy.abs());
                    if edge_push_edge.is_none() {
                        if let Some((px, py)) = last_ptr {
                            let mut min_dist = i32::MAX;
                            for edge in ALL_EDGES {
                                if (edge_mask & edge_bit(edge)) != 0 {
                                    min_dist = min_dist.min(local_geom.distance_to_edge(edge, px, py));
                                }
                            }
                            let pace_ms = if min_dist > 160 && raw_travel < (min_dist as f32) * 0.25
                            {
                                16
                            } else {
                                2
                            };
                            if last_local_query.elapsed() < Duration::from_millis(pace_ms) {
                                continue;
                            }
                        }
                    }
                    last_local_query = Instant::now();

                    let Ok(cookie) = conn.query_pointer(root) else {
                        continue;
                    };
                    let Ok(ptr) = cookie.reply() else {
                        continue;
                    };
                    let px = ptr.root_x as i32;
                    let py = ptr.root_y as i32;
                    last_ptr = Some((px, py));
                    raw_travel = 0.0;

                    let (nx, ny) = local_geom.normalize_point(px, py);
                    shared.last_local_norm.store(nx, ny);

                    let dnd_enabled = shared.drag_drop_files.load(Ordering::Relaxed);
                    if lmb_down && dnd_enabled && !lmb_started_on_edge {
                        let mut min_dist = i32::MAX;
                        for edge in ALL_EDGES {
                            if (edge_mask & edge_bit(edge)) != 0 {
                                min_dist = min_dist.min(local_geom.distance_to_edge(edge, px, py));
                            }
                        }
                        if min_dist <= 48 && !drag_strip_armed {
                            drag_strip_armed = true;
                            let _ = tx.send(CaptureEvent::DragStripArmed(true));
                        } else if min_dist > 80 && drag_strip_armed {
                            drag_strip_armed = false;
                            let _ = tx.send(CaptureEvent::DragStripArmed(false));
                        }
                    } else if drag_strip_armed {
                        drag_strip_armed = false;
                        let _ = tx.send(CaptureEvent::DragStripArmed(false));
                    }

                    let threshold = shared.edge_threshold_px.load(Ordering::Relaxed).max(1) as f32;
                    let dead_corner = shared.dead_corner_px.load(Ordering::Relaxed).max(0);
                    let base_dwell_ms = shared.edge_dwell_ms.load(Ordering::Relaxed);
                    let dwell_ms = if lmb_down && dnd_enabled && !lmb_started_on_edge {
                        base_dwell_ms.max(25)
                    } else {
                        base_dwell_ms
                    };

                    let mut crossed = None;
                    for edge in ALL_EDGES {
                        if (edge_mask & edge_bit(edge)) == 0 {
                            continue;
                        }
                        let outward = match edge {
                            Edge::Left => -dx,
                            Edge::Right => dx,
                            Edge::Top => -dy,
                            Edge::Bottom => dy,
                        };
                        if outward > 0.0 {
                            if let Some(norm) =
                                local_geom.hit_outer_edge_guarded(edge, px, py, 2, dead_corner)
                            {
                                // Only query EWMH _NET_WM_STATE_FULLSCREEN when the cursor actually
                                // touches an active outer edge, cached for 75 ms to avoid spamming
                                // X11 round-trips while panning or dwelling along a border.
                                if shared.auto_lock_fullscreen.load(Ordering::Relaxed)
                                    && fullscreen_cache.check_with(Instant::now(), || {
                                        is_active_window_fullscreen(&conn, root, ewmh_atoms)
                                    })
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
                        last_ptr = None;
                        raw_travel = 0.0;
                        drag_strip_armed = false;
                        shared.remote_mode.store(true, Ordering::SeqCst);
                        // Note: defer do_grab until CMD_ENTER_REMOTE when LMB is down so server.rs
                        // can cancel an X11 XDND source grab cleanly before we grab the pointer!
                        if !(lmb_down && dnd_enabled) {
                            do_grab(
                                &conn,
                                &local_geom,
                                &mut grabbed,
                                &mut kbd_grabbed,
                                &mut cursor_hider,
                                &mut focus_clearer,
                            );
                        }
                        let _ = tx.send(CaptureEvent::EdgeCrossed {
                            edge,
                            normalized_pos,
                        });
                    } else if edge_push_edge.is_some() {
                        let still_touching = edge_push_edge.is_some_and(|e| {
                            local_geom
                                .hit_outer_edge_guarded(e, px, py, 3, dead_corner)
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
                let btn = rb.detail as u8;
                if !shared.remote_mode.load(Ordering::Acquire) {
                    if btn == 1 {
                        lmb_down = true;
                        let edge_mask = shared.active_edges.load();
                        lmb_started_on_edge = edge_mask != 0
                            && conn
                                .query_pointer(root)
                                .ok()
                                .and_then(|c| c.reply().ok())
                                .is_some_and(|ptr| {
                                    ALL_EDGES.iter().any(|&e| {
                                        (edge_mask & edge_bit(e)) != 0
                                            && local_geom.distance_to_edge(
                                                e,
                                                ptr.root_x as i32,
                                                ptr.root_y as i32,
                                            ) <= 4
                                    })
                                });
                        let _ = tx.send(CaptureEvent::LocalLeftButton { pressed: true });
                    }
                    continue;
                }
                if btn == 1 {
                    lmb_down = true;
                }
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
                let btn = rb.detail as u8;
                if !shared.remote_mode.load(Ordering::Acquire) {
                    if btn == 1 {
                        lmb_down = false;
                        lmb_started_on_edge = false;
                        if drag_strip_armed {
                            drag_strip_armed = false;
                            let _ = tx.send(CaptureEvent::DragStripArmed(false));
                        }
                        let _ = tx.send(CaptureEvent::LocalLeftButton { pressed: false });
                    }
                    continue;
                }
                if btn == 1 {
                    lmb_down = false;
                }
                if let Some(mb) = MouseButton::from_x11_button(btn) {
                    let _ = tx.send(CaptureEvent::MouseButton {
                        button: mb,
                        pressed: false,
                    });
                }
            }
            Event::XinputRawKeyPress(rk) => {
                let kc = rk.detail as u8;
                let remote = shared.remote_mode.load(Ordering::Acquire);
                let Some(usage) = HidUsage::from_x11_keycode(kc) else {
                    if remote && !kbd_grabbed {
                        grab_keyboard_sync(&conn, ctrl_win, &mut kbd_grabbed);
                    }
                    trace!(kc, "Unmapped X11 keycode on press");
                    continue;
                };
                let prev_mask = mods;
                mods.update(usage, true);

                match hotkey_suppressor.evaluate(
                    shared.hotkeys.load(),
                    remote,
                    prev_mask,
                    usage,
                    true,
                ) {
                    HotkeyMatch::Trigger(HotkeyAction::Escape) => {
                        shared.remote_mode.store(false, Ordering::SeqCst);
                        last_ptr = None;
                        raw_travel = 0.0;
                        do_ungrab(
                            &conn,
                            &local_geom,
                            &mut grabbed,
                            &mut kbd_grabbed,
                            &mut media_tracker,
                            &mut cursor_hider,
                            None,
                        );
                        let _ = tx.send(CaptureEvent::EscapeHotkey);
                        continue;
                    }
                    HotkeyMatch::Trigger(HotkeyAction::Switch(direction)) => {
                        if !remote {
                            if let Ok(cookie) = conn.query_pointer(root) {
                                if let Ok(ptr) = cookie.reply() {
                                    let (nx, ny) = local_geom
                                        .normalize_point(ptr.root_x as i32, ptr.root_y as i32);
                                    shared.last_local_norm.store(nx, ny);
                                }
                            }
                        }
                        let _ = tx.send(CaptureEvent::HotkeySwitch { direction });
                        continue;
                    }
                    HotkeyMatch::Trigger(HotkeyAction::ToggleLock) => {
                        let _ = tx.send(CaptureEvent::HotkeyToggleLock);
                        continue;
                    }
                    HotkeyMatch::SuppressOnly => {
                        continue;
                    }
                    HotkeyMatch::PassThrough => {}
                }

                if remote {
                    let pin = shared.pin_media_keys_local.load(Ordering::Relaxed);
                    if media_tracker.should_forward(usage, true, pin) {
                        if !kbd_grabbed {
                            grab_keyboard_sync(&conn, ctrl_win, &mut kbd_grabbed);
                        }
                        let _ = tx.send(CaptureEvent::Key {
                            usage,
                            pressed: true,
                        });
                    }
                }
            }
            Event::XinputRawKeyRelease(rk) => {
                let kc = rk.detail as u8;
                let remote = shared.remote_mode.load(Ordering::Acquire);
                if remote && !kbd_grabbed {
                    // Re-acquire our synchronous keyboard grab after a replayed media key's
                    // passive WM grab automatically releases on KeyRelease.
                    grab_keyboard_sync(&conn, ctrl_win, &mut kbd_grabbed);
                }
                let Some(usage) = HidUsage::from_x11_keycode(kc) else {
                    continue;
                };
                let prev_mask = mods;
                mods.update(usage, false);
                if matches!(
                    hotkey_suppressor.evaluate(
                        shared.hotkeys.load(),
                        remote,
                        prev_mask,
                        usage,
                        false,
                    ),
                    HotkeyMatch::SuppressOnly | HotkeyMatch::Trigger(_)
                ) {
                    continue;
                }
                if !remote {
                    continue;
                }
                if !media_tracker.should_forward(
                    usage,
                    false,
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

    if kbd_grabbed {
        let _ = conn.allow_events(Allow::ASYNC_KEYBOARD, x11rb::CURRENT_TIME);
        let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
    }
    if grabbed {
        let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
    }
    cursor_hider.show(&conn, root);
    focus_clearer.destroy(&conn);
    let _ = xinput::xi_select_events(&*conn, root, &[]);
    unsubscribe_randr_events(&conn, root);
    let _ = conn.flush();
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

    fn make_reply(format: u8, value: Vec<u8>) -> GetPropertyReply {
        let value_len = match format {
            8 => value.len() as u32,
            16 => (value.len() / 2) as u32,
            32 => (value.len() / 4) as u32,
            _ => 0,
        };
        GetPropertyReply {
            format,
            sequence: 0,
            length: 0,
            type_: 0,
            bytes_after: 0,
            value_len,
            value,
        }
    }

    #[test]
    fn parses_net_wm_state_atoms_and_detects_fullscreen() {
        const ATOM_ABOVE: u32 = 301;
        const ATOM_FULLSCREEN: u32 = 302;
        const ATOM_FOCUSED: u32 = 303;

        let reply_fs = make_reply(32, encode_u32s(&[ATOM_ABOVE, ATOM_FULLSCREEN, ATOM_FOCUSED]));
        assert!(reply_contains_atom(&reply_fs, ATOM_FULLSCREEN));
        assert!(!reply_contains_atom(&reply_fs, x11rb::NONE));

        let reply_win = make_reply(32, encode_u32s(&[ATOM_ABOVE, ATOM_FOCUSED]));
        assert!(!reply_contains_atom(&reply_win, ATOM_FULLSCREEN));

        // Non-32-bit format or empty/missing EWMH atom gracefully returns false
        let reply_8bit = make_reply(8, encode_u32s(&[ATOM_FULLSCREEN]));
        assert!(!reply_contains_atom(&reply_8bit, ATOM_FULLSCREEN));
        let reply_empty = make_reply(0, Vec::new());
        assert!(!reply_contains_atom(&reply_empty, ATOM_FULLSCREEN));
    }

    #[test]
    fn active_window_parser_ignores_none_root_and_invalid_properties() {
        const ROOT: Window = 0x100;
        const APP_WIN: Window = 0x0240_0007;

        assert_eq!(
            parse_window_id_from_reply(&make_reply(32, encode_u32s(&[APP_WIN])), ROOT),
            Some(APP_WIN)
        );
        // Ignore None (0) and root window
        assert_eq!(
            parse_window_id_from_reply(&make_reply(32, encode_u32s(&[x11rb::NONE])), ROOT),
            None
        );
        assert_eq!(
            parse_window_id_from_reply(&make_reply(32, encode_u32s(&[ROOT])), ROOT),
            None
        );
        // Ignore malformed / missing properties
        assert_eq!(parse_window_id_from_reply(&make_reply(0, Vec::new()), ROOT), None);
        assert_eq!(parse_window_id_from_reply(&make_reply(32, vec![1, 2]), ROOT), None);
        assert_eq!(
            parse_window_id_from_reply(&make_reply(16, encode_u32s(&[APP_WIN])), ROOT),
            None
        );
    }

    #[test]
    fn desktop_window_type_is_detected_and_excluded() {
        const ATOM_NORMAL: u32 = 401;
        const ATOM_DESKTOP: u32 = 402;

        let desktop_reply = make_reply(32, encode_u32s(&[ATOM_NORMAL, ATOM_DESKTOP]));
        let normal_reply = make_reply(32, encode_u32s(&[ATOM_NORMAL]));
        assert!(reply_contains_atom(&desktop_reply, ATOM_DESKTOP));
        assert!(!reply_contains_atom(&normal_reply, ATOM_DESKTOP));
        assert!(!reply_contains_atom(&desktop_reply, x11rb::NONE));
    }

    #[test]
    fn fullscreen_cache_reuses_result_within_ttl_and_refreshes_after() {
        let mut cache = FullscreenCache::default();
        let t0 = Instant::now();
        let mut calls = 0usize;

        assert!(cache.check_with(t0, || {
            calls += 1;
            true
        }));
        assert_eq!(calls, 1);

        // Within 75 ms TTL, cached result is returned without invoking the X11 query
        assert!(cache.check_with(t0 + Duration::from_millis(50), || {
            calls += 1;
            false
        }));
        assert_eq!(calls, 1);

        // At/after 75 ms TTL, query runs again and updates cached value
        assert!(!cache.check_with(t0 + FULLSCREEN_CACHE_TTL, || {
            calls += 1;
            false
        }));
        assert_eq!(calls, 2);
    }

    #[test]
    fn x11_sync_replay_respects_pin_setting_brightness_and_forwarded_tracker() {
        let mut tracker = RemoteMediaKeyTracker::new();
        let vol_up_kc = HidUsage::from_name("AudioVolumeUp")
            .unwrap()
            .to_x11_keycode()
            .unwrap();
        let bright_up_kc = HidUsage::from_name("BrightnessUp")
            .unwrap()
            .to_x11_keycode()
            .unwrap();
        let key_a_kc = HidUsage::from_name("KeyA")
            .unwrap()
            .to_x11_keycode()
            .unwrap();

        // In remote mode with pin=true, media & brightness keys replay to passive WM grabs
        assert!(should_replay_x11_key_press(vol_up_kc, true, true, &tracker));
        assert!(should_replay_x11_key_press(
            bright_up_kc,
            true,
            true,
            &tracker
        ));
        // Standard keyboard keys never replay
        assert!(!should_replay_x11_key_press(key_a_kc, true, true, &tracker));
        // When pin=false or not in remote mode, nothing replays
        assert!(!should_replay_x11_key_press(
            vol_up_kc, false, true, &tracker
        ));
        assert!(!should_replay_x11_key_press(
            vol_up_kc, true, false, &tracker
        ));

        // If a media key was ALREADY forwarded while pin=false, toggling pin=true mid-hold
        // must NOT replay its repeat presses locally until released!
        let vol_up = HidUsage::from_x11_keycode(vol_up_kc).unwrap();
        assert!(tracker.should_forward(vol_up, true, false));
        assert!(!should_replay_x11_key_press(vol_up_kc, true, true, &tracker));
        // Once released, future presses replay locally again
        assert!(tracker.should_forward(vol_up, false, true));
        assert!(should_replay_x11_key_press(vol_up_kc, true, true, &tracker));
    }
}
