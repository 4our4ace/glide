//! Pure-Rust Linux X11 cross-edge file drag-and-drop backend powered by `x11rb`.
//!
//! - **Drag-out (X11 -> Peer):** Maps invisible 1px `InputOnly` `override_redirect`
//!   `XdndAware` (v5) edge-strip windows along active outer edges ONLY while a local
//!   left-button drag is armed. Accepts `XdndEnter`/`XdndPosition`, reads `text/uri-list`
//!   (`file://` paths) via `XdndSelection`, and cancels the local X11 drag cleanly on handoff.
//! - **Drag-in (Peer -> X11):** After files are saved into `~/Downloads/Glide/`, locates
//!   the topmost viewable `XdndAware` window under the cursor (resolving `XdndProxy`) and
//!   performs a real **XDND v5** source handshake (`XdndEnter` -> `XdndPosition` ->
//!   `XdndStatus` -> `XdndDrop` + serving `text/uri-list` on `SelectionRequest` ->
//!   `XdndFinished`). If the target window is not `XdndAware` or rejects the drop, falls
//!   back gracefully by publishing `text/uri-list` + `x-special/gnome-copied-files` on
//!   X11 `CLIPBOARD` so the user can immediately `Ctrl+V`.

use crate::dnd::{
    clamp_hint_popup_pos, compute_edge_strip_rects, format_drag_extension_badge,
    format_drag_preview_label, format_gnome_copied_files, format_uri_list, parse_uri_list,
    DndBackend, DropDeliveryOutcome, EdgeStripRect, StagedEdgeTracker,
};
use crate::geometry::DesktopGeometry;
use crate::linux_x11::query_desktop_geometry;
use anyhow::{anyhow, Context, Result};
use glide_hid::HidUsage;
use glide_proto::Edge;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, trace};
use x11rb::connection::Connection;
use x11rb::protocol::shape::SK;
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeGCAux, ClientMessageData, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt as XProtoExt, CreateGCAux, CreateWindowAux, EventMask, MapState, PropMode,
    Rectangle, SelectionNotifyEvent, SelectionRequestEvent, StackMode, Window, WindowClass,
    CLIENT_MESSAGE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperExt;

const XDND_VERSION: u32 = 5;
const STAGED_MAX_AGE: Duration = Duration::from_secs(4);
const STAGED_LEAVE_GRACE: Duration = Duration::from_millis(350);

struct XdndAtoms {
    xdnd_aware: Atom,
    xdnd_selection: Atom,
    xdnd_enter: Atom,
    xdnd_position: Atom,
    xdnd_status: Atom,
    xdnd_leave: Atom,
    xdnd_drop: Atom,
    xdnd_finished: Atom,
    xdnd_action_copy: Atom,
    xdnd_type_list: Atom,
    xdnd_proxy: Atom,
    text_uri_list: Atom,
    gnome_copied_files: Atom,
    utf8_string: Atom,
    targets: Atom,
    clipboard: Atom,
    wm_state: Atom,
    prop_data: Atom,
    prop_time: Atom,
    wake: Atom,
}

impl XdndAtoms {
    fn intern(conn: &RustConnection) -> Result<Self> {
        let intern =
            |name: &[u8]| -> Result<Atom> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        Ok(Self {
            xdnd_aware: intern(b"XdndAware")?,
            xdnd_selection: intern(b"XdndSelection")?,
            xdnd_enter: intern(b"XdndEnter")?,
            xdnd_position: intern(b"XdndPosition")?,
            xdnd_status: intern(b"XdndStatus")?,
            xdnd_leave: intern(b"XdndLeave")?,
            xdnd_drop: intern(b"XdndDrop")?,
            xdnd_finished: intern(b"XdndFinished")?,
            xdnd_action_copy: intern(b"XdndActionCopy")?,
            xdnd_type_list: intern(b"XdndTypeList")?,
            xdnd_proxy: intern(b"XdndProxy")?,
            text_uri_list: intern(b"text/uri-list")?,
            gnome_copied_files: intern(b"x-special/gnome-copied-files")?,
            utf8_string: intern(b"UTF8_STRING")?,
            targets: intern(b"TARGETS")?,
            clipboard: intern(b"CLIPBOARD")?,
            wm_state: intern(b"WM_STATE")?,
            prop_data: intern(b"GLIDE_XDND_DATA")?,
            prop_time: intern(b"GLIDE_XDND_TIME")?,
            wake: intern(b"GLIDE_XDND_WAKE")?,
        })
    }
}

struct StagedDrag {
    edge: Edge,
    paths: Vec<PathBuf>,
    staged_at: Instant,
    left_at: Option<Instant>,
}

enum DndCmd {
    SyncStrips,
    HideStrips,
    CancelLocalDrag,
    IncomingDragStart {
        label: String,
        badge: String,
        x: i32,
        y: i32,
    },
    IncomingDragMove {
        x: i32,
        y: i32,
    },
    IncomingDragFilesReady {
        paths: Vec<PathBuf>,
    },
    IncomingDragCancel,
    DeliverDrop {
        paths: Vec<PathBuf>,
        reply: std::sync::mpsc::Sender<Result<DropDeliveryOutcome>>,
    },
    Shutdown,
}

struct SharedX11Dnd {
    enabled: AtomicBool,
    armed: AtomicBool,
    dead_corner_px: AtomicI32,
    active_edges: Mutex<Vec<Edge>>,
    staged: Mutex<Option<StagedDrag>>,
    staged_tracker: StagedEdgeTracker,
    cmds: Mutex<VecDeque<DndCmd>>,
}

pub struct X11DndBackend {
    conn: Arc<RustConnection>,
    src_win: Window,
    wake_atom: Atom,
    shared: Arc<SharedX11Dnd>,
}

impl X11DndBackend {
    fn enqueue(&self, cmd: DndCmd) {
        self.shared.cmds.lock().push_back(cmd);
        let ev = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: self.src_win,
            type_: self.wake_atom,
            data: ClientMessageData::from([1u32, 0, 0, 0, 0]),
        };
        let _ = self
            .conn
            .send_event(false, self.src_win, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }
}

impl Drop for X11DndBackend {
    fn drop(&mut self) {
        self.enqueue(DndCmd::Shutdown);
    }
}

impl DndBackend for X11DndBackend {
    fn set_enabled(&self, enabled: bool) {
        self.shared.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.shared.armed.store(false, Ordering::SeqCst);
            self.shared.staged_tracker.clear();
            *self.shared.staged.lock() = None;
        }
        self.enqueue(DndCmd::SyncStrips);
    }

    fn set_active_edges(&self, edges: &[Edge], dead_corner_px: i32) {
        *self.shared.active_edges.lock() = edges.to_vec();
        self.shared
            .dead_corner_px
            .store(dead_corner_px.max(0), Ordering::Relaxed);
        self.enqueue(DndCmd::SyncStrips);
    }

    fn set_drag_armed(&self, armed: bool) {
        let effective = armed && self.shared.enabled.load(Ordering::Acquire);
        let prev = self.shared.armed.swap(effective, Ordering::SeqCst);
        if !effective {
            self.shared.staged_tracker.clear();
            *self.shared.staged.lock() = None;
        }
        if prev != effective {
            self.enqueue(if effective {
                DndCmd::SyncStrips
            } else {
                DndCmd::HideStrips
            });
        }
    }

    fn staged_edge_mask(&self) -> u8 {
        if !self.shared.enabled.load(Ordering::Acquire) {
            return 0;
        }
        self.shared.staged_tracker.active_mask()
    }

    fn take_staged_files(&self, edge: Edge) -> Option<Vec<PathBuf>> {
        if !self.shared.enabled.load(Ordering::Acquire) {
            return None;
        }
        let mut guard = self.shared.staged.lock();
        let staged = guard.as_ref()?;
        if staged.edge != edge {
            return None;
        }
        if staged.staged_at.elapsed() > STAGED_MAX_AGE {
            *guard = None;
            self.shared.staged_tracker.clear();
            return None;
        }
        if let Some(left) = staged.left_at {
            if left.elapsed() > STAGED_LEAVE_GRACE {
                *guard = None;
                self.shared.staged_tracker.clear();
                return None;
            }
        }
        let paths = guard.take()?.paths;
        self.shared.staged_tracker.clear();
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }

    fn cancel_local_drag(&self) {
        self.shared.armed.store(false, Ordering::SeqCst);
        self.shared.staged_tracker.clear();
        *self.shared.staged.lock() = None;
        self.enqueue(DndCmd::CancelLocalDrag);
    }

    fn supports_live_os_drag(&self) -> bool {
        true
    }

    fn on_incoming_drag_start(&self, filenames: &[String], x: i32, y: i32) {
        let label = format_drag_preview_label(filenames);
        let badge = format_drag_extension_badge(filenames);
        self.enqueue(DndCmd::IncomingDragStart { label, badge, x, y });
    }

    fn on_incoming_drag_files_ready(&self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            self.enqueue(DndCmd::IncomingDragFilesReady { paths });
        }
    }

    fn on_incoming_drag_move(&self, x: i32, y: i32) {
        // Coalesce back-to-back move commands so high-polling mice never flood the queue.
        let mut q = self.shared.cmds.lock();
        if let Some(DndCmd::IncomingDragMove {
            x: last_x,
            y: last_y,
        }) = q.back_mut()
        {
            *last_x = x;
            *last_y = y;
            return;
        }
        q.push_back(DndCmd::IncomingDragMove { x, y });
        drop(q);
        let ev = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: self.src_win,
            type_: self.wake_atom,
            data: ClientMessageData::from([1u32, 0, 0, 0, 0]),
        };
        let _ = self
            .conn
            .send_event(false, self.src_win, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }

    fn on_incoming_drag_cancel(&self) {
        self.enqueue(DndCmd::IncomingDragCancel);
    }

    fn deliver_drop(&self, paths: Vec<PathBuf>) -> Result<DropDeliveryOutcome> {
        if paths.is_empty() {
            return Ok(DropDeliveryOutcome::ClipboardFallback);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.enqueue(DndCmd::DeliverDrop { paths, reply: tx });
        rx.recv_timeout(Duration::from_secs(6))
            .map_err(|_| anyhow!("timed out waiting for X11 XDND drop delivery"))?
    }
}

/// Starts the pure-Rust Linux X11 XDND v5 drag-and-drop backend.
pub fn start_x11_dnd() -> Result<Arc<dyn DndBackend>> {
    let (conn, screen_num) =
        RustConnection::connect(None).context("failed to connect to X11 display for DnD")?;
    let conn = Arc::new(conn);
    let root = conn.setup().roots[screen_num].root;
    let atoms = XdndAtoms::intern(&conn)?;

    let src_win = conn.generate_id()?;
    let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
    conn.create_window(
        0,
        src_win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &aux,
    )?;
    conn.flush()?;

    let shared = Arc::new(SharedX11Dnd {
        enabled: AtomicBool::new(true),
        armed: AtomicBool::new(false),
        dead_corner_px: AtomicI32::new(24),
        active_edges: Mutex::new(Vec::new()),
        staged: Mutex::new(None),
        staged_tracker: StagedEdgeTracker::new(STAGED_MAX_AGE, STAGED_LEAVE_GRACE),
        cmds: Mutex::new(VecDeque::new()),
    });

    let wake_atom = atoms.wake;
    let thread_conn = Arc::clone(&conn);
    let thread_shared = Arc::clone(&shared);
    thread::Builder::new()
        .name("glide-x11-dnd".into())
        .spawn(move || {
            run_x11_dnd_loop(thread_conn, screen_num, root, src_win, atoms, thread_shared);
        })
        .context("failed to spawn glide-x11-dnd thread")?;

    Ok(Arc::new(X11DndBackend {
        conn,
        src_win,
        wake_atom,
        shared,
    }))
}

// ---------------------------------------------------------------------------
// X11 DnD thread event loop (drag-out edge strips + drag-in XDND v5 source)
// ---------------------------------------------------------------------------

struct StripWin {
    win: Window,
    rect: EdgeStripRect,
    mapped: bool,
}

struct ActiveEdgeInbound {
    edge: Edge,
    strip_win: Window,
    source_win: Window,
    has_uri_list: bool,
    requested_selection: bool,
}

struct ClipboardOffer {
    uri_list: Vec<u8>,
    gnome_files: Vec<u8>,
}

fn destroy_strips(conn: &RustConnection, strips: &mut Vec<StripWin>) {
    for s in strips.drain(..) {
        let _ = conn.unmap_window(s.win);
        let _ = conn.destroy_window(s.win);
    }
    let _ = conn.flush();
}

fn unmap_strips(conn: &RustConnection, strips: &mut [StripWin]) {
    let mut changed = false;
    for s in strips.iter_mut() {
        if s.mapped {
            let _ = conn.unmap_window(s.win);
            s.mapped = false;
            changed = true;
        }
    }
    if changed {
        let _ = conn.flush();
    }
}

fn sync_x11_strips(
    conn: &RustConnection,
    screen_num: usize,
    root: Window,
    atoms: &XdndAtoms,
    shared: &SharedX11Dnd,
    strips: &mut Vec<StripWin>,
) {
    let enabled = shared.enabled.load(Ordering::Acquire);
    let armed = shared.armed.load(Ordering::Acquire);
    let edges = shared.active_edges.lock().clone();
    let dead_corner = shared.dead_corner_px.load(Ordering::Relaxed);

    if !enabled || !armed || edges.is_empty() {
        unmap_strips(conn, strips);
        return;
    }

    let Ok(geom) = query_desktop_geometry(conn, screen_num) else {
        return;
    };
    let desired = compute_edge_strip_rects(&geom, &edges, dead_corner, 4);

    let needs_rebuild = strips.len() != desired.len()
        || strips
            .iter()
            .zip(desired.iter())
            .any(|(cur, want)| cur.rect != *want);

    if needs_rebuild {
        destroy_strips(conn, strips);
        for rect in desired {
            let Ok(win) = conn.generate_id() else {
                continue;
            };
            let aux = CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::PROPERTY_CHANGE);
            if conn
                .create_window(
                    0,
                    win,
                    root,
                    rect.x as i16,
                    rect.y as i16,
                    rect.width as u16,
                    rect.height as u16,
                    0,
                    WindowClass::INPUT_ONLY,
                    0,
                    &aux,
                )
                .is_err()
            {
                continue;
            }
            let _ = conn.change_property32(
                PropMode::REPLACE,
                win,
                atoms.xdnd_aware,
                AtomEnum::ATOM,
                &[XDND_VERSION],
            );
            strips.push(StripWin {
                win,
                rect,
                mapped: false,
            });
        }
    }

    for s in strips.iter_mut() {
        if !s.mapped {
            let _ = conn.map_window(s.win);
            s.mapped = true;
        }
    }
    let _ = conn.flush();
}

fn send_xdnd_status(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    source_win: Window,
    target_win: Window,
    accept: bool,
) {
    let flags = if accept { 1u32 | 2u32 } else { 0u32 };
    let action = if accept { atoms.xdnd_action_copy } else { 0 };
    let ev = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: source_win,
        type_: atoms.xdnd_status,
        data: ClientMessageData::from([target_win, flags, 0, 0, action]),
    };
    let _ = conn.send_event(false, source_win, EventMask::NO_EVENT, ev);
    let _ = conn.flush();
}

fn send_xdnd_finished(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    source_win: Window,
    target_win: Window,
    accepted: bool,
) {
    let flags = if accepted { 1u32 } else { 0u32 };
    let action = if accepted { atoms.xdnd_action_copy } else { 0 };
    let ev = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: source_win,
        type_: atoms.xdnd_finished,
        data: ClientMessageData::from([target_win, flags, action, 0, 0]),
    };
    let _ = conn.send_event(false, source_win, EventMask::NO_EVENT, ev);
    let _ = conn.flush();
}

fn offered_types_include_uri_list(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    source_win: Window,
    data32: [u32; 5],
) -> bool {
    let more_than_three = (data32[1] & 1) != 0;
    if !more_than_three {
        return data32[2..5].contains(&atoms.text_uri_list);
    }
    if let Ok(cookie) = conn.get_property(
        false,
        source_win,
        atoms.xdnd_type_list,
        AtomEnum::ATOM,
        0,
        256,
    ) {
        if let Ok(reply) = cookie.reply() {
            if let Some(mut iter) = reply.value32() {
                return iter.any(|a| a == atoms.text_uri_list);
            }
        }
    }
    // Fallback check inline slots even if XdndTypeList failed.
    data32[2..5].contains(&atoms.text_uri_list)
}

fn send_synthetic_escape_xtest(conn: &RustConnection) {
    // ESC keycode on X11 (HID 0x29 -> evdev 1 -> X11 9)
    let Some(esc_kc) = HidUsage::keyboard(0x29).to_x11_keycode() else {
        return;
    };
    let _ = conn.xtest_fake_input(
        KEY_PRESS_EVENT,
        esc_kc,
        x11rb::CURRENT_TIME,
        x11rb::NONE,
        0,
        0,
        0,
    );
    let _ = conn.xtest_fake_input(
        KEY_RELEASE_EVENT,
        esc_kc,
        x11rb::CURRENT_TIME,
        x11rb::NONE,
        0,
        0,
        0,
    );
    let _ = conn.flush();
}

fn serve_selection_request(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    req: &SelectionRequestEvent,
    xdnd_offer: Option<&ClipboardOffer>,
    clipboard_offer: Option<&ClipboardOffer>,
) -> bool {
    let property = if req.property == x11rb::NONE {
        req.target
    } else {
        req.property
    };
    let mut reply_prop = x11rb::NONE;
    let mut served_xdnd_uri = false;

    if req.selection == atoms.xdnd_selection {
        if let Some(offer) = xdnd_offer {
            if req.target == atoms.targets {
                let list = [
                    atoms.targets,
                    atoms.text_uri_list,
                    atoms.gnome_copied_files,
                    atoms.utf8_string,
                ];
                if conn
                    .change_property32(
                        PropMode::REPLACE,
                        req.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )
                    .is_ok()
                {
                    reply_prop = property;
                }
            } else if req.target == atoms.gnome_copied_files {
                if conn
                    .change_property8(
                        PropMode::REPLACE,
                        req.requestor,
                        property,
                        req.target,
                        &offer.gnome_files,
                    )
                    .is_ok()
                {
                    reply_prop = property;
                    served_xdnd_uri = true;
                }
            } else if (req.target == atoms.text_uri_list
                || req.target == atoms.utf8_string
                || req.target == Atom::from(AtomEnum::STRING))
                && conn
                    .change_property8(
                        PropMode::REPLACE,
                        req.requestor,
                        property,
                        req.target,
                        &offer.uri_list,
                    )
                    .is_ok()
            {
                reply_prop = property;
                served_xdnd_uri = true;
            }
        }
    } else if req.selection == atoms.clipboard {
        if let Some(offer) = clipboard_offer {
            if req.target == atoms.targets {
                // Only advertise file URI formats (NOT UTF8_STRING / text/plain) so Glide's
                // own clipboard watcher does not mistake dropped files for a text copy echo!
                let list = [
                    atoms.targets,
                    atoms.text_uri_list,
                    atoms.gnome_copied_files,
                ];
                if conn
                    .change_property32(
                        PropMode::REPLACE,
                        req.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )
                    .is_ok()
                {
                    reply_prop = property;
                }
            } else if req.target == atoms.text_uri_list {
                if conn
                    .change_property8(
                        PropMode::REPLACE,
                        req.requestor,
                        property,
                        req.target,
                        &offer.uri_list,
                    )
                    .is_ok()
                {
                    reply_prop = property;
                }
            } else if req.target == atoms.gnome_copied_files
                && conn
                    .change_property8(
                        PropMode::REPLACE,
                        req.requestor,
                        property,
                        req.target,
                        &offer.gnome_files,
                    )
                    .is_ok()
            {
                reply_prop = property;
            }
        }
    }

    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: reply_prop,
    };
    let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
    let _ = conn.flush();
    served_xdnd_uri
}

/// Reads an `XdndAware` version (`>= 3`) on `win`, resolving `XdndProxy` if present.
/// Returns `(target_win, send_win, negotiated_version)`.
fn check_xdnd_aware(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    win: Window,
) -> Option<(Window, Window, u32)> {
    let mut send_win = win;
    if let Ok(cookie) = conn.get_property(false, win, atoms.xdnd_proxy, AtomEnum::WINDOW, 0, 1) {
        if let Ok(reply) = cookie.reply() {
            if let Some(proxy) = reply.value32().and_then(|mut it| it.next()) {
                if proxy != x11rb::NONE {
                    // XDND v5 spec requires the proxy window to point to itself via XdndProxy.
                    let valid = conn
                        .get_property(false, proxy, atoms.xdnd_proxy, AtomEnum::WINDOW, 0, 1)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .and_then(|r| r.value32().and_then(|mut it| it.next()))
                        == Some(proxy);
                    if valid {
                        send_win = proxy;
                    }
                }
            }
        }
    }

    let reply = conn
        .get_property(false, send_win, atoms.xdnd_aware, AtomEnum::ANY, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let ver = reply.value32()?.next()?;
    if ver >= 3 {
        Some((win, send_win, ver.min(XDND_VERSION)))
    } else {
        None
    }
}

fn has_wm_state(conn: &RustConnection, atoms: &XdndAtoms, win: Window) -> bool {
    conn.get_property(false, win, atoms.wm_state, AtomEnum::ANY, 0, 1)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|r| r.format != 0 && !r.value.is_empty())
}

enum TargetSearch {
    Found((Window, Window, u32)),
    /// Hit an opaque, WM-managed (`WM_STATE`) non-`XdndAware` application window directly
    /// under the cursor; stop backtracking so we never drop "through" a visible foreground app.
    OpaqueNonTarget,
    /// Transparent/overlay/helper window without `XdndAware`; keep backtracking siblings below.
    TransparentMiss,
}

fn find_xdnd_target_in(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    parent: Window,
    rel_x: i16,
    rel_y: i16,
    ignore: &[Window],
    depth: usize,
) -> TargetSearch {
    if depth >= 32 {
        return TargetSearch::TransparentMiss;
    }
    let Ok(tree) = conn.query_tree(parent).ok().and_then(|c| c.reply().ok()).ok_or(()) else {
        return TargetSearch::TransparentMiss;
    };

    // X11 query_tree returns children bottom-to-top, so iterate in reverse (topmost first).
    for &child in tree.children.iter().rev() {
        if ignore.contains(&child) {
            continue;
        }
        let Ok(attr_c) = conn.get_window_attributes(child) else {
            continue;
        };
        let Ok(attr) = attr_c.reply() else {
            continue;
        };
        if attr.map_state != MapState::VIEWABLE {
            continue;
        }
        let Ok(geom_c) = conn.get_geometry(child) else {
            continue;
        };
        let Ok(geom) = geom_c.reply() else {
            continue;
        };
        let cx = rel_x - geom.x;
        let cy = rel_y - geom.y;
        if cx < 0 || cy < 0 || cx >= geom.width as i16 || cy >= geom.height as i16 {
            continue;
        }

        // Direct hit: window itself advertises XdndAware (works for normal toplevel apps AND
        // InputOnly XdndAware strips).
        if let Some(found) = check_xdnd_aware(conn, atoms, child) {
            return TargetSearch::Found(found);
        }

        // Skip non-XdndAware InputOnly overlay/guard windows and keep checking siblings below!
        if attr.class == WindowClass::INPUT_ONLY {
            continue;
        }

        // Recurse into WM frames / container subtrees.
        match find_xdnd_target_in(conn, atoms, child, cx, cy, ignore, depth + 1) {
            TargetSearch::Found(found) => return TargetSearch::Found(found),
            TargetSearch::OpaqueNonTarget => return TargetSearch::OpaqueNonTarget,
            TargetSearch::TransparentMiss => {
                // If this non-override-redirect window is itself a WM-managed client window
                // (e.g. a non-XdndAware terminal sitting on top of Nautilus), stop here rather
                // than dropping through it to windows hidden underneath.
                if !attr.override_redirect && has_wm_state(conn, atoms, child) {
                    return TargetSearch::OpaqueNonTarget;
                }
                // Otherwise (compositor guard, shadow overlay, dock frame, or empty container),
                // backtrack and inspect siblings underneath!
            }
        }
    }

    TargetSearch::TransparentMiss
}

/// Walks the X11 window tree from `root` downward at `(root_x, root_y)` with backtracking
/// to find the topmost viewable `XdndAware` application window, skipping Glide's own helper
/// windows and non-`XdndAware` `InputOnly`/overlay compositor guard windows.
fn find_xdnd_target(
    conn: &RustConnection,
    root: Window,
    atoms: &XdndAtoms,
    root_x: i16,
    root_y: i16,
    ignore: &[Window],
) -> Option<(Window, Window, u32)> {
    match find_xdnd_target_in(conn, atoms, root, root_x, root_y, ignore, 0) {
        TargetSearch::Found(found) => Some(found),
        TargetSearch::OpaqueNonTarget => None,
        TargetSearch::TransparentMiss => check_xdnd_aware(conn, atoms, root),
    }
}

fn acquire_server_timestamp(conn: &RustConnection, win: Window, prop_time: Atom) -> u32 {
    if conn
        .change_property8(PropMode::APPEND, win, prop_time, AtomEnum::STRING, &[])
        .is_err()
    {
        return x11rb::CURRENT_TIME;
    }
    let _ = conn.flush();
    let deadline = Instant::now() + Duration::from_millis(80);
    while Instant::now() < deadline {
        match conn.poll_for_event() {
            Ok(Some(Event::PropertyNotify(pn))) if pn.window == win && pn.atom == prop_time => {
                return pn.time;
            }
            Ok(Some(_)) => continue,
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(_) => break,
        }
    }
    x11rb::CURRENT_TIME
}

// ---------------------------------------------------------------------------
// Click-through floating drag preview pill (pure x11rb, XFixes rounded + transparent input)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum X11PreviewStatus {
    Receiving,
    Ready,
    Accepted,
    Rejected,
}

struct X11DragPreview {
    win: Window,
    gc: u32,
    font_id: Option<u32>,
    width: u16,
    height: u16,
    label: String,
    badge: String,
    status: X11PreviewStatus,
    geom: DesktopGeometry,
    last_pos: (i32, i32),
}

impl X11DragPreview {
    fn create(
        conn: &RustConnection,
        screen_num: usize,
        root: Window,
        label: String,
        badge: String,
        cursor_x: i32,
        cursor_y: i32,
    ) -> Result<Self> {
        let screen = &conn.setup().roots[screen_num];
        let geom = query_desktop_geometry(conn, screen_num).unwrap_or_default();
        let width: u16 = 252;
        let height: u16 = 46;
        let (x, y) = clamp_hint_popup_pos(&geom, cursor_x, cursor_y, width as i32, height as i32);

        let win = conn.generate_id()?;
        let win_aux = CreateWindowAux::new()
            .background_pixel(0x00FF_FFFF)
            .border_pixel(0x00CB_D5E1)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE);
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root,
            x as i16,
            y as i16,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &win_aux,
        )?;

        if conn
            .xfixes_query_version(4, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some()
        {
            if let Ok(empty_region) = conn.generate_id() {
                if conn.xfixes_create_region(empty_region, &[]).is_ok() {
                    let _ = conn.xfixes_set_window_shape_region(win, SK::INPUT, 0, 0, empty_region);
                    let _ = conn.xfixes_destroy_region(empty_region);
                }
            }
            if let Ok(round_region) = conn.generate_id() {
                let spans = build_rounded_rect_spans(width, height, 10);
                if conn.xfixes_create_region(round_region, &spans).is_ok() {
                    let _ =
                        conn.xfixes_set_window_shape_region(win, SK::BOUNDING, 0, 0, round_region);
                    let _ = conn.xfixes_destroy_region(round_region);
                }
            }
        }

        let font_id = conn.generate_id().ok().and_then(|fid| {
            for pattern in [
                b"-*-dejavu sans-medium-r-*-*-12-*-*-*-*-*-*-*" as &[u8],
                b"-*-liberation sans-medium-r-*-*-12-*-*-*-*-*-*-*",
                b"-*-helvetica-medium-r-*-*-12-*-*-*-*-*-*-*",
                b"7x13",
                b"fixed",
            ] {
                if conn
                    .open_font(fid, pattern)
                    .ok()
                    .and_then(|c| c.check().ok())
                    .is_some()
                {
                    return Some(fid);
                }
            }
            None
        });

        let gc = conn.generate_id()?;
        let mut gc_aux = CreateGCAux::new()
            .foreground(0x000F_172A)
            .background(0x00FF_FFFF);
        if let Some(fid) = font_id {
            gc_aux = gc_aux.font(fid);
        }
        conn.create_gc(gc, win, &gc_aux)?;
        conn.map_window(win)?;

        let preview = Self {
            win,
            gc,
            font_id,
            width,
            height,
            label,
            badge,
            status: X11PreviewStatus::Receiving,
            geom,
            last_pos: (x, y),
        };
        preview.draw(conn);
        Ok(preview)
    }

    fn move_to(&mut self, conn: &RustConnection, cursor_x: i32, cursor_y: i32) {
        let (x, y) = clamp_hint_popup_pos(
            &self.geom,
            cursor_x,
            cursor_y,
            self.width as i32,
            self.height as i32,
        );
        if (x, y) == self.last_pos {
            return;
        }
        self.last_pos = (x, y);
        let _ = conn.configure_window(
            self.win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .stack_mode(StackMode::ABOVE),
        );
        let _ = conn.flush();
    }

    fn set_status(&mut self, conn: &RustConnection, status: X11PreviewStatus) {
        if self.status != status {
            self.status = status;
            self.draw(conn);
        }
    }

    fn draw(&self, conn: &RustConnection) {
        let (accent, border, sub_color, sub_text) = match self.status {
            X11PreviewStatus::Receiving => (0x0025_63EB, 0x00CB_D5E1, 0x0064_748B, "Receiving..."),
            X11PreviewStatus::Ready => (0x0025_63EB, 0x00CB_D5E1, 0x0047_5569, "Drop in folder or app"),
            X11PreviewStatus::Accepted => (0x0016_A34A, 0x0086_EFAC, 0x0015_803D, "+ Copy here"),
            X11PreviewStatus::Rejected => (0x0064_748B, 0x00CB_D5E1, 0x0064_748B, "Release for Ctrl+V copy"),
        };

        // White background card
        let _ = conn.change_gc(self.gc, &ChangeGCAux::new().foreground(0x00FF_FFFF));
        let _ = conn.poly_fill_rectangle(
            self.win,
            self.gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            }],
        );

        // 1px border
        let _ = conn.change_gc(self.gc, &ChangeGCAux::new().foreground(border));
        let _ = conn.poly_rectangle(
            self.win,
            self.gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: self.width.saturating_sub(1),
                height: self.height.saturating_sub(1),
            }],
        );

        // Left file-type pill icon
        let _ = conn.change_gc(self.gc, &ChangeGCAux::new().foreground(accent));
        let _ = conn.poly_fill_rectangle(
            self.win,
            self.gc,
            &[Rectangle {
                x: 10,
                y: 9,
                width: 34,
                height: 28,
            }],
        );
        let _ = conn.change_gc(
            self.gc,
            &ChangeGCAux::new()
                .foreground(0x00FF_FFFF)
                .background(accent),
        );
        let badge_ascii: Vec<u8> = self
            .badge
            .bytes()
            .filter(|b| b.is_ascii_graphic())
            .take(4)
            .collect();
        let badge_x = 13 + (4usize.saturating_sub(badge_ascii.len()) as i16) * 3;
        let _ = conn.image_text8(self.win, self.gc, badge_x, 27, &badge_ascii);

        // Primary filename / count line (ASCII-safe Latin1 for core X11 image_text8)
        let label_bytes: Vec<u8> = self
            .label
            .chars()
            .take(25)
            .map(|c| {
                let code = c as u32;
                if (0x20..=0xFF).contains(&code) {
                    code as u8
                } else {
                    b'?'
                }
            })
            .collect();
        let _ = conn.change_gc(
            self.gc,
            &ChangeGCAux::new()
                .foreground(0x000F_172A)
                .background(0x00FF_FFFF),
        );
        let _ = conn.image_text8(self.win, self.gc, 54, 20, &label_bytes);

        // Status subtitle line
        let _ = conn.change_gc(
            self.gc,
            &ChangeGCAux::new()
                .foreground(sub_color)
                .background(0x00FF_FFFF),
        );
        let _ = conn.image_text8(self.win, self.gc, 54, 36, sub_text.as_bytes());
        let _ = conn.flush();
    }

    fn destroy(self, conn: &RustConnection) {
        let _ = conn.free_gc(self.gc);
        if let Some(fid) = self.font_id {
            let _ = conn.close_font(fid);
        }
        let _ = conn.unmap_window(self.win);
        let _ = conn.destroy_window(self.win);
        let _ = conn.flush();
    }
}

// ---------------------------------------------------------------------------
// Live XDND v5 hover session (highlights folders in Nautilus/Dolphin/Thunar/Chrome live!)
// ---------------------------------------------------------------------------

struct ActiveLiveXdnd {
    offer: ClipboardOffer,
    timestamp: u32,
    target: Option<(Window, Window, u32)>,
    accepted: bool,
    last_pos: (i16, i16),
    last_pos_at: Instant,
}

fn send_xdnd_enter_msg(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    src_win: Window,
    target_win: Window,
    send_win: Window,
    ver: u32,
) {
    // Advertise text/uri-list, x-special/gnome-copied-files, and UTF8_STRING inline
    // (<= 3 types fits directly inside XdndEnter without requiring XdndTypeList lookup,
    // while XdndTypeList is also populated on src_win for clients that read it unconditionally).
    let enter = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: target_win,
        type_: atoms.xdnd_enter,
        data: ClientMessageData::from([
            src_win,
            ver << 24,
            atoms.text_uri_list,
            atoms.gnome_copied_files,
            atoms.utf8_string,
        ]),
    };
    let _ = conn.send_event(false, send_win, EventMask::NO_EVENT, enter);
}

fn send_xdnd_position_msg(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    src_win: Window,
    target_win: Window,
    send_win: Window,
    pos: (i16, i16),
    timestamp: u32,
) {
    let coords = (((pos.0 as u16) as u32) << 16) | ((pos.1 as u16) as u32);
    let msg = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: target_win,
        type_: atoms.xdnd_position,
        data: ClientMessageData::from([src_win, 0, coords, timestamp, atoms.xdnd_action_copy]),
    };
    let _ = conn.send_event(false, send_win, EventMask::NO_EVENT, msg);
}

fn send_xdnd_leave_msg(
    conn: &RustConnection,
    atoms: &XdndAtoms,
    src_win: Window,
    target_win: Window,
    send_win: Window,
) {
    let leave = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: target_win,
        type_: atoms.xdnd_leave,
        data: ClientMessageData::from([src_win, 0, 0, 0, 0]),
    };
    let _ = conn.send_event(false, send_win, EventMask::NO_EVENT, leave);
}

fn start_live_xdnd_session(
    conn: &RustConnection,
    root: Window,
    src_win: Window,
    atoms: &XdndAtoms,
    paths: &[PathBuf],
    ignore: &[Window],
) -> Option<ActiveLiveXdnd> {
    let uri_list = format_uri_list(paths).into_bytes();
    let gnome_files = format_gnome_copied_files(paths).into_bytes();
    if uri_list.is_empty() {
        return None;
    }

    let timestamp = acquire_server_timestamp(conn, src_win, atoms.prop_time);
    let _ = conn.change_property32(
        PropMode::REPLACE,
        src_win,
        atoms.xdnd_type_list,
        AtomEnum::ATOM,
        &[
            atoms.text_uri_list,
            atoms.gnome_copied_files,
            atoms.utf8_string,
        ],
    );
    if conn
        .set_selection_owner(src_win, atoms.xdnd_selection, timestamp)
        .is_err()
    {
        return None;
    }
    let _ = conn.flush();

    let owner = conn
        .get_selection_owner(atoms.xdnd_selection)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.owner);
    if owner != Some(src_win) {
        return None;
    }

    let mut live = ActiveLiveXdnd {
        offer: ClipboardOffer {
            uri_list,
            gnome_files,
        },
        timestamp,
        target: None,
        accepted: false,
        last_pos: (i16::MIN, i16::MIN),
        last_pos_at: Instant::now() - Duration::from_secs(1),
    };

    if let Ok(cookie) = conn.query_pointer(root) {
        if let Ok(ptr) = cookie.reply() {
            update_live_xdnd_hover(
                conn,
                root,
                src_win,
                atoms,
                &mut live,
                ptr.root_x,
                ptr.root_y,
                ignore,
            );
        }
    }
    Some(live)
}

#[allow(clippy::too_many_arguments)]
fn update_live_xdnd_hover(
    conn: &RustConnection,
    root: Window,
    src_win: Window,
    atoms: &XdndAtoms,
    live: &mut ActiveLiveXdnd,
    x: i16,
    y: i16,
    ignore: &[Window],
) {
    let found = find_xdnd_target(conn, root, atoms, x, y, ignore);
    if found != live.target {
        if let Some((old_target, old_send, _)) = live.target.take() {
            send_xdnd_leave_msg(conn, atoms, src_win, old_target, old_send);
        }
        live.accepted = false;
        if let Some((target_win, send_win, ver)) = found {
            send_xdnd_enter_msg(conn, atoms, src_win, target_win, send_win, ver);
            send_xdnd_position_msg(conn, atoms, src_win, target_win, send_win, (x, y), live.timestamp);
            live.target = Some((target_win, send_win, ver));
            live.last_pos = (x, y);
            live.last_pos_at = Instant::now();
        }
        let _ = conn.flush();
    } else if let Some((target_win, send_win, _)) = live.target {
        if (x, y) != live.last_pos || live.last_pos_at.elapsed() >= Duration::from_millis(50) {
            send_xdnd_position_msg(conn, atoms, src_win, target_win, send_win, (x, y), live.timestamp);
            let _ = conn.flush();
            live.last_pos = (x, y);
            live.last_pos_at = Instant::now();
        }
    }
}

fn finish_live_xdnd_cancel(
    conn: &RustConnection,
    src_win: Window,
    atoms: &XdndAtoms,
    live: ActiveLiveXdnd,
) {
    if let Some((target_win, send_win, _)) = live.target {
        send_xdnd_leave_msg(conn, atoms, src_win, target_win, send_win);
        let _ = conn.flush();
    }
}

#[allow(clippy::too_many_arguments)]
fn perform_xdnd_drop_or_fallback(
    conn: &RustConnection,
    root: Window,
    src_win: Window,
    atoms: &XdndAtoms,
    strips: &[StripWin],
    paths: &[PathBuf],
    live_xdnd: Option<ActiveLiveXdnd>,
    clipboard_offer: &mut Option<ClipboardOffer>,
) -> Result<DropDeliveryOutcome> {
    let uri_list = format_uri_list(paths).into_bytes();
    let gnome_files = format_gnome_copied_files(paths).into_bytes();
    if uri_list.is_empty() {
        if let Some(live) = live_xdnd {
            finish_live_xdnd_cancel(conn, src_win, atoms, live);
        }
        return Ok(DropDeliveryOutcome::ClipboardFallback);
    }

    let xdnd_offer = ClipboardOffer {
        uri_list,
        gnome_files,
    };

    let ptr = conn
        .query_pointer(root)
        .context("X11 query_pointer failed during XDND drop")?
        .reply()
        .context("X11 query_pointer reply failed")?;
    let drop_x = ptr.root_x;
    let drop_y = ptr.root_y;

    let mut ignore: Vec<Window> = Vec::with_capacity(strips.len() + 1);
    ignore.push(src_win);
    for s in strips {
        ignore.push(s.win);
    }

    let current_target = find_xdnd_target(conn, root, atoms, drop_x, drop_y, &ignore);

    let (reused_live, mut already_accepted, timestamp) = match live_xdnd {
        Some(live) => {
            if live.target == current_target && current_target.is_some() {
                (true, live.accepted, live.timestamp)
            } else {
                finish_live_xdnd_cancel(conn, src_win, atoms, live);
                (
                    false,
                    false,
                    acquire_server_timestamp(conn, src_win, atoms.prop_time),
                )
            }
        }
        None => (
            false,
            false,
            acquire_server_timestamp(conn, src_win, atoms.prop_time),
        ),
    };

    if let Some((target_win, send_win, ver)) = current_target {
        debug!(
            target_win,
            send_win,
            ver,
            drop_x,
            drop_y,
            reused_live,
            already_accepted,
            count = paths.len(),
            "Initiating XDND v5 drop handoff"
        );

        let _ = conn.change_property32(
            PropMode::REPLACE,
            src_win,
            atoms.xdnd_type_list,
            AtomEnum::ATOM,
            &[
                atoms.text_uri_list,
                atoms.gnome_copied_files,
                atoms.utf8_string,
            ],
        );
        if !reused_live {
            conn.set_selection_owner(src_win, atoms.xdnd_selection, timestamp)?;
            conn.flush()?;
        }

        let owner = conn
            .get_selection_owner(atoms.xdnd_selection)?
            .reply()?
            .owner;
        if owner == src_win {
            if !reused_live {
                send_xdnd_enter_msg(conn, atoms, src_win, target_win, send_win, ver);
            }
            send_xdnd_position_msg(conn, atoms, src_win, target_win, send_win, (drop_x, drop_y), timestamp);
            let _ = conn.flush();

            // Wait for XdndStatus (if live hover already received acceptance, wait briefly for
            // any updated status at the exact release coordinate without blocking unnecessarily).
            let wait_ms = if already_accepted { 120 } else { 550 };
            let status_deadline = Instant::now() + Duration::from_millis(wait_ms);
            while Instant::now() < status_deadline {
                match conn.poll_for_event() {
                    Ok(Some(Event::ClientMessage(cm))) if cm.type_ == atoms.xdnd_status => {
                        let d = cm.data.as_data32();
                        if d[0] == target_win || d[0] == send_win {
                            already_accepted = (d[1] & 1) != 0;
                            trace!(
                                target_win,
                                accepted = already_accepted,
                                action = d[4],
                                "Received XdndStatus"
                            );
                            break;
                        }
                    }
                    Ok(Some(Event::SelectionRequest(req))) => {
                        serve_selection_request(
                            conn,
                            atoms,
                            &req,
                            Some(&xdnd_offer),
                            clipboard_offer.as_ref(),
                        );
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => thread::sleep(Duration::from_millis(4)),
                    Err(_) => break,
                }
            }

            if already_accepted {
                // Send XdndDrop and serve SelectionRequest(XdndSelection) until XdndFinished.
                let drop_ev = ClientMessageEvent {
                    response_type: CLIENT_MESSAGE_EVENT,
                    format: 32,
                    sequence: 0,
                    window: target_win,
                    type_: atoms.xdnd_drop,
                    data: ClientMessageData::from([src_win, 0, timestamp, 0, 0]),
                };
                let _ = conn.send_event(false, send_win, EventMask::NO_EVENT, drop_ev);
                let _ = conn.flush();

                let mut served_uri = false;
                let mut served_at: Option<Instant> = None;
                let drop_deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < drop_deadline {
                    if let Some(t) = served_at {
                        if t.elapsed() >= Duration::from_millis(300) {
                            break;
                        }
                    }
                    match conn.poll_for_event() {
                        Ok(Some(Event::SelectionRequest(req))) => {
                            if serve_selection_request(
                                conn,
                                atoms,
                                &req,
                                Some(&xdnd_offer),
                                clipboard_offer.as_ref(),
                            ) {
                                served_uri = true;
                                served_at = Some(Instant::now());
                            }
                        }
                        Ok(Some(Event::ClientMessage(cm))) if cm.type_ == atoms.xdnd_finished => {
                            let d = cm.data.as_data32();
                            if d[0] == target_win || d[0] == send_win {
                                debug!(target_win, flags = d[1], "XDND v5 drop finished");
                                return Ok(DropDeliveryOutcome::DroppedIntoTarget);
                            }
                        }
                        Ok(Some(Event::SelectionClear(sc)))
                            if sc.selection == atoms.xdnd_selection =>
                        {
                            break;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => thread::sleep(Duration::from_millis(4)),
                        Err(_) => break,
                    }
                }

                if served_uri {
                    debug!(
                        target_win,
                        "XDND v5 selection served to target (no XdndFinished received)"
                    );
                    return Ok(DropDeliveryOutcome::DroppedIntoTarget);
                }
            } else {
                send_xdnd_leave_msg(conn, atoms, src_win, target_win, send_win);
                let _ = conn.flush();
            }
        }
    }

    // Graceful fallback: publish `text/uri-list` + `x-special/gnome-copied-files` on X11 CLIPBOARD.
    *clipboard_offer = Some(xdnd_offer);
    let _ = conn.set_selection_owner(src_win, atoms.clipboard, x11rb::CURRENT_TIME);
    let _ = conn.flush();
    debug!(
        count = paths.len(),
        "Published text/uri-list fallback to X11 CLIPBOARD"
    );
    spawn_x11_fallback_hint(drop_x as i32, drop_y as i32);
    Ok(DropDeliveryOutcome::ClipboardFallback)
}

fn build_ignore_list(
    src_win: Window,
    strips: &[StripWin],
    preview: Option<&X11DragPreview>,
) -> Vec<Window> {
    let mut ignore = Vec::with_capacity(strips.len() + 2);
    ignore.push(src_win);
    for s in strips {
        ignore.push(s.win);
    }
    if let Some(p) = preview {
        ignore.push(p.win);
    }
    ignore
}

fn run_x11_dnd_loop(
    conn: Arc<RustConnection>,
    screen_num: usize,
    root: Window,
    src_win: Window,
    atoms: XdndAtoms,
    shared: Arc<SharedX11Dnd>,
) {
    let mut strips: Vec<StripWin> = Vec::new();
    let mut active_inbound: Option<ActiveEdgeInbound> = None;
    let mut clipboard_offer: Option<ClipboardOffer> = None;
    let mut preview: Option<X11DragPreview> = None;
    let mut live_xdnd: Option<ActiveLiveXdnd> = None;
    let mut last_poll_ptr: (i16, i16) = (i16::MIN, i16::MIN);

    'outer: loop {
        // Idle mode: block with 0% CPU until an X11 event or wake command arrives.
        // Active incoming drag mode: poll at ~80Hz so local Server drags also move the
        // floating preview pill and drive live XDND folder highlighting smoothly.
        let ev = if preview.is_some() || live_xdnd.is_some() {
            match conn.poll_for_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => {
                    if let Ok(cookie) = conn.query_pointer(root) {
                        if let Ok(ptr) = cookie.reply() {
                            if (ptr.root_x, ptr.root_y) != last_poll_ptr {
                                last_poll_ptr = (ptr.root_x, ptr.root_y);
                                if let Some(ref mut p) = preview {
                                    p.move_to(&conn, ptr.root_x as i32, ptr.root_y as i32);
                                }
                                if let Some(ref mut live) = live_xdnd {
                                    let ignore =
                                        build_ignore_list(src_win, &strips, preview.as_ref());
                                    update_live_xdnd_hover(
                                        &conn,
                                        root,
                                        src_win,
                                        &atoms,
                                        live,
                                        ptr.root_x,
                                        ptr.root_y,
                                        &ignore,
                                    );
                                    if let Some(ref mut p) = preview {
                                        let st = if live.target.is_none() {
                                            X11PreviewStatus::Ready
                                        } else if live.accepted {
                                            X11PreviewStatus::Accepted
                                        } else {
                                            X11PreviewStatus::Ready
                                        };
                                        p.set_status(&conn, st);
                                    }
                                }
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(12));
                    continue;
                }
                Err(e) => {
                    debug!(error = %e, "X11 DnD connection closed");
                    break;
                }
            }
        } else {
            match conn.wait_for_event() {
                Ok(ev) => ev,
                Err(e) => {
                    debug!(error = %e, "X11 DnD connection closed");
                    break;
                }
            }
        };

        match ev {
            Event::ClientMessage(cm) if cm.type_ == atoms.wake => {
                loop {
                    let next = shared.cmds.lock().pop_front();
                    let Some(cmd) = next else { break };
                    match cmd {
                        DndCmd::SyncStrips => {
                            sync_x11_strips(
                                &conn,
                                screen_num,
                                root,
                                &atoms,
                                &shared,
                                &mut strips,
                            );
                        }
                        DndCmd::HideStrips => {
                            active_inbound = None;
                            unmap_strips(&conn, &mut strips);
                        }
                        DndCmd::CancelLocalDrag => {
                            if let Some(inb) = active_inbound.take() {
                                send_xdnd_status(&conn, &atoms, inb.source_win, inb.strip_win, false);
                            }
                            unmap_strips(&conn, &mut strips);
                            send_synthetic_escape_xtest(&conn);
                        }
                        DndCmd::IncomingDragStart { label, badge, x, y } => {
                            if let Some(old) = preview.take() {
                                old.destroy(&conn);
                            }
                            if let Some(old_live) = live_xdnd.take() {
                                finish_live_xdnd_cancel(&conn, src_win, &atoms, old_live);
                            }
                            let (px, py) = conn
                                .query_pointer(root)
                                .ok()
                                .and_then(|c| c.reply().ok())
                                .map(|p| (p.root_x as i32, p.root_y as i32))
                                .unwrap_or((x, y));
                            last_poll_ptr = (px as i16, py as i16);
                            match X11DragPreview::create(
                                &conn, screen_num, root, label, badge, px, py,
                            ) {
                                Ok(p) => preview = Some(p),
                                Err(e) => debug!(error = %e, "Could not create X11 drag preview pill"),
                            }
                        }
                        DndCmd::IncomingDragMove { x, y } => {
                            last_poll_ptr = (x as i16, y as i16);
                            if let Some(ref mut p) = preview {
                                p.move_to(&conn, x, y);
                            }
                            if let Some(ref mut live) = live_xdnd {
                                let ignore = build_ignore_list(src_win, &strips, preview.as_ref());
                                update_live_xdnd_hover(
                                    &conn,
                                    root,
                                    src_win,
                                    &atoms,
                                    live,
                                    x as i16,
                                    y as i16,
                                    &ignore,
                                );
                            }
                        }
                        DndCmd::IncomingDragFilesReady { paths } => {
                            if let Some(old_live) = live_xdnd.take() {
                                finish_live_xdnd_cancel(&conn, src_win, &atoms, old_live);
                            }
                            let ignore = build_ignore_list(src_win, &strips, preview.as_ref());
                            live_xdnd =
                                start_live_xdnd_session(&conn, root, src_win, &atoms, &paths, &ignore);
                            if let Some(ref mut p) = preview {
                                p.set_status(&conn, X11PreviewStatus::Ready);
                            }
                        }
                        DndCmd::IncomingDragCancel => {
                            if let Some(old) = preview.take() {
                                old.destroy(&conn);
                            }
                            if let Some(old_live) = live_xdnd.take() {
                                finish_live_xdnd_cancel(&conn, src_win, &atoms, old_live);
                            }
                            last_poll_ptr = (i16::MIN, i16::MIN);
                        }
                        DndCmd::DeliverDrop { paths, reply } => {
                            if let Some(old) = preview.take() {
                                old.destroy(&conn);
                            }
                            let live = live_xdnd.take();
                            last_poll_ptr = (i16::MIN, i16::MIN);
                            let res = perform_xdnd_drop_or_fallback(
                                &conn,
                                root,
                                src_win,
                                &atoms,
                                &strips,
                                &paths,
                                live,
                                &mut clipboard_offer,
                            );
                            let _ = reply.send(res);
                        }
                        DndCmd::Shutdown => {
                            break 'outer;
                        }
                    }
                }
            }

            // --- Live drag-in: XdndStatus feedback from hovered target ---
            Event::ClientMessage(cm) if cm.type_ == atoms.xdnd_status => {
                if let Some(ref mut live) = live_xdnd {
                    let d = cm.data.as_data32();
                    if let Some((target_win, send_win, _)) = live.target {
                        if d[0] == target_win || d[0] == send_win {
                            let accepted = (d[1] & 1) != 0;
                            live.accepted = accepted;
                            if let Some(ref mut p) = preview {
                                p.set_status(
                                    &conn,
                                    if accepted {
                                        X11PreviewStatus::Accepted
                                    } else {
                                        X11PreviewStatus::Rejected
                                    },
                                );
                            }
                        }
                    }
                }
            }

            Event::Expose(exp) => {
                if let Some(ref p) = preview {
                    if exp.window == p.win {
                        p.draw(&conn);
                    }
                }
            }

            // --- Drag-out: edge-strip XdndEnter ---
            Event::ClientMessage(cm) if cm.type_ == atoms.xdnd_enter => {
                let Some(strip) = strips.iter().find(|s| s.win == cm.window) else {
                    continue;
                };
                let d = cm.data.as_data32();
                let source_win = d[0];
                let edge = strip.rect.edge;
                let has_uri = offered_types_include_uri_list(&conn, &atoms, source_win, d);
                if has_uri {
                    debug!(?edge, source_win, "XdndEnter with text/uri-list on X11 edge strip");
                    let _ = conn.convert_selection(
                        cm.window,
                        atoms.xdnd_selection,
                        atoms.text_uri_list,
                        atoms.prop_data,
                        x11rb::CURRENT_TIME,
                    );
                    let _ = conn.flush();
                    active_inbound = Some(ActiveEdgeInbound {
                        edge,
                        strip_win: cm.window,
                        source_win,
                        has_uri_list: true,
                        requested_selection: true,
                    });
                } else {
                    // Non-file XDND drag (e.g. text or tab): reject and hide strip immediately.
                    send_xdnd_status(&conn, &atoms, source_win, cm.window, false);
                    shared.armed.store(false, Ordering::SeqCst);
                    unmap_strips(&conn, &mut strips);
                    active_inbound = None;
                }
            }

            // --- Drag-out: edge-strip XdndPosition ---
            Event::ClientMessage(cm) if cm.type_ == atoms.xdnd_position => {
                let d = cm.data.as_data32();
                let source_win = d[0];
                let time = d[3];
                if let Some(ref mut inb) = active_inbound {
                    if inb.strip_win == cm.window && inb.source_win == source_win && inb.has_uri_list
                    {
                        let already_staged = shared
                            .staged
                            .lock()
                            .as_ref()
                            .is_some_and(|s| s.edge == inb.edge);
                        if !already_staged && (!inb.requested_selection || time != 0) {
                            inb.requested_selection = true;
                            let _ = conn.convert_selection(
                                cm.window,
                                atoms.xdnd_selection,
                                atoms.text_uri_list,
                                atoms.prop_data,
                                if time != 0 { time } else { x11rb::CURRENT_TIME },
                            );
                        }
                        if let Some(ref mut staged) = *shared.staged.lock() {
                            if staged.edge == inb.edge {
                                staged.staged_at = Instant::now();
                                staged.left_at = None;
                                shared.staged_tracker.touch(inb.edge);
                            }
                        }
                        send_xdnd_status(&conn, &atoms, source_win, cm.window, true);
                        continue;
                    }
                }
                send_xdnd_status(&conn, &atoms, source_win, cm.window, false);
            }

            // --- Drag-out: edge-strip XdndLeave ---
            Event::ClientMessage(cm) if cm.type_ == atoms.xdnd_leave => {
                if let Some(inb) = active_inbound.take() {
                    if inb.strip_win == cm.window {
                        if let Some(ref mut staged) = *shared.staged.lock() {
                            if staged.edge == inb.edge {
                                staged.left_at = Some(Instant::now());
                                shared.staged_tracker.mark_left(inb.edge);
                            }
                        }
                    } else {
                        active_inbound = Some(inb);
                    }
                }
            }

            // --- Drag-out: edge-strip XdndDrop ---
            Event::ClientMessage(cm) if cm.type_ == atoms.xdnd_drop => {
                let d = cm.data.as_data32();
                let source_win = d[0];
                // Always reply accepted=false so source never deletes original files on a Move drag.
                send_xdnd_finished(&conn, &atoms, source_win, cm.window, false);
                active_inbound = None;
            }

            // --- Drag-out: SelectionNotify containing text/uri-list ---
            Event::SelectionNotify(sn) if sn.selection == atoms.xdnd_selection => {
                if sn.property == x11rb::NONE {
                    continue;
                }
                let edge = active_inbound
                    .as_ref()
                    .filter(|i| i.strip_win == sn.requestor)
                    .map(|i| i.edge)
                    .or_else(|| {
                        strips
                            .iter()
                            .find(|s| s.win == sn.requestor)
                            .map(|s| s.rect.edge)
                    });
                let Some(edge) = edge else { continue };

                if let Ok(cookie) = conn.get_property(
                    true,
                    sn.requestor,
                    sn.property,
                    AtomEnum::ANY,
                    0,
                    65536,
                ) {
                    if let Ok(prop) = cookie.reply() {
                        let paths = parse_uri_list(&prop.value);
                        if !paths.is_empty() {
                            debug!(
                                ?edge,
                                count = paths.len(),
                                "Captured text/uri-list paths on X11 edge strip"
                            );
                            *shared.staged.lock() = Some(StagedDrag {
                                edge,
                                paths,
                                staged_at: Instant::now(),
                                left_at: None,
                            });
                            shared.staged_tracker.mark_staged(edge, false);
                        }
                    }
                }
            }

            // --- Live XDND + Clipboard fallback SelectionRequest ---
            Event::SelectionRequest(req) => {
                serve_selection_request(
                    &conn,
                    &atoms,
                    &req,
                    live_xdnd.as_ref().map(|l| &l.offer),
                    clipboard_offer.as_ref(),
                );
            }

            Event::SelectionClear(sc) if sc.selection == atoms.clipboard => {
                clipboard_offer = None;
            }

            _ => {}
        }
    }

    if let Some(old) = preview.take() {
        old.destroy(&conn);
    }
    if let Some(old_live) = live_xdnd.take() {
        finish_live_xdnd_cancel(&conn, src_win, &atoms, old_live);
    }
    destroy_strips(&conn, &mut strips);
    let _ = conn.destroy_window(src_win);
    let _ = conn.flush();
}

/// Standalone helper that publishes `text/uri-list` + `x-special/gnome-copied-files`
/// on X11 `CLIPBOARD` (used as XWayland fallback on Wayland compositors without data-control).
pub fn publish_x11_clipboard_uris(paths: &[PathBuf]) -> Result<()> {
    let uri_list = format_uri_list(paths).into_bytes();
    let gnome_files = format_gnome_copied_files(paths).into_bytes();
    if uri_list.is_empty() {
        return Ok(());
    }

    let (conn, screen_num) =
        RustConnection::connect(None).context("failed to connect to X11/XWayland display")?;
    let root = conn.setup().roots[screen_num].root;
    let atoms = XdndAtoms::intern(&conn)?;

    let win = conn.generate_id()?;
    conn.create_window(
        0,
        win,
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
    conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME)?;
    conn.flush()?;

    let offer = ClipboardOffer {
        uri_list,
        gnome_files,
    };

    thread::Builder::new()
        .name("glide-x11-uri-clip".into())
        .spawn(move || {
            loop {
                match conn.wait_for_event() {
                    Ok(Event::SelectionRequest(req)) if req.selection == atoms.clipboard => {
                        serve_selection_request(&conn, &atoms, &req, None, Some(&offer));
                    }
                    Ok(Event::SelectionClear(sc)) if sc.selection == atoms.clipboard => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = conn.destroy_window(win);
            let _ = conn.flush();
        })
        .context("failed to spawn X11 URI clipboard helper thread")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Cursor-adjacent fallback hint popup (pure x11rb, override_redirect, click-through)
// ---------------------------------------------------------------------------

/// Spawns a non-blocking, click-through, non-focus-stealing floating white card next to the
/// cursor on X11 when a target window cannot accept an XDND drop. Auto-closes after 3.5 seconds.
pub fn spawn_x11_fallback_hint(cursor_x: i32, cursor_y: i32) {
    let _ = thread::Builder::new()
        .name("glide-x11-dnd-hint".into())
        .spawn(move || {
            if let Err(e) = show_x11_fallback_hint_blocking(cursor_x, cursor_y) {
                debug!(error = %e, "Could not display X11 DnD fallback hint popup");
            }
        });
}

fn build_rounded_rect_spans(w: u16, h: u16, radius: i32) -> Vec<x11rb::protocol::xproto::Rectangle> {
    use x11rb::protocol::xproto::Rectangle;
    let r = radius.min(w as i32 / 2).min(h as i32 / 2);
    let mut rects = Vec::with_capacity(h as usize);
    for row in 0..h as i32 {
        let dy = if row < r {
            r - row
        } else if row >= h as i32 - r {
            row - (h as i32 - 1 - r)
        } else {
            0
        };
        let inset = if dy > 0 {
            let inside = ((r * r - dy * dy).max(0) as f32).sqrt().floor() as i32;
            (r - inside).max(0)
        } else {
            0
        };
        let span_w = (w as i32 - 2 * inset).max(1) as u16;
        rects.push(Rectangle {
            x: inset as i16,
            y: row as i16,
            width: span_w,
            height: 1,
        });
    }
    rects
}

fn show_x11_fallback_hint_blocking(cursor_x: i32, cursor_y: i32) -> Result<()> {
    use crate::dnd::{clamp_hint_popup_pos, FALLBACK_HINT_SUBTITLE, FALLBACK_HINT_TITLE};
    use x11rb::protocol::shape::SK;
    use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
    use x11rb::protocol::xproto::{
        Arc as X11Arc, ChangeGCAux, CreateGCAux, Rectangle,
    };

    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let geom = query_desktop_geometry(&conn, screen_num).unwrap_or_default();

    let width: u16 = 344;
    let height: u16 = 56;
    let (x, y) = clamp_hint_popup_pos(&geom, cursor_x, cursor_y, width as i32, height as i32);

    let win = conn.generate_id()?;
    let win_aux = CreateWindowAux::new()
        .background_pixel(0x00FF_FFFF)
        .border_pixel(0x00E2_E8F0)
        .override_redirect(1)
        .event_mask(EventMask::EXPOSURE);

    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        x as i16,
        y as i16,
        width,
        height,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &win_aux,
    )?;

    // Make the floating card 100% click-through & give it rounded corners via XFixes shapes.
    if conn
        .xfixes_query_version(4, 0)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some()
    {
        if let Ok(empty_region) = conn.generate_id() {
            if conn.xfixes_create_region(empty_region, &[]).is_ok() {
                let _ = conn.xfixes_set_window_shape_region(win, SK::INPUT, 0, 0, empty_region);
                let _ = conn.xfixes_destroy_region(empty_region);
            }
        }
        if let Ok(round_region) = conn.generate_id() {
            let spans = build_rounded_rect_spans(width, height, 10);
            if conn.xfixes_create_region(round_region, &spans).is_ok() {
                let _ = conn.xfixes_set_window_shape_region(win, SK::BOUNDING, 0, 0, round_region);
                let _ = conn.xfixes_destroy_region(round_region);
            }
        }
    }

    // Try crisp sans fonts first, falling back to universal X11 "fixed".
    let font_id = conn.generate_id()?;
    let mut font_opened = false;
    for pattern in [
        b"-*-dejavu sans-medium-r-*-*-12-*-*-*-*-*-*-*" as &[u8],
        b"-*-liberation sans-medium-r-*-*-12-*-*-*-*-*-*-*",
        b"-*-helvetica-medium-r-*-*-12-*-*-*-*-*-*-*",
        b"7x13",
        b"fixed",
    ] {
        if conn
            .open_font(font_id, pattern)
            .ok()
            .and_then(|c| c.check().ok())
            .is_some()
        {
            font_opened = true;
            break;
        }
    }

    let gc = conn.generate_id()?;
    let mut gc_aux = CreateGCAux::new()
        .foreground(0x000F_172A)
        .background(0x00FF_FFFF);
    if font_opened {
        gc_aux = gc_aux.font(font_id);
    }
    conn.create_gc(gc, win, &gc_aux)?;

    let draw = |conn: &RustConnection| {
        // White fill (#FFFFFF)
        let _ = conn.change_gc(gc, &ChangeGCAux::new().foreground(0x00FF_FFFF));
        let _ = conn.poly_fill_rectangle(
            win,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width,
                height,
            }],
        );

        // Subtle 1px hairline border (#E2E8F0)
        let _ = conn.change_gc(gc, &ChangeGCAux::new().foreground(0x00E2_E8F0));
        let _ = conn.poly_rectangle(
            win,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: width.saturating_sub(1),
                height: height.saturating_sub(1),
            }],
        );

        // Small blue info circle (#2563EB) on the left + crisp white 'i'
        let icon_x: i16 = 14;
        let icon_y: i16 = ((height as i16) - 20) / 2;
        let _ = conn.change_gc(gc, &ChangeGCAux::new().foreground(0x0025_63EB));
        let _ = conn.poly_fill_arc(
            win,
            gc,
            &[X11Arc {
                x: icon_x,
                y: icon_y,
                width: 20,
                height: 20,
                angle1: 0,
                angle2: 360 * 64,
            }],
        );
        let _ = conn.change_gc(gc, &ChangeGCAux::new().foreground(0x00FF_FFFF));
        let _ = conn.poly_fill_rectangle(
            win,
            gc,
            &[
                Rectangle {
                    x: icon_x + 9,
                    y: icon_y + 5,
                    width: 2,
                    height: 2,
                },
                Rectangle {
                    x: icon_x + 9,
                    y: icon_y + 9,
                    width: 2,
                    height: 6,
                },
            ],
        );

        // Title (#0F172A) & subtitle (#475569)
        let text_x: i16 = icon_x + 20 + 10;
        let _ = conn.change_gc(
            gc,
            &ChangeGCAux::new()
                .foreground(0x000F_172A)
                .background(0x00FF_FFFF),
        );
        let _ = conn.image_text8(win, gc, text_x, 23, FALLBACK_HINT_TITLE.as_bytes());

        let _ = conn.change_gc(
            gc,
            &ChangeGCAux::new()
                .foreground(0x0047_5569)
                .background(0x00FF_FFFF),
        );
        let _ = conn.image_text8(win, gc, text_x, 42, FALLBACK_HINT_SUBTITLE.as_bytes());
        let _ = conn.flush();
    };

    conn.map_window(win)?;
    draw(&conn);

    let deadline = Instant::now() + Duration::from_millis(3500);
    while Instant::now() < deadline {
        match conn.poll_for_event() {
            Ok(Some(Event::Expose(_))) => draw(&conn),
            Ok(Some(_)) => {}
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(_) => break,
        }
    }

    let _ = conn.free_gc(gc);
    if font_opened {
        let _ = conn.close_font(font_id);
    }
    let _ = conn.unmap_window(win);
    let _ = conn.destroy_window(win);
    let _ = conn.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    #[ignore = "requires an active X11 DISPLAY (e.g. Xvfb :99)"]
    fn live_x11_dnd_xdnd_v5_and_fallback_smoke() {
        let dnd = start_x11_dnd().expect("start_x11_dnd");
        let tmp_dir = std::env::temp_dir().join(format!("glide-x11-dnd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp_dir);
        let sample = tmp_dir.join("hello world äöü.txt");
        std::fs::write(&sample, b"xdnd v5 payload").unwrap();

        // -----------------------------------------------------------------
        // 1. Live XDND v5 drop into an XdndAware target window
        // -----------------------------------------------------------------
        let (tx_received, rx_received) = mpsc::channel::<Vec<PathBuf>>();
        let (tx_ready, rx_ready) = mpsc::channel::<(Window, i16, i16)>();
        let target_thread = thread::spawn(move || {
            let (conn, screen_num) = RustConnection::connect(None).expect("target X11 connect");
            let root = conn.setup().roots[screen_num].root;
            let atoms = XdndAtoms::intern(&conn).expect("intern atoms");
            let prop_recv = conn
                .intern_atom(false, b"GLIDE_TEST_RECV")
                .unwrap()
                .reply()
                .unwrap()
                .atom;

            let win = conn.generate_id().unwrap();
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                root,
                400,
                300,
                300,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            )
            .unwrap();
            conn.change_property32(
                PropMode::REPLACE,
                win,
                atoms.xdnd_aware,
                AtomEnum::ATOM,
                &[XDND_VERSION],
            )
            .unwrap();
            conn.map_window(win).unwrap();
            conn.flush().unwrap();
            let _ = tx_ready.send((win, 520, 400));

            let mut src_win: Window = 0;
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                match conn.poll_for_event().unwrap() {
                    Some(Event::ClientMessage(cm)) if cm.window == win => {
                        let data = cm.data.as_data32();
                        if cm.type_ == atoms.xdnd_enter {
                            src_win = data[0];
                        } else if cm.type_ == atoms.xdnd_position {
                            src_win = data[0];
                            let status = ClientMessageEvent::new(
                                32,
                                src_win,
                                atoms.xdnd_status,
                                [win, 1, 0, 0, atoms.xdnd_action_copy],
                            );
                            conn.send_event(false, src_win, EventMask::NO_EVENT, status)
                                .unwrap();
                            conn.flush().unwrap();
                        } else if cm.type_ == atoms.xdnd_drop {
                            let ts = data[2];
                            conn.convert_selection(
                                win,
                                atoms.xdnd_selection,
                                atoms.text_uri_list,
                                prop_recv,
                                ts,
                            )
                            .unwrap();
                            conn.flush().unwrap();
                        }
                    }
                    Some(Event::SelectionNotify(sn)) if sn.requestor == win => {
                        let mut paths = Vec::new();
                        if sn.property != x11rb::NONE {
                            let reply = conn
                                .get_property(
                                    true,
                                    win,
                                    sn.property,
                                    AtomEnum::ANY,
                                    0,
                                    64 * 1024,
                                )
                                .unwrap()
                                .reply()
                                .unwrap();
                            paths = parse_uri_list(&reply.value);
                        }
                        if src_win != 0 {
                            let finished = ClientMessageEvent::new(
                                32,
                                src_win,
                                atoms.xdnd_finished,
                                [win, 1, atoms.xdnd_action_copy, 0, 0],
                            );
                            conn.send_event(false, src_win, EventMask::NO_EVENT, finished)
                                .unwrap();
                            conn.flush().unwrap();
                        }
                        let _ = tx_received.send(paths);
                        break;
                    }
                    Some(_) => {}
                    None => thread::sleep(Duration::from_millis(5)),
                }
            }
            let _ = conn.destroy_window(win);
            let _ = conn.flush();
        });

        let (_target_win, tx, ty) = rx_ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let (warp_conn, warp_screen) = RustConnection::connect(None).unwrap();
        let root = warp_conn.setup().roots[warp_screen].root;
        warp_conn
            .warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, tx, ty)
            .unwrap();
        warp_conn.flush().unwrap();

        let outcome = dnd.deliver_drop(vec![sample.clone()]).expect("deliver_drop");
        assert_eq!(
            outcome,
            DropDeliveryOutcome::DroppedIntoTarget,
            "expected direct XDND v5 drop into XdndAware target window"
        );
        let received = rx_received
            .recv_timeout(Duration::from_secs(2))
            .expect("target window should receive XDND URI list");
        assert_eq!(received, vec![sample.clone()]);
        target_thread.join().unwrap();

        // -----------------------------------------------------------------
        // 2. Fallback path on non-XdndAware surface (clipboard + hint popup)
        // -----------------------------------------------------------------
        warp_conn
            .warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 120, 120)
            .unwrap();
        warp_conn.flush().unwrap();

        let fb_outcome = dnd
            .deliver_drop(vec![sample.clone()])
            .expect("deliver_drop fallback");
        assert_eq!(
            fb_outcome,
            DropDeliveryOutcome::ClipboardFallback,
            "expected clipboard fallback on non-XdndAware root window"
        );

        // Verify CLIPBOARD offers text/uri-list (and NOT UTF8_STRING)
        let atoms = XdndAtoms::intern(&warp_conn).unwrap();
        let req_win = warp_conn.generate_id().unwrap();
        warp_conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                req_win,
                root,
                -10,
                -10,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap();
        let prop_clip = warp_conn
            .intern_atom(false, b"GLIDE_TEST_CLIP")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        warp_conn
            .convert_selection(
                req_win,
                atoms.clipboard,
                atoms.text_uri_list,
                prop_clip,
                x11rb::CURRENT_TIME,
            )
            .unwrap();
        warp_conn.flush().unwrap();

        let mut clip_paths = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match warp_conn.poll_for_event().unwrap() {
                Some(Event::SelectionNotify(sn)) if sn.requestor == req_win => {
                    if sn.property != x11rb::NONE {
                        let reply = warp_conn
                            .get_property(true, req_win, sn.property, AtomEnum::ANY, 0, 64 * 1024)
                            .unwrap()
                            .reply()
                            .unwrap();
                        clip_paths = parse_uri_list(&reply.value);
                    }
                    break;
                }
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(5)),
            }
        }
        assert_eq!(clip_paths, vec![sample.clone()]);
        let _ = warp_conn.destroy_window(req_win);

        // -----------------------------------------------------------------
        // 3. Edge-strip drag-out: arm Edge::Right and deliver XDND enter/pos
        // -----------------------------------------------------------------
        dnd.set_active_edges(&[Edge::Right], 24);
        dnd.set_drag_armed(true);
        thread::sleep(Duration::from_millis(60));

        // Find the mapped 1px XdndAware strip at the right edge (x = max_x - 1, y = height / 2)
        let geom = query_desktop_geometry(&warp_conn, warp_screen).unwrap();
        let (_min_x, min_y, max_x, max_y) = geom.bounds();
        let sx = (max_x - 1) as i16;
        let sy = ((min_y + max_y) / 2) as i16;
        let strip_win = find_xdnd_target(&warp_conn, root, &atoms, sx, sy, &[])
            .expect("expected armed right-edge XdndAware strip window")
            .0;

        let src_win = warp_conn.generate_id().unwrap();
        warp_conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                src_win,
                root,
                -10,
                -10,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap();
        warp_conn
            .set_selection_owner(src_win, atoms.xdnd_selection, x11rb::CURRENT_TIME)
            .unwrap();

        let enter = ClientMessageEvent::new(
            32,
            strip_win,
            atoms.xdnd_enter,
            [src_win, XDND_VERSION << 24, atoms.text_uri_list, 0, 0],
        );
        warp_conn
            .send_event(false, strip_win, EventMask::NO_EVENT, enter)
            .unwrap();
        let pos_xy = (((sx as u32) & 0xFFFF) << 16) | ((sy as u32) & 0xFFFF);
        let pos = ClientMessageEvent::new(
            32,
            strip_win,
            atoms.xdnd_position,
            [
                src_win,
                0,
                pos_xy,
                x11rb::CURRENT_TIME,
                atoms.xdnd_action_copy,
            ],
        );
        warp_conn
            .send_event(false, strip_win, EventMask::NO_EVENT, pos)
            .unwrap();
        warp_conn.flush().unwrap();

        // Serve SelectionRequest when the edge strip asks for text/uri-list
        let uri_payload = format_uri_list(std::slice::from_ref(&sample));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut staged = None;
        while Instant::now() < deadline {
            if let Ok(Some(Event::SelectionRequest(req))) = warp_conn.poll_for_event() {
                if req.selection == atoms.xdnd_selection && req.target == atoms.text_uri_list {
                    let prop = if req.property == x11rb::NONE {
                        req.target
                    } else {
                        req.property
                    };
                    warp_conn
                        .change_property8(
                            PropMode::REPLACE,
                            req.requestor,
                            prop,
                            req.target,
                            uri_payload.as_bytes(),
                        )
                        .unwrap();
                    let notify = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: req.time,
                        requestor: req.requestor,
                        selection: req.selection,
                        target: req.target,
                        property: prop,
                    };
                    warp_conn
                        .send_event(false, req.requestor, EventMask::NO_EVENT, notify)
                        .unwrap();
                    warp_conn.flush().unwrap();
                }
            }
            if let Some(paths) = dnd.take_staged_files(Edge::Right) {
                staged = Some(paths);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            staged,
            Some(vec![sample.clone()]),
            "expected edge strip to stage dragged file paths from XdndSelection"
        );

        let _ = warp_conn.destroy_window(src_win);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    #[ignore = "requires an active X11 DISPLAY (e.g. Xvfb :99)"]
    fn live_x11_dnd_backtracking_and_live_hover_drop() {
        let dnd = start_x11_dnd().expect("start_x11_dnd");
        let tmp_dir = std::env::temp_dir().join(format!("glide-x11-live-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp_dir);
        let sample = tmp_dir.join("magic-drop.pdf");
        std::fs::write(&sample, b"%PDF-1.7 live test").unwrap();

        let (conn, screen_num) = RustConnection::connect(None).expect("X11 connect");
        let root = conn.setup().roots[screen_num].root;
        let atoms = XdndAtoms::intern(&conn).expect("intern atoms");

        // 1. Target XdndAware window at (200..600, 200..500)
        let target_win = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            target_win,
            root,
            200,
            200,
            400,
            300,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap();
        conn.change_property32(
            PropMode::REPLACE,
            target_win,
            atoms.xdnd_aware,
            AtomEnum::ATOM,
            &[XDND_VERSION],
        )
        .unwrap();
        conn.map_window(target_win).unwrap();

        // 2. Stack BOTH an InputOnly guard window AND an override_redirect shadow overlay
        //    directly on top of `target_win` — backtracking MUST still find `target_win`!
        let guard_win = conn.generate_id().unwrap();
        conn.create_window(
            0,
            guard_win,
            root,
            0,
            0,
            1000,
            800,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .unwrap();
        conn.map_window(guard_win).unwrap();

        let overlay_win = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            overlay_win,
            root,
            150,
            150,
            500,
            400,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .unwrap();
        conn.map_window(overlay_win).unwrap();
        conn.flush().unwrap();

        let found = find_xdnd_target(&conn, root, &atoms, 350, 320, &[]);
        assert_eq!(
            found.map(|t| t.0),
            Some(target_win),
            "find_xdnd_target must backtrack past InputOnly guard & override_redirect overlay"
        );

        // 3. Stack a real WM-managed (WM_STATE) non-XdndAware app window on part of target_win:
        //    over that opaque window we must NOT drop through to target_win underneath!
        let blocker_win = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            blocker_win,
            root,
            220,
            220,
            120,
            120,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap();
        conn.change_property32(
            PropMode::REPLACE,
            blocker_win,
            atoms.wm_state,
            atoms.wm_state,
            &[1u32, 0u32],
        )
        .unwrap();
        conn.map_window(blocker_win).unwrap();
        conn.flush().unwrap();

        assert_eq!(
            find_xdnd_target(&conn, root, &atoms, 260, 260, &[]),
            None,
            "must NOT drop through an opaque WM_STATE non-XdndAware app window"
        );
        assert_eq!(
            find_xdnd_target(&conn, root, &atoms, 450, 350, &[]).map(|t| t.0),
            Some(target_win),
            "unblocked region of target_win must still be found"
        );
        let _ = conn.destroy_window(blocker_win);
        conn.flush().unwrap();

        // 4. Live hover preview + live XdndEnter/XdndPosition -> drop requesting gnome-copied-files
        let (tx_drop, rx_drop) = mpsc::channel::<(bool, String)>();
        let prop_recv = conn
            .intern_atom(false, b"GLIDE_TEST_GNOME")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let sample_clone = sample.clone();
        let target_loop = thread::spawn(move || {
            let mut saw_live_hover = false;
            let mut src_win: Window = 0;
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                match conn.poll_for_event().unwrap() {
                    Some(Event::ClientMessage(cm)) if cm.window == target_win => {
                        let d = cm.data.as_data32();
                        if cm.type_ == atoms.xdnd_enter {
                            src_win = d[0];
                            // Verify both text/uri-list and gnome-copied-files are advertised!
                            assert!(d[2..5].contains(&atoms.text_uri_list));
                            assert!(d[2..5].contains(&atoms.gnome_copied_files));
                        } else if cm.type_ == atoms.xdnd_position {
                            src_win = d[0];
                            saw_live_hover = true;
                            let status = ClientMessageEvent::new(
                                32,
                                src_win,
                                atoms.xdnd_status,
                                [target_win, 1, 0, 0, atoms.xdnd_action_copy],
                            );
                            conn.send_event(false, src_win, EventMask::NO_EVENT, status)
                                .unwrap();
                            conn.flush().unwrap();
                        } else if cm.type_ == atoms.xdnd_drop {
                            let ts = d[2];
                            conn.convert_selection(
                                target_win,
                                atoms.xdnd_selection,
                                atoms.gnome_copied_files,
                                prop_recv,
                                ts,
                            )
                            .unwrap();
                            conn.flush().unwrap();
                        }
                    }
                    Some(Event::SelectionNotify(sn)) if sn.requestor == target_win => {
                        let reply = conn
                            .get_property(
                                true,
                                target_win,
                                sn.property,
                                AtomEnum::ANY,
                                0,
                                64 * 1024,
                            )
                            .unwrap()
                            .reply()
                            .unwrap();
                        let text = String::from_utf8_lossy(&reply.value).to_string();
                        if src_win != 0 {
                            let fin = ClientMessageEvent::new(
                                32,
                                src_win,
                                atoms.xdnd_finished,
                                [target_win, 1, atoms.xdnd_action_copy, 0, 0],
                            );
                            conn.send_event(false, src_win, EventMask::NO_EVENT, fin)
                                .unwrap();
                            conn.flush().unwrap();
                        }
                        let _ = tx_drop.send((saw_live_hover, text));
                        break;
                    }
                    Some(_) => {}
                    None => thread::sleep(Duration::from_millis(5)),
                }
            }
            let _ = conn.destroy_window(overlay_win);
            let _ = conn.destroy_window(guard_win);
            let _ = conn.destroy_window(target_win);
            let _ = conn.flush();
        });

        let (wconn, wscreen) = RustConnection::connect(None).unwrap();
        let wroot = wconn.setup().roots[wscreen].root;
        wconn
            .warp_pointer(x11rb::NONE, wroot, 0, 0, 0, 0, 420, 330)
            .unwrap();
        wconn.flush().unwrap();

        dnd.on_incoming_drag_start(&["magic-drop.pdf".into()], 420, 330);
        dnd.on_incoming_drag_files_ready(vec![sample_clone.clone()]);
        thread::sleep(Duration::from_millis(80));
        dnd.on_incoming_drag_move(440, 340);
        wconn
            .warp_pointer(x11rb::NONE, wroot, 0, 0, 0, 0, 440, 340)
            .unwrap();
        wconn.flush().unwrap();
        thread::sleep(Duration::from_millis(80));

        let outcome = dnd.deliver_drop(vec![sample_clone]).expect("live deliver_drop");
        assert_eq!(outcome, DropDeliveryOutcome::DroppedIntoTarget);

        let (saw_hover, gnome_payload) = rx_drop.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(saw_hover, "target should have received live XdndPosition during hover");
        assert!(
            gnome_payload.starts_with("copy\nfile://"),
            "expected x-special/gnome-copied-files payload, got: {gnome_payload:?}"
        );
        assert!(gnome_payload.contains("magic-drop.pdf"));
        target_loop.join().unwrap();
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

