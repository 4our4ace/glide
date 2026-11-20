//! X11 Server input capture using XInput2 raw events (`XI_Raw*`), pointer/keyboard grab,
//! and XRandR multi-monitor edge detection.

use crate::geometry::DesktopGeometry;
use crate::linux_x11::query_desktop_geometry;
use crate::{CaptureConfig, CaptureControl, CaptureEvent, CaptureHandle};
use anyhow::{Context, Result};
use glide_hid::{HidUsage, MouseButton};
use glide_proto::Edge;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as XInputExt, EventMask, Fp3232, XIEventMask};
use x11rb::protocol::xproto::{
    ClientMessageData, ClientMessageEvent, ConnectionExt as XProtoExt, CreateWindowAux, Cursor,
    EventMask as CoreEventMask, GrabMode, Window, WindowClass, CLIENT_MESSAGE_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const CMD_ENTER_REMOTE: u32 = 1;
const CMD_LEAVE_REMOTE: u32 = 2;

struct SharedState {
    active_edges: Mutex<Vec<Edge>>,
    geometry: Mutex<DesktopGeometry>,
    remote_mode: AtomicBool,
    pending_leave: Mutex<Option<(Edge, f32)>>,
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

    fn enter_remote(&self) -> Result<()> {
        self.shared.remote_mode.store(true, Ordering::SeqCst);
        self.send_cmd(CMD_ENTER_REMOTE)
    }

    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()> {
        *self.shared.pending_leave.lock() = Some((return_edge, normalized_pos));
        self.shared.remote_mode.store(false, Ordering::SeqCst);
        self.send_cmd(CMD_LEAVE_REMOTE)
    }

    fn desktop_geometry(&self) -> DesktopGeometry {
        self.shared.geometry.lock().clone()
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

    // Create a tiny InputOnly window for grabs and instant ClientMessage wakeups.
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
    let blank_cursor = create_blank_cursor(&conn, root)?;

    // Select raw input events on all master devices (XIAllMasterDevices = 1).
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
        pending_leave: Mutex::new(None),
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
    config: CaptureConfig,
    shared: Arc<SharedState>,
    tx: mpsc::UnboundedSender<CaptureEvent>,
) {
    let mut grabbed = false;
    let mut edge_push_edge: Option<Edge> = None;
    let mut edge_push_accum: f32 = 0.0;
    let mut last_geom_refresh = Instant::now();
    let mut last_local_query = Instant::now();

    let do_grab = |conn: &RustConnection, shared: &SharedState, grabbed: &mut bool| {
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
        // Park cursor at center of primary monitor so compositor hot-corners never trigger.
        let (cx, cy) = shared.geometry.lock().primary_center();
        let _ = conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, cx as i16, cy as i16);
        let _ = conn.flush();
        *grabbed = true;
        debug!("X11Capture grabbed pointer & keyboard (remote mode)");
    };

    let do_ungrab = |conn: &RustConnection,
                     shared: &SharedState,
                     grabbed: &mut bool,
                     return_pos: Option<(Edge, f32)>| {
        if *grabbed {
            let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
            let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
            *grabbed = false;
        }
        if let Some((edge, norm)) = return_pos {
            let (x, y) = shared.geometry.lock().point_on_edge(edge, norm, 6);
            let _ = conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, x as i16, y as i16);
        }
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
                    CMD_ENTER_REMOTE => do_grab(&conn, &shared, &mut grabbed),
                    CMD_LEAVE_REMOTE => {
                        let leave = shared.pending_leave.lock().take();
                        edge_push_edge = None;
                        edge_push_accum = 0.0;
                        do_ungrab(&conn, &shared, &mut grabbed, leave);
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
                        do_grab(&conn, &shared, &mut grabbed);
                    }
                    let _ = tx.send(CaptureEvent::MouseDelta { dx, dy });
                } else {
                    // Local mode: check active screen edges for crossover.
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

                    let mut crossed = None;
                    for &edge in &edges {
                        let outward = match edge {
                            Edge::Left => -dx,
                            Edge::Right => dx,
                            Edge::Top => -dy,
                            Edge::Bottom => dy,
                        };
                        if outward > 0.0 {
                            if let Some(norm) =
                                geom.hit_outer_edge(edge, ptr.root_x as i32, ptr.root_y as i32, 2)
                            {
                                if edge_push_edge == Some(edge) {
                                    edge_push_accum += outward;
                                } else {
                                    edge_push_edge = Some(edge);
                                    edge_push_accum = outward;
                                }
                                if edge_push_accum >= config.edge_threshold_px.max(1) as f32 {
                                    crossed = Some((edge, norm));
                                }
                                break;
                            }
                        }
                    }

                    if let Some((edge, normalized_pos)) = crossed {
                        edge_push_edge = None;
                        edge_push_accum = 0.0;
                        shared.remote_mode.store(true, Ordering::SeqCst);
                        do_grab(&conn, &shared, &mut grabbed);
                        let _ = tx.send(CaptureEvent::EdgeCrossed {
                            edge,
                            normalized_pos,
                        });
                    } else if edge_push_edge.is_some() {
                        // Reset accumulator if pointer moved away from the edge.
                        let still_touching = edge_push_edge.is_some_and(|e| {
                            geom.hit_outer_edge(e, ptr.root_x as i32, ptr.root_y as i32, 3)
                                .is_some()
                        });
                        if !still_touching {
                            edge_push_edge = None;
                            edge_push_accum = 0.0;
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
                if usage == config.escape_key && shared.remote_mode.load(Ordering::Acquire) {
                    shared.remote_mode.store(false, Ordering::SeqCst);
                    do_ungrab(&conn, &shared, &mut grabbed, None);
                    let _ = tx.send(CaptureEvent::EscapeHotkey);
                    continue;
                }
                if shared.remote_mode.load(Ordering::Acquire) {
                    let _ = tx.send(CaptureEvent::Key {
                        usage,
                        pressed: true,
                    });
                }
            }
            Event::XinputRawKeyRelease(rk) => {
                if !shared.remote_mode.load(Ordering::Acquire) {
                    continue;
                }
                let kc = rk.detail as u8;
                if let Some(usage) = HidUsage::from_x11_keycode(kc) {
                    if usage == config.escape_key {
                        continue;
                    }
                    let _ = tx.send(CaptureEvent::Key {
                        usage,
                        pressed: false,
                    });
                }
            }
            _ => {}
        }
    }

    if grabbed {
        let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = conn.flush();
    }
    let _ = xinput::xi_select_events(&*conn, root, &[]);
}
