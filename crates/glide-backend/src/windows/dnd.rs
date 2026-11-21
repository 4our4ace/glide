//! Pure-Rust Windows 11 cross-edge file drag-and-drop backend via Win32 COM/OLE (`windows-sys`).
//!
//! - **Drag-out (Windows -> Peer):** Registers pure-Rust COM `IDropTarget` instances on
//!   invisible 4px topmost layered edge-strip windows (`WS_EX_LAYERED | WS_EX_TOPMOST |
//!   WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`) placed along active outer edges (inset by
//!   `dead_corner_px`). Strips remain hidden (`SW_HIDE`) at all times except while a local
//!   left-button drag is armed so normal clicks, scrollbars, and window resizing are never
//!   obstructed. Extracts dragged local paths from `CF_HDROP` (`DragQueryFileW`).
//! - **Drag-in (Peer -> Windows):**
//!   1. Immediately displays a click-through floating Windows-11-style file preview badge
//!      following the cursor at 120Hz when `DragStart` arrives (before files finish streaming).
//!   2. As soon as files finish saving to `%USERPROFILE%\Downloads\Glide\`, builds a native
//!      Windows Shell `IDataObject` (`SHCreateDataObject` + `IDragSourceHelper2` with
//!      `DSH_ALLOWDROPDESCRIPTIONTEXT`, falling back to a `SetData`-capable `CF_HDROP` +
//!      `Preferred DropEffect` `FileDataObject`), acquires real foreground capture via an
//!      invisible full virtual-desktop starter click, and runs **live `SHDoDragDrop`** so Windows 11 File
//!      Explorer highlights folders live, shows `"+ Copy to <Folder>"` tooltips, and drops
//!      files directly into the hovered folder, Desktop, Chrome, Discord, or VS Code.
//!   3. Automatically falls back to `CF_HDROP` on the Windows clipboard + cursor hint card
//!      if the target window rejects or blocks OLE drops (e.g. non-drop app or elevated UIPI).

use crate::dnd::{
    clamp_hint_popup_pos, compute_edge_strip_rects, format_drag_extension_badge,
    format_drag_preview_label, should_defer_live_ole_drag, DndBackend, DropDeliveryOutcome,
    EdgeStripRect, StagedEdgeTracker,
};
use crate::geometry::DesktopGeometry;
use crate::windows::{init_dpi_awareness, query_desktop_geometry, GLIDE_EXTRA_INFO};
use anyhow::{anyhow, Context, Result};
use glide_proto::{Edge, MAX_DRAG_FILES};
use parking_lot::Mutex;
use std::ffi::{c_void, OsString};
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{
    GlobalFree, BOOL, DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
    DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, DV_E_TYMED, E_INVALIDARG, E_NOINTERFACE,
    E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT,
    OLE_E_ADVISENOTSUPPORTED, POINT, POINTL, RECT, SIZE, S_FALSE, S_OK, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateRoundRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetDC, InvalidateRect, ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor,
    SetWindowRgn, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_SWISS, HBITMAP,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CLSCTX_INPROC_SERVER, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM,
    STGMEDIUM_0, TYMED_HGLOBAL, TYMED_NULL,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GHND,
};
use windows_sys::Win32::System::Ole::{
    OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP,
    DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEINPUT,
    VK_ESCAPE, VK_LBUTTON,
};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{
    DragQueryFileW, ILCreateFromPathW, ILFindLastID, ILFree, SHCreateDataObject, SHDoDragDrop,
    CLSID_DragDropHelper, DROPFILES, HDROP, SHDRAGIMAGE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetMessageW, GetSystemMetrics, GetWindowLongPtrW, KillTimer, PeekMessageW, PostMessageW,
    RegisterClassW, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, GWLP_USERDATA, GWL_EXSTYLE, HWND_TOPMOST, LWA_ALPHA, MSG,
    PM_REMOVE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW,
    SW_SHOWNOACTIVATE, WM_APP, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND, WM_LBUTTONDOWN,
    WM_PAINT, WM_QUIT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_OVERLAPPED, WS_POPUP,
};

const WM_APP_SYNC_STRIPS: u32 = WM_APP + 21;
const WM_APP_HIDE_STRIPS: u32 = WM_APP + 22;
const WM_APP_PREVIEW_SHOW: u32 = WM_APP + 23;
const WM_APP_PREVIEW_HIDE: u32 = WM_APP + 24;
const WM_APP_PREVIEW_MOVE: u32 = WM_APP + 25;
const WM_APP_DISPLAY_CHANGED: u32 = WM_APP + 26;

const PREVIEW_TIMER_ID: usize = 7;
const MK_LBUTTON: u32 = 0x0001;
const DSH_ALLOWDROPDESCRIPTIONTEXT: u32 = 0x0001;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x0000_0000_0000_0000_c000_0000_0000_0046);
const IID_IDROPTARGET: GUID = GUID::from_u128(0x0000_0122_0000_0000_c000_0000_0000_0046);
const IID_IDROPSOURCE: GUID = GUID::from_u128(0x0000_0121_0000_0000_c000_0000_0000_0046);
const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000_010e_0000_0000_c000_0000_0000_0046);
const IID_IENUMFORMATETC: GUID = GUID::from_u128(0x0000_0103_0000_0000_c000_0000_0000_0046);
const IID_IDRAGSOURCEHELPER: GUID = GUID::from_u128(0xde5b_f786_477a_11d2_839d_00c0_4fd9_18d0);
const IID_IDRAGSOURCEHELPER2: GUID = GUID::from_u128(0x83e0_7d0d_0c5f_4163_bf1a_60b2_7405_1e40);

const STAGED_MAX_AGE: Duration = Duration::from_secs(4);
const STAGED_LEAVE_GRACE: Duration = Duration::from_millis(350);

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

// ---------------------------------------------------------------------------
// Shared state between DndBackend handle, STA strip/preview thread, and live OLE thread
// ---------------------------------------------------------------------------

struct StagedDrag {
    edge: Edge,
    paths: Vec<PathBuf>,
    staged_at: Instant,
    left_at: Option<Instant>,
}

#[derive(Default, Clone)]
struct PreviewState {
    visible: bool,
    label: String,
    badge: String,
    ready: bool,
    cursor_x: i32,
    cursor_y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionResult {
    Outcome(DropDeliveryOutcome),
    Cancelled,
    LiveDeferred,
}

struct LiveOleSession {
    drop_requested: AtomicBool,
    cancel_requested: AtomicBool,
    synthesized_lmb_down: AtomicBool,
    done: AtomicBool,
    result_rx: Mutex<Option<std::sync::mpsc::Receiver<Result<SessionResult>>>>,
}

impl LiveOleSession {
    fn new(rx: std::sync::mpsc::Receiver<Result<SessionResult>>) -> Self {
        Self {
            drop_requested: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            synthesized_lmb_down: AtomicBool::new(false),
            done: AtomicBool::new(false),
            result_rx: Mutex::new(Some(rx)),
        }
    }
}

struct SharedDnd {
    enabled: AtomicBool,
    armed: AtomicBool,
    dead_corner_px: AtomicI32,
    ctrl_hwnd: AtomicIsize,
    active_edges: Mutex<Vec<Edge>>,
    staged: Mutex<Option<StagedDrag>>,
    staged_tracker: StagedEdgeTracker,
    preview: Mutex<PreviewState>,
    live_session: Mutex<Option<Arc<LiveOleSession>>>,
}

impl SharedDnd {
    fn clear_staged(&self) {
        let mut guard = self.staged.lock();
        *guard = None;
        self.staged_tracker.clear();
    }

    fn post_ctrl(&self, msg: u32) {
        let hwnd = self.ctrl_hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, msg, 0, 0);
            }
        }
    }

    fn post_hide_strips(&self) {
        self.post_ctrl(WM_APP_HIDE_STRIPS);
    }

    fn post_preview_show(&self) {
        self.post_ctrl(WM_APP_PREVIEW_SHOW);
    }

    fn post_preview_hide(&self) {
        self.preview.lock().visible = false;
        self.post_ctrl(WM_APP_PREVIEW_HIDE);
    }

    fn post_preview_move(&self) {
        self.post_ctrl(WM_APP_PREVIEW_MOVE);
    }
}

pub struct WindowsDndBackend {
    shared: Arc<SharedDnd>,
}

impl WindowsDndBackend {
    fn post_sync(&self) {
        self.shared.post_ctrl(WM_APP_SYNC_STRIPS);
    }

    fn cancel_live_session_inner(&self) {
        if let Some(session) = self.shared.live_session.lock().take() {
            session.cancel_requested.store(true, Ordering::Release);
            if !session.done.load(Ordering::Acquire) {
                if session.synthesized_lmb_down.load(Ordering::Acquire)
                    && (unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000) != 0
                {
                    send_mouse_button_input(MOUSEEVENTF_LEFTUP);
                }
                send_synthetic_escape();
            }
        }
    }
}

impl Drop for WindowsDndBackend {
    fn drop(&mut self) {
        self.cancel_live_session_inner();
        let hwnd = self.shared.ctrl_hwnd.swap(0, Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_QUIT, 0, 0);
            }
        }
    }
}

impl DndBackend for WindowsDndBackend {
    fn set_enabled(&self, enabled: bool) {
        self.shared.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.shared.armed.store(false, Ordering::SeqCst);
            self.shared.clear_staged();
        }
        self.post_sync();
    }

    fn set_active_edges(&self, edges: &[Edge], dead_corner_px: i32) {
        *self.shared.active_edges.lock() = edges.to_vec();
        self.shared
            .dead_corner_px
            .store(dead_corner_px.max(0), Ordering::Relaxed);
        self.post_sync();
    }

    fn set_drag_armed(&self, armed: bool) {
        let effective = armed && self.shared.enabled.load(Ordering::Acquire);
        let prev = self.shared.armed.swap(effective, Ordering::SeqCst);
        if !effective {
            self.shared.clear_staged();
        }
        if prev != effective {
            if effective {
                self.post_sync();
            } else {
                self.shared.post_hide_strips();
            }
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
        // 1. Disarm and clear staged state under the staged lock (keep the 4px strip visible
        //    until AFTER LEFTUP so if Explorer ignores/delays Escape, releasing VK_LBUTTON
        //    lands on our EdgeDropTarget returning DROPEFFECT_NONE instead of dropping/moving
        //    files into whatever local window sits underneath the screen edge).
        self.shared.armed.store(false, Ordering::SeqCst);
        self.shared.clear_staged();
        // 2. Send a synthetic Escape tap (both VK_ESCAPE + scancode, tagged GLIDE_EXTRA_INFO)
        //    so Explorer's modal DoDragDrop loop cancels cleanly without moving/deleting files
        //    and our LL keyboard hook ignores the injected key.
        send_synthetic_escape();
        // 3. Give Explorer a brief moment to process VK_ESCAPE and exit DoDragDrop cleanly.
        thread::sleep(Duration::from_millis(20));
        // 4. Release Windows' kernel VK_LBUTTON if still down (tagged GLIDE_EXTRA_INFO so
        //    mouse_ll_proc ignores it and HookState.lmb_down stays `true` for the remote drag).
        if (unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000) != 0 {
            send_mouse_button_input(MOUSEEVENTF_LEFTUP);
        }
        // 5. Now hide the edge strips.
        self.shared.post_hide_strips();
    }

    fn supports_live_os_drag(&self) -> bool {
        true
    }

    fn on_incoming_drag_start(&self, filenames: &[String], x: i32, y: i32) {
        self.cancel_live_session_inner();
        {
            let mut p = self.shared.preview.lock();
            p.visible = true;
            p.label = format_drag_preview_label(filenames);
            p.badge = format_drag_extension_badge(filenames);
            p.ready = false;
            p.cursor_x = x;
            p.cursor_y = y;
        }
        self.shared.post_preview_show();
    }

    fn on_incoming_drag_files_ready(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.cancel_live_session_inner();
        {
            let mut p = self.shared.preview.lock();
            p.ready = true;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let session = Arc::new(LiveOleSession::new(rx));
        *self.shared.live_session.lock() = Some(Arc::clone(&session));

        let shared_clone = Arc::clone(&self.shared);
        let session_clone = Arc::clone(&session);
        if let Err(e) = thread::Builder::new()
            .name("glide-win-ole-live".into())
            .spawn(move || {
                let res = run_windows_ole_drag_session(
                    &paths,
                    DragRunMode::Live,
                    &session_clone,
                    &shared_clone,
                );
                let _ = tx.send(res);
            })
        {
            warn!(error = %e, "Failed spawning live Windows OLE drag thread");
            *self.shared.live_session.lock() = None;
        }
    }

    fn on_incoming_drag_move(&self, x: i32, y: i32) {
        let mut p = self.shared.preview.lock();
        if p.visible {
            p.cursor_x = x;
            p.cursor_y = y;
            drop(p);
            self.shared.post_preview_move();
        }
    }

    fn on_incoming_drag_cancel(&self) {
        // Detach and cancel any live session BEFORE hiding the preview so a concurrent
        // LiveDeferred exit cannot re-show the preview after we hide it.
        self.cancel_live_session_inner();
        self.shared.post_preview_hide();
    }

    fn deliver_drop(&self, paths: Vec<PathBuf>) -> Result<DropDeliveryOutcome> {
        self.shared.post_preview_hide();
        if paths.is_empty() {
            self.cancel_live_session_inner();
            return Ok(DropDeliveryOutcome::ClipboardFallback);
        }

        // If a live SHDoDragDrop session is already running while the user moved over Explorer,
        // complete it right where the cursor is!
        if let Some(session) = self.shared.live_session.lock().take() {
            session.drop_requested.store(true, Ordering::Release);
            if !session.done.load(Ordering::Acquire) {
                if session.synthesized_lmb_down.load(Ordering::Acquire)
                    && (unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000) != 0
                {
                    send_mouse_button_input(MOUSEEVENTF_LEFTUP);
                }
                // Multi-step backup wiggle loop in case physical LMB was already up or cursor is
                // stationary during a fast flick-drop (spans past QueryContinueDrag's 260ms
                // threshold so SHDoDragDrop can never stall waiting for a message).
                let nudge_session = Arc::clone(&session);
                thread::spawn(move || {
                    for delay in [25u64, 60, 110, 120] {
                        thread::sleep(Duration::from_millis(delay));
                        if nudge_session.done.load(Ordering::Acquire) {
                            return;
                        }
                        send_mouse_wiggle();
                    }
                });
            }
            if let Some(rx) = session.result_rx.lock().take() {
                match rx.recv_timeout(Duration::from_secs(6)) {
                    Ok(Ok(SessionResult::Outcome(outcome))) => {
                        self.shared.post_preview_hide();
                        return Ok(outcome);
                    }
                    Ok(Ok(SessionResult::Cancelled)) => {
                        self.shared.post_preview_hide();
                        debug!("Live Windows OLE drag was explicitly cancelled by user");
                        return Ok(DropDeliveryOutcome::DroppedIntoTarget);
                    }
                    Ok(Ok(SessionResult::LiveDeferred)) => {
                        debug!(
                            "Live Windows OLE drag deferred; performing ImmediatePostRelease drop at target cursor"
                        );
                    }
                    Ok(Err(e)) => {
                        self.shared.post_preview_hide();
                        return Err(e);
                    }
                    Err(_) => {
                        warn!("Timed out waiting for live Windows OLE drop; using clipboard fallback");
                        session.cancel_requested.store(true, Ordering::Release);
                        send_synthetic_escape();
                        self.shared.post_preview_hide();
                        set_clipboard_hdrop(&paths)?;
                        spawn_windows_fallback_hint();
                        return Ok(DropDeliveryOutcome::ClipboardFallback);
                    }
                }
            }
        }

        // Ensure floating preview is hidden before ImmediatePostRelease runs at the target cursor.
        self.shared.post_preview_hide();

        // Post-release drop path (e.g. large/slow file that finished transferring after LMB up,
        // or a Live session that deferred early): run SHDoDragDrop on a dedicated STA thread with
        // real foreground capture & wiggle at the user's real drop coordinate.
        let paths_clone = paths.clone();
        let shared_clone = Arc::clone(&self.shared);
        let (_, dummy_rx) = std::sync::mpsc::channel();
        let session = Arc::new(LiveOleSession::new(dummy_rx));
        let outcome = thread::Builder::new()
            .name("glide-win-ole-drop".into())
            .spawn(move || {
                run_windows_ole_drag_session(
                    &paths_clone,
                    DragRunMode::ImmediatePostRelease,
                    &session,
                    &shared_clone,
                )
            })
            .context("failed to spawn Windows OLE drop thread")?
            .join()
            .map_err(|_| anyhow!("Windows OLE drop thread panicked"))??;

        self.shared.post_preview_hide();
        match outcome {
            SessionResult::Outcome(o) => Ok(o),
            SessionResult::Cancelled | SessionResult::LiveDeferred => {
                Ok(DropDeliveryOutcome::ClipboardFallback)
            }
        }
    }
}

/// Starts the Windows 11 COM/OLE cross-edge drag-and-drop backend.
pub fn start_windows_dnd() -> Result<Arc<dyn DndBackend>> {
    init_dpi_awareness();
    let shared = Arc::new(SharedDnd {
        enabled: AtomicBool::new(true),
        armed: AtomicBool::new(false),
        dead_corner_px: AtomicI32::new(24),
        ctrl_hwnd: AtomicIsize::new(0),
        active_edges: Mutex::new(Vec::new()),
        staged: Mutex::new(None),
        staged_tracker: StagedEdgeTracker::new(STAGED_MAX_AGE, STAGED_LEAVE_GRACE),
        preview: Mutex::new(PreviewState::default()),
        live_session: Mutex::new(None),
    });

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<isize>>();
    let thread_shared = Arc::clone(&shared);

    thread::Builder::new()
        .name("glide-win-dnd".into())
        .spawn(move || {
            run_ole_strip_thread(thread_shared, ready_tx);
        })
        .context("failed to spawn glide-win-dnd thread")?;

    let ctrl_hwnd = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .context("timed out waiting for Windows DnD thread")??;
    shared.ctrl_hwnd.store(ctrl_hwnd, Ordering::Release);

    Ok(Arc::new(WindowsDndBackend { shared }))
}

// ---------------------------------------------------------------------------
// STA thread hosting invisible 4px topmost edge-strip IDropTarget windows
// + click-through floating drag preview card
// ---------------------------------------------------------------------------

struct StripWindow {
    hwnd: HWND,
    rect: EdgeStripRect,
    visible: bool,
}

struct PreviewWindow {
    hwnd: HWND,
    width: i32,
    height: i32,
    visible: bool,
    last_pos: (i32, i32),
}

struct StripThreadCtx {
    shared: Arc<SharedDnd>,
    ctrl_hwnd: HWND,
    strip_class: Vec<u16>,
    preview_class: Vec<u16>,
    cached_geom: DesktopGeometry,
    last_edges: Vec<Edge>,
    last_dead_corner: i32,
    strips: Vec<StripWindow>,
    preview_win: Option<PreviewWindow>,
}

unsafe extern "system" fn strip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn ctrl_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DISPLAYCHANGE || msg == WM_DPICHANGED {
        unsafe {
            PostMessageW(hwnd, WM_APP_DISPLAY_CHANGED, 0, 0);
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn preview_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            if !hdc.is_null() {
                let mut rc = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                unsafe {
                    GetClientRect(hwnd, &mut rc);
                }
                let w = (rc.right - rc.left).max(1);
                let h = (rc.bottom - rc.top).max(1);
                let scale = (h as f32 / 52.0).clamp(0.85, 3.0);
                let px = |v: f32| (v * scale).round() as i32;

                let (label, badge, ready) = {
                    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const SharedDnd;
                    if raw.is_null() {
                        ("1 file".to_string(), "FILE".to_string(), false)
                    } else {
                        let shared = unsafe { &*raw };
                        let p = shared.preview.lock();
                        (p.label.clone(), p.badge.clone(), p.ready)
                    }
                };

                // White translucent card (#FFFFFF) + crisp border (#CBD5E1)
                let bg_brush = unsafe { CreateSolidBrush(win_rgb(0xFF, 0xFF, 0xFF)) };
                let border_pen =
                    unsafe { CreatePen(PS_SOLID, px(1.0).max(1), win_rgb(0xCB, 0xD5, 0xE1)) };
                let old_brush = unsafe { SelectObject(hdc, bg_brush) };
                let old_pen = unsafe { SelectObject(hdc, border_pen) };
                let corner = px(18.0);
                unsafe {
                    RoundRect(hdc, 0, 0, w, h, corner, corner);
                }

                // Blue file badge icon on left (#2563EB)
                let icon_w = px(38.0);
                let icon_h = px(32.0);
                let icon_x = px(10.0);
                let icon_y = (h - icon_h) / 2;
                let accent = if ready {
                    win_rgb(0x25, 0x63, 0xEB)
                } else {
                    win_rgb(0x3B, 0x82, 0xF6)
                };
                let badge_brush = unsafe { CreateSolidBrush(accent) };
                let badge_pen = unsafe { CreatePen(PS_SOLID, 1, accent) };
                unsafe {
                    SelectObject(hdc, badge_brush);
                    SelectObject(hdc, badge_pen);
                    RoundRect(
                        hdc,
                        icon_x,
                        icon_y,
                        icon_x + icon_w,
                        icon_y + icon_h,
                        px(10.0),
                        px(10.0),
                    );
                }

                let face = encode_wide("Segoe UI");
                let badge_font = unsafe {
                    CreateFontW(
                        -px(11.0),
                        0,
                        0,
                        0,
                        700,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET as u32,
                        OUT_DEFAULT_PRECIS as u32,
                        CLIP_DEFAULT_PRECIS as u32,
                        CLEARTYPE_QUALITY as u32,
                        (DEFAULT_PITCH | FF_SWISS) as u32,
                        face.as_ptr(),
                    )
                };
                let title_font = unsafe {
                    CreateFontW(
                        -px(13.0),
                        0,
                        0,
                        0,
                        600,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET as u32,
                        OUT_DEFAULT_PRECIS as u32,
                        CLIP_DEFAULT_PRECIS as u32,
                        CLEARTYPE_QUALITY as u32,
                        (DEFAULT_PITCH | FF_SWISS) as u32,
                        face.as_ptr(),
                    )
                };
                let sub_font = unsafe {
                    CreateFontW(
                        -px(11.0),
                        0,
                        0,
                        0,
                        400,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET as u32,
                        OUT_DEFAULT_PRECIS as u32,
                        CLIP_DEFAULT_PRECIS as u32,
                        CLEARTYPE_QUALITY as u32,
                        (DEFAULT_PITCH | FF_SWISS) as u32,
                        face.as_ptr(),
                    )
                };

                unsafe {
                    SetBkMode(hdc, TRANSPARENT as i32);
                }
                let old_font = unsafe { SelectObject(hdc, badge_font) };
                unsafe {
                    SetTextColor(hdc, win_rgb(0xFF, 0xFF, 0xFF));
                }
                let mut badge_rc = RECT {
                    left: icon_x,
                    top: icon_y,
                    right: icon_x + icon_w,
                    bottom: icon_y + icon_h,
                };
                let badge_w = encode_wide(&badge);
                unsafe {
                    DrawTextW(
                        hdc,
                        badge_w.as_ptr(),
                        -1,
                        &mut badge_rc,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }

                let text_x = icon_x + icon_w + px(10.0);
                let text_right = w - px(10.0);
                let title_w = encode_wide(&label);
                let sub_w = encode_wide(if ready {
                    "+ Drop in folder or app"
                } else {
                    "Receiving file..."
                });

                let mut title_rc = RECT {
                    left: text_x,
                    top: px(8.0),
                    right: text_right,
                    bottom: px(27.0),
                };
                unsafe {
                    SelectObject(hdc, title_font);
                    SetTextColor(hdc, win_rgb(0x0F, 0x17, 0x2A));
                    DrawTextW(
                        hdc,
                        title_w.as_ptr(),
                        -1,
                        &mut title_rc,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }

                let mut sub_rc = RECT {
                    left: text_x,
                    top: px(26.0),
                    right: text_right,
                    bottom: h - px(7.0),
                };
                unsafe {
                    SelectObject(hdc, sub_font);
                    SetTextColor(
                        hdc,
                        if ready {
                            win_rgb(0x25, 0x63, 0xEB)
                        } else {
                            win_rgb(0x64, 0x74, 0x8B)
                        },
                    );
                    DrawTextW(
                        hdc,
                        sub_w.as_ptr(),
                        -1,
                        &mut sub_rc,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }

                unsafe {
                    SelectObject(hdc, old_font);
                    SelectObject(hdc, old_brush);
                    SelectObject(hdc, old_pen);
                    DeleteObject(badge_font);
                    DeleteObject(title_font);
                    DeleteObject(sub_font);
                    DeleteObject(badge_brush);
                    DeleteObject(badge_pen);
                    DeleteObject(bg_brush);
                    DeleteObject(border_pen);
                    EndPaint(hwnd, &ps);
                }
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn show_or_update_preview(ctx: &mut StripThreadCtx) {
    let pstate = ctx.shared.preview.lock().clone();
    if !pstate.visible {
        hide_preview(ctx);
        return;
    }

    let mut pt = POINT {
        x: pstate.cursor_x,
        y: pstate.cursor_y,
    };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    if let Ok(g) = query_desktop_geometry() {
        ctx.cached_geom = g;
    }
    let scale = ctx
        .cached_geom
        .monitors
        .iter()
        .find(|m| m.contains(pt.x, pt.y))
        .map(|m| m.scale)
        .unwrap_or_else(|| ctx.cached_geom.primary_scale())
        .clamp(1.0, 3.0);

    let width = (248.0 * scale).round() as i32;
    let height = (52.0 * scale).round() as i32;
    let radius = (18.0 * scale).round() as i32;
    let (x, y) = clamp_hint_popup_pos(&ctx.cached_geom, pt.x, pt.y, width, height);

    if ctx.preview_win.is_none() {
        let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
        let ex_style =
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                ctx.preview_class.as_ptr(),
                ptr::null(),
                WS_POPUP,
                x,
                y,
                width,
                height,
                ptr::null_mut(),
                ptr::null_mut(),
                hinstance,
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            return;
        }
        unsafe {
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                Arc::as_ptr(&ctx.shared) as isize,
            );
            SetLayeredWindowAttributes(hwnd, 0, 238, LWA_ALPHA);
            let rgn = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius, radius);
            if !rgn.is_null() {
                SetWindowRgn(hwnd, rgn, 1);
            }
        }
        ctx.preview_win = Some(PreviewWindow {
            hwnd,
            width,
            height,
            visible: false,
            last_pos: (x, y),
        });
    }

    if let Some(ref mut pw) = ctx.preview_win {
        pw.width = width;
        pw.height = height;
        pw.last_pos = (x, y);
        unsafe {
            SetWindowPos(pw.hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE);
            if !pw.visible {
                ShowWindow(pw.hwnd, SW_SHOWNOACTIVATE);
                pw.visible = true;
                SetTimer(ctx.ctrl_hwnd, PREVIEW_TIMER_ID, 8, None);
            }
            InvalidateRect(pw.hwnd, ptr::null(), 0);
        }
    }
}

fn move_preview_to_cursor(ctx: &mut StripThreadCtx) {
    let Some(ref mut pw) = ctx.preview_win else {
        return;
    };
    if !pw.visible {
        return;
    }
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return;
    }
    let (x, y) = clamp_hint_popup_pos(&ctx.cached_geom, pt.x, pt.y, pw.width, pw.height);
    if (x, y) != pw.last_pos {
        pw.last_pos = (x, y);
        unsafe {
            SetWindowPos(
                pw.hwnd,
                HWND_TOPMOST,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

fn hide_preview(ctx: &mut StripThreadCtx) {
    if let Some(ref mut pw) = ctx.preview_win {
        if pw.visible {
            unsafe {
                KillTimer(ctx.ctrl_hwnd, PREVIEW_TIMER_ID);
                ShowWindow(pw.hwnd, SW_HIDE);
            }
            pw.visible = false;
        }
    }
}

fn destroy_preview(ctx: &mut StripThreadCtx) {
    hide_preview(ctx);
    if let Some(pw) = ctx.preview_win.take() {
        unsafe {
            SetWindowLongPtrW(pw.hwnd, GWLP_USERDATA, 0);
            DestroyWindow(pw.hwnd);
        }
    }
}

fn destroy_all_strips(strips: &mut Vec<StripWindow>) {
    for s in strips.drain(..) {
        unsafe {
            let _ = RevokeDragDrop(s.hwnd);
            DestroyWindow(s.hwnd);
        }
    }
}

fn hide_all_strips(strips: &mut [StripWindow]) {
    for s in strips.iter_mut() {
        if s.visible {
            unsafe {
                ShowWindow(s.hwnd, SW_HIDE);
            }
            s.visible = false;
        }
    }
}

fn sync_strips(ctx: &mut StripThreadCtx) {
    let enabled = ctx.shared.enabled.load(Ordering::Acquire);
    let armed = ctx.shared.armed.load(Ordering::Acquire);
    let edges = ctx.shared.active_edges.lock().clone();
    let dead_corner = ctx.shared.dead_corner_px.load(Ordering::Relaxed);

    if !enabled || edges.is_empty() {
        destroy_all_strips(&mut ctx.strips);
        ctx.last_edges.clear();
        return;
    }

    // Only re-query display geometry when active edges or dead-corner settings actually change.
    // Arming/disarming reuses `ctx.cached_geom` so drag-arm never stalls on display queries.
    let config_changed = ctx.last_edges != edges || ctx.last_dead_corner != dead_corner;
    if config_changed {
        if let Ok(g) = query_desktop_geometry() {
            ctx.cached_geom = g;
        }
        ctx.last_edges = edges.clone();
        ctx.last_dead_corner = dead_corner;
    }

    // 4px width completely covers `hit_outer_edge_guarded(..., margin_px = 2, ...)` (the 3px
    // edge band) with margin to spare so Explorer's COM DragEnter always hits before crossing.
    let desired = compute_edge_strip_rects(&ctx.cached_geom, &edges, dead_corner, 4);

    let needs_rebuild = ctx.strips.len() != desired.len()
        || ctx
            .strips
            .iter()
            .zip(desired.iter())
            .any(|(cur, want)| cur.rect != *want);

    if needs_rebuild {
        destroy_all_strips(&mut ctx.strips);
        let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
        for rect in desired {
            let ex_style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            // Pre-created hidden (WS_VISIBLE omitted) and COM-registered ahead of time so
            // arming only needs a <1ms SetWindowPos(SWP_SHOWWINDOW).
            let hwnd = unsafe {
                CreateWindowExW(
                    ex_style,
                    ctx.strip_class.as_ptr(),
                    ptr::null(),
                    WS_POPUP,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    hinstance,
                    ptr::null(),
                )
            };
            if hwnd.is_null() {
                warn!(edge = ?rect.edge, "CreateWindowExW failed for DnD edge strip");
                continue;
            }
            // Alpha = 1/255: completely invisible visually while staying hit-testable by OLE.
            unsafe {
                SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);
            }

            let target = EdgeDropTarget::new_raw(rect.edge, Arc::clone(&ctx.shared));
            let hr = unsafe { RegisterDragDrop(hwnd, target) };
            // Release our initial ref (OLE added its own ref on success; on error this frees it).
            unsafe {
                com_release(target);
            }
            if hr < 0 {
                warn!(edge = ?rect.edge, hr, "RegisterDragDrop failed on DnD edge strip");
                unsafe {
                    DestroyWindow(hwnd);
                }
                continue;
            }

            ctx.strips.push(StripWindow {
                hwnd,
                rect,
                visible: false,
            });
        }
    }

    if !armed {
        hide_all_strips(&mut ctx.strips);
        return;
    }

    for s in &mut ctx.strips {
        let flags = if s.visible {
            SWP_NOACTIVATE
        } else {
            SWP_SHOWWINDOW | SWP_NOACTIVATE
        };
        unsafe {
            SetWindowPos(
                s.hwnd,
                HWND_TOPMOST,
                s.rect.x,
                s.rect.y,
                s.rect.width,
                s.rect.height,
                flags,
            );
        }
        s.visible = true;
    }
}

fn run_ole_strip_thread(
    shared: Arc<SharedDnd>,
    ready_tx: std::sync::mpsc::Sender<Result<isize>>,
) {
    let hr = unsafe { OleInitialize(ptr::null()) };
    if hr < 0 {
        let _ = ready_tx.send(Err(anyhow!("OleInitialize failed (HRESULT 0x{hr:08X})")));
        return;
    }

    let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
    let ctrl_class: Vec<u16> = "GlideDndCtrlWindow\0".encode_utf16().collect();
    let strip_class: Vec<u16> = "GlideDndEdgeStrip\0".encode_utf16().collect();
    let preview_class: Vec<u16> = "GlideDndPreviewCard\0".encode_utf16().collect();

    let ctrl_wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(ctrl_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: ctrl_class.as_ptr(),
    };
    let strip_wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(strip_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: strip_class.as_ptr(),
    };
    let preview_wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(preview_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: preview_class.as_ptr(),
    };
    unsafe {
        RegisterClassW(&ctrl_wc);
        RegisterClassW(&strip_wc);
        RegisterClassW(&preview_wc);
    }

    // Use a hidden top-level window (NOT HWND_MESSAGE) so Windows delivers WM_DISPLAYCHANGE and
    // WM_DPICHANGED broadcasts to `ctrl_wnd_proc`, allowing us to cache `DesktopGeometry` without
    // polling on every drag arm or 125Hz preview move. WS_VISIBLE is omitted so it stays invisible.
    let ctrl_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            ctrl_class.as_ptr(),
            ctrl_class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        )
    };
    if ctrl_hwnd.is_null() {
        let _ = ready_tx.send(Err(anyhow!("CreateWindowExW failed for DnD control window")));
        unsafe {
            OleUninitialize();
        }
        return;
    }

    let _ = ready_tx.send(Ok(ctrl_hwnd as isize));
    debug!("Windows OLE DnD edge-strip & preview thread ready");

    let mut ctx = StripThreadCtx {
        shared,
        ctrl_hwnd,
        strip_class,
        preview_class,
        cached_geom: query_desktop_geometry().unwrap_or_default(),
        last_edges: Vec::new(),
        last_dead_corner: -1,
        strips: Vec::new(),
        preview_win: None,
    };

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
        if msg.hwnd == ctrl_hwnd {
            match msg.message {
                WM_APP_SYNC_STRIPS => {
                    sync_strips(&mut ctx);
                    continue;
                }
                WM_APP_DISPLAY_CHANGED => {
                    if let Ok(g) = query_desktop_geometry() {
                        ctx.cached_geom = g;
                    }
                    sync_strips(&mut ctx);
                    continue;
                }
                WM_APP_HIDE_STRIPS => {
                    hide_all_strips(&mut ctx.strips);
                    continue;
                }
                WM_APP_PREVIEW_SHOW => {
                    show_or_update_preview(&mut ctx);
                    continue;
                }
                WM_APP_PREVIEW_HIDE => {
                    hide_preview(&mut ctx);
                    continue;
                }
                WM_APP_PREVIEW_MOVE => {
                    move_preview_to_cursor(&mut ctx);
                    continue;
                }
                WM_TIMER if msg.wParam == PREVIEW_TIMER_ID => {
                    move_preview_to_cursor(&mut ctx);
                    continue;
                }
                _ => {}
            }
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    destroy_preview(&mut ctx);
    destroy_all_strips(&mut ctx.strips);
    unsafe {
        DestroyWindow(ctrl_hwnd);
        OleUninitialize();
    }
}

// ---------------------------------------------------------------------------
// Synthetic Escape helper (cancels local Explorer drag cleanly)
// ---------------------------------------------------------------------------

fn send_synthetic_escape() {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_ESCAPE,
                    wScan: 0x01, // Escape scancode
                    dwFlags: 0,
                    time: 0,
                    dwExtraInfo: GLIDE_EXTRA_INFO,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_ESCAPE,
                    wScan: 0x01,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: GLIDE_EXTRA_INFO,
                },
            },
        },
    ];
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
    }
}

// ---------------------------------------------------------------------------
// Pure-Rust COM IUnknown / IDataObject / IDropTarget / IEnumFORMATETC / IDropSource
// ---------------------------------------------------------------------------

#[repr(C)]
struct IUnknownVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

unsafe fn com_release(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    let vtbl = unsafe { *(obj as *const *const IUnknownVtbl) };
    unsafe {
        ((*vtbl).release)(obj);
    }
}

#[repr(C)]
struct IDataObjectVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_data: unsafe extern "system" fn(*mut c_void, *mut FORMATETC, *mut STGMEDIUM) -> HRESULT,
    get_data_here:
        unsafe extern "system" fn(*mut c_void, *mut FORMATETC, *mut STGMEDIUM) -> HRESULT,
    query_get_data: unsafe extern "system" fn(*mut c_void, *mut FORMATETC) -> HRESULT,
    get_canonical_format_etc:
        unsafe extern "system" fn(*mut c_void, *mut FORMATETC, *mut FORMATETC) -> HRESULT,
    set_data:
        unsafe extern "system" fn(*mut c_void, *mut FORMATETC, *mut STGMEDIUM, BOOL) -> HRESULT,
    enum_format_etc:
        unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    d_advise: unsafe extern "system" fn(
        *mut c_void,
        *mut FORMATETC,
        u32,
        *mut c_void,
        *mut u32,
    ) -> HRESULT,
    d_unadvise: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    enum_d_advise: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

/// Calls `IDataObject::GetData(CF_HDROP)` on an incoming drag object and extracts file paths.
unsafe fn extract_hdrop_paths(data_obj: *mut c_void) -> Vec<PathBuf> {
    if data_obj.is_null() {
        return Vec::new();
    }
    let vtbl = unsafe { *(data_obj as *const *const IDataObjectVtbl) };
    let mut fmt = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    };
    let mut medium: STGMEDIUM = unsafe { std::mem::zeroed() };
    let hr = unsafe { ((*vtbl).get_data)(data_obj, &mut fmt, &mut medium) };
    if hr < 0 {
        return Vec::new();
    }

    let mut paths = Vec::new();
    if medium.tymed == TYMED_HGLOBAL as u32 {
        let hglobal = unsafe { medium.u.hGlobal };
        if !hglobal.is_null() {
            let hdrop = hglobal as HDROP;
            let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, ptr::null_mut(), 0) };
            let limit = (count as usize).min(MAX_DRAG_FILES);
            for idx in 0..limit {
                let len = unsafe { DragQueryFileW(hdrop, idx as u32, ptr::null_mut(), 0) };
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; (len + 1) as usize];
                let copied = unsafe { DragQueryFileW(hdrop, idx as u32, buf.as_mut_ptr(), len + 1) };
                if copied > 0 {
                    let os = OsString::from_wide(&buf[..copied as usize]);
                    paths.push(PathBuf::from(os));
                }
            }
        }
    }

    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    paths
}

// --- IDropTarget for invisible edge-strip windows ---

#[repr(C)]
struct IDropTargetVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    drag_enter:
        unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> HRESULT,
    drag_over: unsafe extern "system" fn(*mut c_void, u32, POINTL, *mut u32) -> HRESULT,
    drag_leave: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    drop: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> HRESULT,
}

#[repr(C)]
struct EdgeDropTarget {
    vtbl: *const IDropTargetVtbl,
    ref_count: AtomicU32,
    edge: Edge,
    shared: Arc<SharedDnd>,
}

static EDGE_DROP_TARGET_VTBL: IDropTargetVtbl = IDropTargetVtbl {
    query_interface: edge_dt_query_interface,
    add_ref: edge_dt_add_ref,
    release: edge_dt_release,
    drag_enter: edge_dt_drag_enter,
    drag_over: edge_dt_drag_over,
    drag_leave: edge_dt_drag_leave,
    drop: edge_dt_drop,
};

impl EdgeDropTarget {
    fn new_raw(edge: Edge, shared: Arc<SharedDnd>) -> *mut c_void {
        Box::into_raw(Box::new(Self {
            vtbl: &EDGE_DROP_TARGET_VTBL,
            ref_count: AtomicU32::new(1),
            edge,
            shared,
        })) as *mut c_void
    }
}

unsafe extern "system" fn edge_dt_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe {
        *ppv = ptr::null_mut();
    }
    if riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = unsafe { &*riid };
    if guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IDROPTARGET) {
        unsafe {
            edge_dt_add_ref(this);
            *ppv = this;
        }
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "system" fn edge_dt_add_ref(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const EdgeDropTarget) };
    me.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn edge_dt_release(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const EdgeDropTarget) };
    let prev = me.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this as *mut EdgeDropTarget));
        }
    }
    prev - 1
}

unsafe extern "system" fn edge_dt_drag_enter(
    this: *mut c_void,
    data_obj: *mut c_void,
    _key_state: u32,
    _pt: POINTL,
    pdw_effect: *mut u32,
) -> HRESULT {
    let me = unsafe { &*(this as *const EdgeDropTarget) };
    if !me.shared.armed.load(Ordering::Acquire) {
        if !pdw_effect.is_null() {
            unsafe {
                *pdw_effect = DROPEFFECT_NONE;
            }
        }
        return S_OK;
    }
    let paths = unsafe { extract_hdrop_paths(data_obj) };
    if !paths.is_empty() {
        let mut guard = me.shared.staged.lock();
        if !me.shared.armed.load(Ordering::Acquire) {
            if !pdw_effect.is_null() {
                unsafe {
                    *pdw_effect = DROPEFFECT_NONE;
                }
            }
            return S_OK;
        }
        debug!(
            edge = ?me.edge,
            count = paths.len(),
            "Captured CF_HDROP files on Windows edge strip"
        );
        *guard = Some(StagedDrag {
            edge: me.edge,
            paths,
            staged_at: Instant::now(),
            left_at: None,
        });
        me.shared.staged_tracker.mark_staged(me.edge, false);
        if !pdw_effect.is_null() {
            unsafe {
                *pdw_effect = DROPEFFECT_COPY;
            }
        }
    } else {
        // Non-file OLE drag (e.g. selected text or browser tab): do not intercept,
        // disarm & hide the edge strip immediately so normal desktop dragging is unimpeded.
        me.shared.armed.store(false, Ordering::SeqCst);
        me.shared.clear_staged();
        me.shared.post_hide_strips();
        if !pdw_effect.is_null() {
            unsafe {
                *pdw_effect = DROPEFFECT_NONE;
            }
        }
    }
    S_OK
}

unsafe extern "system" fn edge_dt_drag_over(
    this: *mut c_void,
    _key_state: u32,
    _pt: POINTL,
    pdw_effect: *mut u32,
) -> HRESULT {
    let me = unsafe { &*(this as *const EdgeDropTarget) };
    if !me.shared.armed.load(Ordering::Acquire) {
        if !pdw_effect.is_null() {
            unsafe {
                *pdw_effect = DROPEFFECT_NONE;
            }
        }
        return S_OK;
    }
    let mut guard = me.shared.staged.lock();
    let has_files = if me.shared.armed.load(Ordering::Acquire) {
        if let Some(ref mut staged) = *guard {
            if staged.edge == me.edge {
                staged.staged_at = Instant::now();
                staged.left_at = None;
                me.shared.staged_tracker.touch(me.edge);
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    if !pdw_effect.is_null() {
        unsafe {
            *pdw_effect = if has_files {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            };
        }
    }
    S_OK
}

unsafe extern "system" fn edge_dt_drag_leave(this: *mut c_void) -> HRESULT {
    let me = unsafe { &*(this as *const EdgeDropTarget) };
    if let Some(ref mut staged) = *me.shared.staged.lock() {
        if staged.edge == me.edge {
            staged.left_at = Some(Instant::now());
            me.shared.staged_tracker.mark_left(me.edge);
        }
    }
    S_OK
}

unsafe extern "system" fn edge_dt_drop(
    this: *mut c_void,
    data_obj: *mut c_void,
    _key_state: u32,
    _pt: POINTL,
    pdw_effect: *mut u32,
) -> HRESULT {
    let me = unsafe { &*(this as *const EdgeDropTarget) };
    if me.shared.armed.load(Ordering::Acquire) {
        let paths = unsafe { extract_hdrop_paths(data_obj) };
        if !paths.is_empty() {
            let mut guard = me.shared.staged.lock();
            if me.shared.armed.load(Ordering::Acquire) {
                *guard = Some(StagedDrag {
                    edge: me.edge,
                    paths,
                    staged_at: Instant::now(),
                    left_at: Some(Instant::now()),
                });
                me.shared.staged_tracker.mark_staged(me.edge, true);
            }
        }
    }
    // Never return DROPEFFECT_MOVE so Explorer never deletes the original file locally.
    if !pdw_effect.is_null() {
        unsafe {
            *pdw_effect = DROPEFFECT_NONE;
        }
    }
    S_OK
}

// ---------------------------------------------------------------------------
// Synthetic mouse helpers (all tagged GLIDE_EXTRA_INFO so capture hooks ignore them)
// ---------------------------------------------------------------------------

fn send_mouse_button_input(flags: u32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: GLIDE_EXTRA_INFO,
            },
        },
    };
    unsafe {
        SendInput(1, &input, size_of::<INPUT>() as i32);
    }
}

fn send_mouse_wiggle() {
    // Non-zero (+1, 0) then (-1, 0) relative motion guarantees win32k generates real WM_MOUSEMOVE
    // events into ole32's DoDragDrop capture window (unlike dx=0, dy=0 which win32k drops!).
    let inputs = [
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 1,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: GLIDE_EXTRA_INFO,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: -1,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: GLIDE_EXTRA_INFO,
                },
            },
        },
    ];
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
    }
}

// ---------------------------------------------------------------------------
// Drag-in (Peer -> Windows): Native Shell IDataObject + IDragSourceHelper2
// + SetData-capable FileDataObject fallback + Live SHDoDragDrop
// ---------------------------------------------------------------------------

/// Strips Windows verbatim path prefixes (`\\?\C:\...` -> `C:\...`, `\\?\UNC\...` -> `\\...`)
/// because Shell APIs (`ILCreateFromPathW`, Explorer `CF_HDROP`) reject `\\?\` paths.
fn strip_windows_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

fn preferred_drop_effect_format() -> u16 {
    let name = encode_wide("Preferred DropEffect");
    unsafe { RegisterClipboardFormatW(name.as_ptr()) as u16 }
}

fn build_hdrop_global(paths: &[PathBuf]) -> Result<HGLOBAL> {
    let mut wide_list: Vec<u16> = Vec::new();
    for path in paths {
        let clean = strip_windows_verbatim_prefix(path);
        let s: Vec<u16> = clean.as_os_str().encode_wide().collect();
        if s.is_empty() {
            continue;
        }
        wide_list.extend_from_slice(&s);
        wide_list.push(0);
    }
    wide_list.push(0); // double NUL terminator

    let header_len = size_of::<DROPFILES>();
    let total_bytes = header_len + wide_list.len() * size_of::<u16>();

    let hglobal = unsafe { GlobalAlloc(GHND, total_bytes) };
    if hglobal.is_null() {
        return Err(anyhow!(
            "GlobalAlloc failed for CF_HDROP ({total_bytes} bytes)"
        ));
    }

    let dst = unsafe { GlobalLock(hglobal) } as *mut u8;
    if dst.is_null() {
        unsafe {
            GlobalFree(hglobal);
        }
        return Err(anyhow!("GlobalLock failed for CF_HDROP"));
    }

    let hdr = DROPFILES {
        pFiles: header_len as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: 0,
        fWide: 1,
    };
    unsafe {
        ptr::copy_nonoverlapping(&hdr as *const DROPFILES as *const u8, dst, header_len);
        ptr::copy_nonoverlapping(
            wide_list.as_ptr() as *const u8,
            dst.add(header_len),
            wide_list.len() * size_of::<u16>(),
        );
        GlobalUnlock(hglobal);
    }
    Ok(hglobal)
}

fn build_dword_global(val: u32) -> Result<HGLOBAL> {
    let hglobal = unsafe { GlobalAlloc(GHND, size_of::<u32>()) };
    if hglobal.is_null() {
        return Err(anyhow!("GlobalAlloc failed for DWORD format"));
    }
    let dst = unsafe { GlobalLock(hglobal) } as *mut u32;
    if dst.is_null() {
        unsafe {
            GlobalFree(hglobal);
        }
        return Err(anyhow!("GlobalLock failed for DWORD format"));
    }
    unsafe {
        *dst = val;
        GlobalUnlock(hglobal);
    }
    Ok(hglobal)
}

unsafe fn clone_hglobal(src: HGLOBAL) -> Option<HGLOBAL> {
    if src.is_null() {
        return None;
    }
    let size = unsafe { GlobalSize(src) };
    if size == 0 {
        return None;
    }
    let dst = unsafe { GlobalAlloc(GHND, size) };
    if dst.is_null() {
        return None;
    }
    let src_ptr = unsafe { GlobalLock(src) } as *const u8;
    let dst_ptr = unsafe { GlobalLock(dst) } as *mut u8;
    if src_ptr.is_null() || dst_ptr.is_null() {
        if !src_ptr.is_null() {
            unsafe {
                GlobalUnlock(src);
            }
        }
        if !dst_ptr.is_null() {
            unsafe {
                GlobalUnlock(dst);
            }
        }
        unsafe {
            GlobalFree(dst);
        }
        return None;
    }
    unsafe {
        ptr::copy_nonoverlapping(src_ptr, dst_ptr, size);
        GlobalUnlock(src);
        GlobalUnlock(dst);
    }
    Some(dst)
}

/// Builds a genuine Windows Shell `IDataObject` via `SHCreateDataObject` for files sharing a
/// common parent folder (`Downloads\Glide\`). Explorer, Desktop, Quick Access, OneDrive, Chrome,
/// Discord, and VS Code treat this identically to dragging directly from Windows 11 File Explorer!
unsafe fn create_shell_data_object(paths: &[PathBuf]) -> Option<*mut c_void> {
    if paths.is_empty() || paths.len() > MAX_DRAG_FILES {
        return None;
    }
    let cleaned: Vec<PathBuf> = paths
        .iter()
        .map(|p| strip_windows_verbatim_prefix(p))
        .collect();
    let parent = cleaned[0].parent()?;
    if parent.as_os_str().is_empty() || cleaned.iter().any(|p| p.parent() != Some(parent)) {
        return None;
    }

    let parent_wide: Vec<u16> = parent
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let parent_pidl = unsafe { ILCreateFromPathW(parent_wide.as_ptr()) };
    if parent_pidl.is_null() {
        return None;
    }

    let mut full_pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(cleaned.len());
    let mut child_pidls: Vec<*const ITEMIDLIST> = Vec::with_capacity(cleaned.len());
    for p in &cleaned {
        let wide: Vec<u16> = p
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let pidl = unsafe { ILCreateFromPathW(wide.as_ptr()) };
        if pidl.is_null() {
            continue;
        }
        let child = unsafe { ILFindLastID(pidl) };
        if !child.is_null() {
            child_pidls.push(child as *const ITEMIDLIST);
        }
        full_pidls.push(pidl);
    }

    let mut data_obj: *mut c_void = ptr::null_mut();
    let hr = if !child_pidls.is_empty() {
        unsafe {
            SHCreateDataObject(
                parent_pidl as *const ITEMIDLIST,
                child_pidls.len() as u32,
                child_pidls.as_ptr(),
                ptr::null_mut(),
                &IID_IDATAOBJECT,
                &mut data_obj,
            )
        }
    } else {
        E_INVALIDARG
    };

    for pidl in full_pidls {
        unsafe {
            ILFree(pidl as *const ITEMIDLIST);
        }
    }
    unsafe {
        ILFree(parent_pidl as *const ITEMIDLIST);
    }

    if hr >= 0 && !data_obj.is_null() {
        Some(data_obj)
    } else {
        debug!(hr, "SHCreateDataObject unavailable; falling back to FileDataObject");
        None
    }
}

// --- IDragSourceHelper2 COM vtable (verified against Windows 11 SDK 10.0.26100 shobjidl.h) ---

#[repr(C)]
struct IDragSourceHelper2Vtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    initialize_from_bitmap:
        unsafe extern "system" fn(*mut c_void, *mut SHDRAGIMAGE, *mut c_void) -> HRESULT,
    initialize_from_window:
        unsafe extern "system" fn(*mut c_void, HWND, *const POINT, *mut c_void) -> HRESULT,
    set_flags: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
}

/// Renders a crisp Windows-11-style drag thumbnail bitmap (with magenta color key for rounded
/// transparent corners) so `IDragSourceHelper2` displays a file preview icon following the cursor
/// together with Explorer's native `"+ Copy to <Folder>"` badge!
fn create_drag_image_bitmap(paths: &[PathBuf]) -> Option<(HBITMAP, i32, i32)> {
    let filenames: Vec<String> = paths
        .iter()
        .map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string()
        })
        .collect();
    let badge = format_drag_extension_badge(&filenames);
    let label = format_drag_preview_label(&filenames);

    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let geom = query_desktop_geometry().unwrap_or_default();
    let scale = geom
        .monitors
        .iter()
        .find(|m| m.contains(pt.x, pt.y))
        .map(|m| m.scale)
        .unwrap_or_else(|| geom.primary_scale())
        .clamp(1.0, 2.5);
    let px = |v: f32| (v * scale).round() as i32;

    let w = px(136.0).max(96);
    let h = px(88.0).max(64);
    let screen_dc = unsafe { GetDC(ptr::null_mut()) };
    if screen_dc.is_null() {
        return None;
    }
    let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let hbmp = unsafe { CreateCompatibleBitmap(screen_dc, w, h) };
    unsafe {
        ReleaseDC(ptr::null_mut(), screen_dc);
    }
    if mem_dc.is_null() || hbmp.is_null() {
        if !hbmp.is_null() {
            unsafe {
                DeleteObject(hbmp);
            }
        }
        if !mem_dc.is_null() {
            unsafe {
                DeleteDC(mem_dc);
            }
        }
        return None;
    }

    let old_bmp = unsafe { SelectObject(mem_dc, hbmp) };

    // Fill background with magenta color key (0x00FF00FF -> rendered 100% transparent by Shell)
    let key_brush = unsafe { CreateSolidBrush(win_rgb(0xFF, 0x00, 0xFF)) };
    let full_rc = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    unsafe {
        FillRect(mem_dc, &full_rc, key_brush);
    }

    // Crisp rounded white card with subtle border
    let card_brush = unsafe { CreateSolidBrush(win_rgb(0xFA, 0xFC, 0xFF)) };
    let border_pen = unsafe { CreatePen(PS_SOLID, px(1.5).max(1), win_rgb(0xCB, 0xD5, 0xE1)) };
    let old_brush = unsafe { SelectObject(mem_dc, card_brush) };
    let old_pen = unsafe { SelectObject(mem_dc, border_pen) };
    let pad = px(2.0);
    unsafe {
        RoundRect(mem_dc, pad, pad, w - pad, h - pad, px(16.0), px(16.0));
    }

    // Centered blue document badge tile
    let tile_w = px(52.0);
    let tile_h = px(40.0);
    let tile_x = (w - tile_w) / 2;
    let tile_y = px(12.0);
    let accent_brush = unsafe { CreateSolidBrush(win_rgb(0x25, 0x63, 0xEB)) };
    let accent_pen = unsafe { CreatePen(PS_SOLID, 1, win_rgb(0x25, 0x63, 0xEB)) };
    unsafe {
        SelectObject(mem_dc, accent_brush);
        SelectObject(mem_dc, accent_pen);
        RoundRect(
            mem_dc,
            tile_x,
            tile_y,
            tile_x + tile_w,
            tile_y + tile_h,
            px(10.0),
            px(10.0),
        );
    }

    let face = encode_wide("Segoe UI");
    let badge_font = unsafe {
        CreateFontW(
            -px(13.0),
            0,
            0,
            0,
            700,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_SWISS) as u32,
            face.as_ptr(),
        )
    };
    let label_font = unsafe {
        CreateFontW(
            -px(11.0),
            0,
            0,
            0,
            600,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_SWISS) as u32,
            face.as_ptr(),
        )
    };

    unsafe {
        SetBkMode(mem_dc, TRANSPARENT as i32);
    }
    let old_font = unsafe { SelectObject(mem_dc, badge_font) };
    let badge_w = encode_wide(&badge);
    let mut tile_rc = RECT {
        left: tile_x,
        top: tile_y,
        right: tile_x + tile_w,
        bottom: tile_y + tile_h,
    };
    unsafe {
        SetTextColor(mem_dc, win_rgb(0xFF, 0xFF, 0xFF));
        DrawTextW(
            mem_dc,
            badge_w.as_ptr(),
            -1,
            &mut tile_rc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    let label_w = encode_wide(&label);
    let mut lbl_rc = RECT {
        left: px(8.0),
        top: tile_y + tile_h + px(6.0),
        right: w - px(8.0),
        bottom: h - px(8.0),
    };
    unsafe {
        SelectObject(mem_dc, label_font);
        SetTextColor(mem_dc, win_rgb(0x0F, 0x17, 0x2A));
        DrawTextW(
            mem_dc,
            label_w.as_ptr(),
            -1,
            &mut lbl_rc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );

        SelectObject(mem_dc, old_font);
        SelectObject(mem_dc, old_brush);
        SelectObject(mem_dc, old_pen);
        SelectObject(mem_dc, old_bmp);
        DeleteObject(badge_font);
        DeleteObject(label_font);
        DeleteObject(accent_brush);
        DeleteObject(accent_pen);
        DeleteObject(card_brush);
        DeleteObject(border_pen);
        DeleteObject(key_brush);
        DeleteDC(mem_dc);
    }

    Some((hbmp, w, h))
}

unsafe fn attach_shell_drag_image(data_obj: *mut c_void, paths: &[PathBuf]) {
    let mut helper: *mut c_void = ptr::null_mut();
    let hr2 = unsafe {
        CoCreateInstance(
            &CLSID_DragDropHelper,
            ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IDRAGSOURCEHELPER2,
            &mut helper,
        )
    };
    let is_helper2 = hr2 >= 0 && !helper.is_null();
    if !is_helper2 {
        let hr1 = unsafe {
            CoCreateInstance(
                &CLSID_DragDropHelper,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IDRAGSOURCEHELPER,
                &mut helper,
            )
        };
        if hr1 < 0 || helper.is_null() {
            return;
        }
    }

    let vtbl = unsafe { *(helper as *const *const IDragSourceHelper2Vtbl) };
    if is_helper2 {
        // Enable Windows 11 native "+ Copy to <Folder>" dynamic drop descriptions!
        let _ = unsafe { ((*vtbl).set_flags)(helper, DSH_ALLOWDROPDESCRIPTIONTEXT) };
    }

    if let Some((hbmp, w, h)) = create_drag_image_bitmap(paths) {
        let mut shdi = SHDRAGIMAGE {
            sizeDragImage: SIZE { cx: w, cy: h },
            ptOffset: POINT {
                x: w / 2,
                y: h - (h / 5),
            },
            hbmpDragImage: hbmp,
            crColorKey: win_rgb(0xFF, 0x00, 0xFF),
        };
        let hr = unsafe { ((*vtbl).initialize_from_bitmap)(helper, &mut shdi, data_obj) };
        if hr < 0 {
            // Caller retains ownership of hbmp only on error; on S_OK Shell owns it.
            unsafe {
                DeleteObject(hbmp);
            }
            trace!(hr, "IDragSourceHelper::InitializeFromBitmap returned error");
        }
    }

    unsafe {
        com_release(helper);
    }
}

fn hdrop_formatetc() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    }
}

fn preferred_effect_formatetc(cf: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    }
}

fn formatetc_matches(a: &FORMATETC, b: &FORMATETC) -> bool {
    a.cfFormat == b.cfFormat
        && (a.dwAspect & b.dwAspect) != 0
        && (a.lindex == b.lindex || a.lindex == -1 || b.lindex == -1)
        && (a.tymed & b.tymed) != 0
}

// --- IEnumFORMATETC (multi-format, used by upgraded FileDataObject) ---

#[repr(C)]
struct IEnumFormatEtcVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    next: unsafe extern "system" fn(*mut c_void, u32, *mut FORMATETC, *mut u32) -> HRESULT,
    skip: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    reset: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    clone: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct MultiFormatEnum {
    vtbl: *const IEnumFormatEtcVtbl,
    ref_count: AtomicU32,
    formats: Vec<FORMATETC>,
    index: AtomicU32,
}

static MULTI_FORMAT_ENUM_VTBL: IEnumFormatEtcVtbl = IEnumFormatEtcVtbl {
    query_interface: enum_fmt_query_interface,
    add_ref: enum_fmt_add_ref,
    release: enum_fmt_release,
    next: enum_fmt_next,
    skip: enum_fmt_skip,
    reset: enum_fmt_reset,
    clone: enum_fmt_clone,
};

impl MultiFormatEnum {
    fn new_raw(formats: Vec<FORMATETC>, index: u32) -> *mut c_void {
        Box::into_raw(Box::new(Self {
            vtbl: &MULTI_FORMAT_ENUM_VTBL,
            ref_count: AtomicU32::new(1),
            formats,
            index: AtomicU32::new(index),
        })) as *mut c_void
    }
}

unsafe extern "system" fn enum_fmt_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe {
        *ppv = ptr::null_mut();
    }
    if riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = unsafe { &*riid };
    if guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IENUMFORMATETC) {
        unsafe {
            enum_fmt_add_ref(this);
            *ppv = this;
        }
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "system" fn enum_fmt_add_ref(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const MultiFormatEnum) };
    me.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn enum_fmt_release(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const MultiFormatEnum) };
    let prev = me.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this as *mut MultiFormatEnum));
        }
    }
    prev - 1
}

unsafe extern "system" fn enum_fmt_next(
    this: *mut c_void,
    celt: u32,
    rgelt: *mut FORMATETC,
    pcelt_fetched: *mut u32,
) -> HRESULT {
    if rgelt.is_null() || (celt > 1 && pcelt_fetched.is_null()) {
        return E_INVALIDARG;
    }
    let me = unsafe { &*(this as *const MultiFormatEnum) };
    let len = me.formats.len() as u32;
    let cur = me.index.load(Ordering::Relaxed).min(len);
    let avail = len.saturating_sub(cur);
    let to_copy = celt.min(avail);
    for i in 0..to_copy {
        unsafe {
            *rgelt.add(i as usize) = me.formats[(cur + i) as usize];
        }
    }
    me.index.store(cur + to_copy, Ordering::Relaxed);
    if !pcelt_fetched.is_null() {
        unsafe {
            *pcelt_fetched = to_copy;
        }
    }
    if to_copy == celt {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn enum_fmt_skip(this: *mut c_void, celt: u32) -> HRESULT {
    let me = unsafe { &*(this as *const MultiFormatEnum) };
    let len = me.formats.len() as u32;
    let cur = me.index.load(Ordering::Relaxed);
    let next = cur.saturating_add(celt);
    me.index.store(next.min(len), Ordering::Relaxed);
    if next <= len {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn enum_fmt_reset(this: *mut c_void) -> HRESULT {
    let me = unsafe { &*(this as *const MultiFormatEnum) };
    me.index.store(0, Ordering::Relaxed);
    S_OK
}

unsafe extern "system" fn enum_fmt_clone(this: *mut c_void, ppenum: *mut *mut c_void) -> HRESULT {
    if ppenum.is_null() {
        return E_POINTER;
    }
    let me = unsafe { &*(this as *const MultiFormatEnum) };
    let idx = me.index.load(Ordering::Relaxed);
    unsafe {
        *ppenum = MultiFormatEnum::new_raw(me.formats.clone(), idx);
    }
    S_OK
}

// --- IDataObject (upgraded fallback supporting CF_HDROP, Preferred DropEffect, and SetData) ---

struct StoredFormat {
    format: FORMATETC,
    medium: STGMEDIUM,
}

impl Drop for StoredFormat {
    fn drop(&mut self) {
        unsafe {
            ReleaseStgMedium(&mut self.medium);
        }
    }
}

#[repr(C)]
struct FileDataObject {
    vtbl: *const IDataObjectVtbl,
    ref_count: AtomicU32,
    paths: Vec<PathBuf>,
    preferred_effect_cf: u16,
    extra: Mutex<Vec<StoredFormat>>,
}

static FILE_DATA_OBJECT_VTBL: IDataObjectVtbl = IDataObjectVtbl {
    query_interface: data_obj_query_interface,
    add_ref: data_obj_add_ref,
    release: data_obj_release,
    get_data: data_obj_get_data,
    get_data_here: data_obj_get_data_here,
    query_get_data: data_obj_query_get_data,
    get_canonical_format_etc: data_obj_get_canonical,
    set_data: data_obj_set_data,
    enum_format_etc: data_obj_enum_format_etc,
    d_advise: data_obj_d_advise,
    d_unadvise: data_obj_d_unadvise,
    enum_d_advise: data_obj_enum_d_advise,
};

impl FileDataObject {
    fn new_raw(paths: Vec<PathBuf>) -> *mut c_void {
        Box::into_raw(Box::new(Self {
            vtbl: &FILE_DATA_OBJECT_VTBL,
            ref_count: AtomicU32::new(1),
            paths,
            preferred_effect_cf: preferred_drop_effect_format(),
            extra: Mutex::new(Vec::new()),
        })) as *mut c_void
    }
}

unsafe extern "system" fn data_obj_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe {
        *ppv = ptr::null_mut();
    }
    if riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = unsafe { &*riid };
    if guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IDATAOBJECT) {
        unsafe {
            data_obj_add_ref(this);
            *ppv = this;
        }
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "system" fn data_obj_add_ref(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const FileDataObject) };
    me.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn data_obj_release(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const FileDataObject) };
    let prev = me.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this as *mut FileDataObject));
        }
    }
    prev - 1
}

unsafe extern "system" fn data_obj_get_data(
    this: *mut c_void,
    pformatetc: *mut FORMATETC,
    pmedium: *mut STGMEDIUM,
) -> HRESULT {
    if pformatetc.is_null() || pmedium.is_null() {
        return E_INVALIDARG;
    }
    unsafe {
        (*pmedium).tymed = TYMED_NULL as u32;
        (*pmedium).u = STGMEDIUM_0 {
            hGlobal: ptr::null_mut(),
        };
        (*pmedium).pUnkForRelease = ptr::null_mut();
    }
    let fmt = unsafe { &*pformatetc };
    let me = unsafe { &*(this as *const FileDataObject) };

    // 1. Check any custom/Shell format stored via SetData (e.g. DragImageBits, DropDescription,
    //    DragContext, InShellDragLoop, Performed DropEffect).
    {
        let extra = me.extra.lock();
        if let Some(stored) = extra.iter().find(|s| formatetc_matches(fmt, &s.format)) {
            if stored.medium.tymed == TYMED_HGLOBAL as u32 {
                let src = unsafe { stored.medium.u.hGlobal };
                if let Some(cloned) = unsafe { clone_hglobal(src) } {
                    unsafe {
                        (*pmedium).tymed = TYMED_HGLOBAL as u32;
                        (*pmedium).u = STGMEDIUM_0 { hGlobal: cloned };
                        (*pmedium).pUnkForRelease = ptr::null_mut();
                    }
                    return S_OK;
                }
                return E_OUTOFMEMORY;
            }
            return DV_E_TYMED;
        }
    }

    // 2. CF_HDROP
    if fmt.cfFormat == CF_HDROP {
        if (fmt.dwAspect & DVASPECT_CONTENT) == 0 || (fmt.lindex != -1 && fmt.lindex != 0) {
            return DV_E_FORMATETC;
        }
        if (fmt.tymed & (TYMED_HGLOBAL as u32)) == 0 {
            return DV_E_TYMED;
        }
        return match build_hdrop_global(&me.paths) {
            Ok(hglobal) => {
                unsafe {
                    (*pmedium).tymed = TYMED_HGLOBAL as u32;
                    (*pmedium).u = STGMEDIUM_0 { hGlobal: hglobal };
                    (*pmedium).pUnkForRelease = ptr::null_mut();
                }
                S_OK
            }
            Err(_) => E_OUTOFMEMORY,
        };
    }

    // 3. Preferred DropEffect (DROPEFFECT_COPY)
    if me.preferred_effect_cf != 0 && fmt.cfFormat == me.preferred_effect_cf {
        if (fmt.tymed & (TYMED_HGLOBAL as u32)) == 0 {
            return DV_E_TYMED;
        }
        return match build_dword_global(DROPEFFECT_COPY) {
            Ok(hglobal) => {
                unsafe {
                    (*pmedium).tymed = TYMED_HGLOBAL as u32;
                    (*pmedium).u = STGMEDIUM_0 { hGlobal: hglobal };
                    (*pmedium).pUnkForRelease = ptr::null_mut();
                }
                S_OK
            }
            Err(_) => E_OUTOFMEMORY,
        };
    }

    DV_E_FORMATETC
}

unsafe extern "system" fn data_obj_get_data_here(
    _this: *mut c_void,
    _pformatetc: *mut FORMATETC,
    _pmedium: *mut STGMEDIUM,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn data_obj_query_get_data(
    this: *mut c_void,
    pformatetc: *mut FORMATETC,
) -> HRESULT {
    if pformatetc.is_null() {
        return E_INVALIDARG;
    }
    let fmt = unsafe { &*pformatetc };
    let me = unsafe { &*(this as *const FileDataObject) };

    if me.extra.lock().iter().any(|s| formatetc_matches(fmt, &s.format)) {
        return S_OK;
    }
    if fmt.cfFormat == CF_HDROP
        || (me.preferred_effect_cf != 0 && fmt.cfFormat == me.preferred_effect_cf)
    {
        if (fmt.dwAspect & DVASPECT_CONTENT) == 0 || (fmt.lindex != -1 && fmt.lindex != 0) {
            return DV_E_FORMATETC;
        }
        if (fmt.tymed & (TYMED_HGLOBAL as u32)) == 0 {
            return DV_E_TYMED;
        }
        return S_OK;
    }
    DV_E_FORMATETC
}

unsafe extern "system" fn data_obj_get_canonical(
    _this: *mut c_void,
    _pformatect_in: *mut FORMATETC,
    pformatetc_out: *mut FORMATETC,
) -> HRESULT {
    if !pformatetc_out.is_null() {
        unsafe {
            (*pformatetc_out).ptd = ptr::null_mut();
        }
    }
    DATA_S_SAMEFORMATETC
}

unsafe extern "system" fn data_obj_set_data(
    this: *mut c_void,
    pformatetc: *mut FORMATETC,
    pmedium: *mut STGMEDIUM,
    frelease: BOOL,
) -> HRESULT {
    if pformatetc.is_null() || pmedium.is_null() {
        return E_INVALIDARG;
    }
    let mut fmt = unsafe { *pformatetc };
    fmt.ptd = ptr::null_mut();
    let incoming = unsafe { *pmedium };

    let stored_medium = if frelease != 0 {
        incoming
    } else if incoming.tymed == TYMED_HGLOBAL as u32 {
        let src = unsafe { incoming.u.hGlobal };
        let Some(cloned) = (unsafe { clone_hglobal(src) }) else {
            return E_OUTOFMEMORY;
        };
        STGMEDIUM {
            tymed: TYMED_HGLOBAL as u32,
            u: STGMEDIUM_0 { hGlobal: cloned },
            pUnkForRelease: ptr::null_mut(),
        }
    } else {
        return DV_E_TYMED;
    };

    let me = unsafe { &*(this as *const FileDataObject) };
    let mut extra = me.extra.lock();
    extra.retain(|s| {
        !(s.format.cfFormat == fmt.cfFormat
            && s.format.dwAspect == fmt.dwAspect
            && s.format.lindex == fmt.lindex)
    });
    extra.push(StoredFormat {
        format: fmt,
        medium: stored_medium,
    });
    S_OK
}

unsafe extern "system" fn data_obj_enum_format_etc(
    this: *mut c_void,
    dw_direction: u32,
    ppenum: *mut *mut c_void,
) -> HRESULT {
    if ppenum.is_null() {
        return E_POINTER;
    }
    unsafe {
        *ppenum = ptr::null_mut();
    }
    if dw_direction == DATADIR_GET as u32 {
        let me = unsafe { &*(this as *const FileDataObject) };
        let mut formats = vec![hdrop_formatetc()];
        if me.preferred_effect_cf != 0 {
            formats.push(preferred_effect_formatetc(me.preferred_effect_cf));
        }
        for s in me.extra.lock().iter() {
            if !formats.iter().any(|f| f.cfFormat == s.format.cfFormat) {
                formats.push(s.format);
            }
        }
        unsafe {
            *ppenum = MultiFormatEnum::new_raw(formats, 0);
        }
        S_OK
    } else {
        E_NOTIMPL
    }
}

unsafe extern "system" fn data_obj_d_advise(
    _this: *mut c_void,
    _pformatetc: *mut FORMATETC,
    _advf: u32,
    _padv_sink: *mut c_void,
    _pdw_connection: *mut u32,
) -> HRESULT {
    OLE_E_ADVISENOTSUPPORTED
}

unsafe extern "system" fn data_obj_d_unadvise(_this: *mut c_void, _dw_connection: u32) -> HRESULT {
    OLE_E_ADVISENOTSUPPORTED
}

unsafe extern "system" fn data_obj_enum_d_advise(
    _this: *mut c_void,
    _ppenum: *mut *mut c_void,
) -> HRESULT {
    OLE_E_ADVISENOTSUPPORTED
}

// --- IDropSource (Live hover + Post-release modes) ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragRunMode {
    Live,
    ImmediatePostRelease,
}

#[repr(C)]
struct IDropSourceVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    query_continue_drag: unsafe extern "system" fn(*mut c_void, BOOL, u32) -> HRESULT,
    give_feedback: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
}

#[repr(C)]
struct LiveDropSource {
    vtbl: *const IDropSourceVtbl,
    ref_count: AtomicU32,
    mode: DragRunMode,
    session: Arc<LiveOleSession>,
    shared: Arc<SharedDnd>,
    feedback_seen: Arc<AtomicBool>,
    user_escaped: Arc<AtomicBool>,
    hid_preview: AtomicBool,
    started: Instant,
}

static LIVE_DROP_SOURCE_VTBL: IDropSourceVtbl = IDropSourceVtbl {
    query_interface: drop_src_query_interface,
    add_ref: drop_src_add_ref,
    release: drop_src_release,
    query_continue_drag: drop_src_query_continue_drag,
    give_feedback: drop_src_give_feedback,
};

impl LiveDropSource {
    fn new_raw(
        mode: DragRunMode,
        session: Arc<LiveOleSession>,
        shared: Arc<SharedDnd>,
        feedback_seen: Arc<AtomicBool>,
        user_escaped: Arc<AtomicBool>,
    ) -> *mut c_void {
        Box::into_raw(Box::new(Self {
            vtbl: &LIVE_DROP_SOURCE_VTBL,
            ref_count: AtomicU32::new(1),
            mode,
            session,
            shared,
            feedback_seen,
            user_escaped,
            hid_preview: AtomicBool::new(false),
            started: Instant::now(),
        })) as *mut c_void
    }
}

unsafe extern "system" fn drop_src_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    unsafe {
        *ppv = ptr::null_mut();
    }
    if riid.is_null() {
        return E_INVALIDARG;
    }
    let iid = unsafe { &*riid };
    if guid_eq(iid, &IID_IUNKNOWN) || guid_eq(iid, &IID_IDROPSOURCE) {
        unsafe {
            drop_src_add_ref(this);
            *ppv = this;
        }
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "system" fn drop_src_add_ref(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const LiveDropSource) };
    me.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn drop_src_release(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const LiveDropSource) };
    let prev = me.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this as *mut LiveDropSource));
        }
    }
    prev - 1
}

unsafe extern "system" fn drop_src_query_continue_drag(
    this: *mut c_void,
    escape_pressed: BOOL,
    key_state: u32,
) -> HRESULT {
    let me = unsafe { &*(this as *const LiveDropSource) };
    if escape_pressed != 0 {
        me.user_escaped.store(true, Ordering::Release);
        return DRAGDROP_S_CANCEL;
    }
    if me.session.cancel_requested.load(Ordering::Acquire) {
        return DRAGDROP_S_CANCEL;
    }

    match me.mode {
        DragRunMode::Live => {
            if me.session.drop_requested.load(Ordering::Acquire) {
                if me.feedback_seen.load(Ordering::Acquire)
                    || me.started.elapsed() >= Duration::from_millis(260)
                {
                    return DRAGDROP_S_DROP;
                }
                return S_OK;
            }
            // Wait at least 100ms before treating MK_LBUTTON==0 as a release so starter-click
            // input state has settled and SHDoDragDrop never drops prematurely on entry.
            if (key_state & MK_LBUTTON) == 0
                && me.started.elapsed() >= Duration::from_millis(100)
            {
                return DRAGDROP_S_DROP;
            }
            S_OK
        }
        DragRunMode::ImmediatePostRelease => {
            if me.feedback_seen.load(Ordering::Acquire) {
                return DRAGDROP_S_DROP;
            }
            if me.started.elapsed() >= Duration::from_millis(360) {
                return DRAGDROP_S_CANCEL;
            }
            S_OK
        }
    }
}

unsafe extern "system" fn drop_src_give_feedback(this: *mut c_void, _dw_effect: u32) -> HRESULT {
    let me = unsafe { &*(this as *const LiveDropSource) };
    me.feedback_seen.store(true, Ordering::Release);
    // As soon as Windows Shell's own SHDoDragDrop visual feedback is active, hide our temporary
    // "Receiving..." preview overlay so there is zero double-card overlap or flicker.
    if !me.hid_preview.swap(true, Ordering::AcqRel) {
        me.shared.post_preview_hide();
    }
    DRAGDROP_S_USEDEFAULTCURSORS
}

// --- Invisible virtual-desktop drag-starter window (grants real foreground SetCapture to SHDoDragDrop) ---

struct StarterState {
    got_lbutton_down: bool,
}

/// Makes `starter_hwnd` completely click-through and parks it 1×1 off-screen while KEEPING it
/// visible & foreground on this STA thread — critically avoiding `ShowWindow(SW_HIDE)`, which
/// would synchronously deactivate the window and cause `ole32!DoDragDrop`'s `SetCapture()` to fail.
unsafe fn park_starter_click_through(hwnd: HWND) {
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | (WS_EX_TRANSPARENT as isize));
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            -32000,
            -32000,
            1,
            1,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }
}

unsafe extern "system" fn starter_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_LBUTTONDOWN {
        let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut StarterState;
        if !raw.is_null() {
            unsafe {
                (*raw).got_lbutton_down = true;
            }
        }
        // Keep the window visible & active (foreground) so ole32!DoDragDrop's SetCapture()
        // succeeds globally, while immediately making it click-through and moving it off-screen
        // so WindowFromPoint hits File Explorer / Desktop / target app underneath!
        unsafe {
            park_starter_click_through(hwnd);
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn acquire_ole_drag_capture_window(
    session: &Arc<LiveOleSession>,
) -> Option<(HWND, bool)> {
    let mut pt = POINT { x: 200, y: 200 };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }

    // Cover the entire virtual desktop during the ~3ms capture click so fast inward mouse
    // motion can never escape the starter window before SendInput(LEFTDOWN) lands.
    let raw_vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let raw_vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let (vx, vy, vw, vh) = if raw_vw > 0 && raw_vh > 0 {
        (
            unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
            unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
            raw_vw,
            raw_vh,
        )
    } else {
        (pt.x - 64, pt.y - 64, 128, 128)
    };

    let class_name = encode_wide("GlideOleDragStarter");
    let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(starter_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    unsafe {
        RegisterClassW(&wc);
    }

    // Visually invisible (alpha = 1/255) & hidden from taskbar/Alt+Tab (WS_EX_TOOLWINDOW),
    // yet hit-testable & activatable so our synthetic LMB down directly grants this STA thread
    // foreground mouse-capture eligibility required by ole32!DoDragDrop's SetCapture().
    let ex_style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            ptr::null(),
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return None;
    }

    let mut starter_state = StarterState {
        got_lbutton_down: false,
    };
    unsafe {
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            &mut starter_state as *mut StarterState as isize,
        );
        SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);
        SetWindowPos(hwnd, HWND_TOPMOST, vx, vy, vw, vh, 0);
        ShowWindow(hwnd, SW_SHOW);
    }

    let phys_lmb_down = (unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000) != 0;
    if phys_lmb_down {
        // Server receiving Client -> Server drag while user physically holds LMB:
        // bounce UP -> DOWN (tagged GLIDE_EXTRA_INFO so mouse_ll_proc ignores both) onto
        // our invisible full-desktop starter window so ole32 receives valid mouse capture.
        let inputs = [
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_LEFTUP,
                        time: 0,
                        dwExtraInfo: GLIDE_EXTRA_INFO,
                    },
                },
            },
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_LEFTDOWN,
                        time: 0,
                        dwExtraInfo: GLIDE_EXTRA_INFO,
                    },
                },
            },
        ];
        unsafe {
            SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
        }
    } else {
        // Client receiving Server -> Client drag (or Server after remote LMB suppression):
        // synthesize LMB down onto our starter window and remember to release it later!
        session.synthesized_lmb_down.store(true, Ordering::Release);
        send_mouse_button_input(MOUSEEVENTF_LEFTDOWN);
    }

    let deadline = Instant::now() + Duration::from_millis(160);
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while !starter_state.got_lbutton_down
        && !session.cancel_requested.load(Ordering::Acquire)
        && Instant::now() < deadline
    {
        let mut pumped = false;
        while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            pumped = true;
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if starter_state.got_lbutton_down {
                break;
            }
        }
        if !pumped {
            thread::sleep(Duration::from_millis(2));
        }
    }

    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        if starter_state.got_lbutton_down {
            // Ensure click-through & off-screen WITHOUT calling ShowWindow(SW_HIDE)!
            park_starter_click_through(hwnd);
        } else {
            ShowWindow(hwnd, SW_HIDE);
        }
    }

    Some((hwnd, starter_state.got_lbutton_down))
}

/// Executes a live or post-release Windows Shell `SHDoDragDrop` session on a dedicated STA thread,
/// falling back cleanly to `CF_HDROP` clipboard + cursor hint card if the target rejects the drop.
fn run_windows_ole_drag_session(
    paths: &[PathBuf],
    mode: DragRunMode,
    session: &Arc<LiveOleSession>,
    shared: &Arc<SharedDnd>,
) -> Result<SessionResult> {
    let hr_init = unsafe { OleInitialize(ptr::null()) };
    if hr_init < 0 {
        session.done.store(true, Ordering::Release);
        warn!(
            hr = hr_init,
            ?mode,
            "OleInitialize failed during drop handoff"
        );
        if mode == DragRunMode::Live {
            return Ok(SessionResult::LiveDeferred);
        }
        shared.post_preview_hide();
        set_clipboard_hdrop(paths)?;
        spawn_windows_fallback_hint();
        return Ok(SessionResult::Outcome(DropDeliveryOutcome::ClipboardFallback));
    }

    let data_obj = unsafe { create_shell_data_object(paths) }
        .unwrap_or_else(|| FileDataObject::new_raw(paths.to_vec()));
    unsafe {
        attach_shell_drag_image(data_obj, paths);
    }

    let starter = acquire_ole_drag_capture_window(session);
    let (starter_hwnd, got_capture) = starter.unwrap_or((ptr::null_mut(), false));

    let feedback_seen = Arc::new(AtomicBool::new(false));
    let user_escaped = Arc::new(AtomicBool::new(false));
    let drop_src = LiveDropSource::new_raw(
        mode,
        Arc::clone(session),
        Arc::clone(shared),
        Arc::clone(&feedback_seen),
        Arc::clone(&user_escaped),
    );

    let ole_started = Instant::now();
    let mut effect: u32 = DROPEFFECT_NONE;
    let hr = if got_capture && !session.cancel_requested.load(Ordering::Acquire) {
        let session_bg = Arc::clone(session);
        let fb_bg = Arc::clone(&feedback_seen);
        let synth_bg = session.synthesized_lmb_down.load(Ordering::Acquire);
        let _kicker = thread::spawn(move || {
            // Wake SHDoDragDrop immediately with a real (+1, 0), (-1, 0) pointer wiggle so
            // Explorer runs DragEnter and displays the native Shell drag image + "+ Copy to"
            // badge even if the user holds the cursor perfectly still!
            thread::sleep(Duration::from_millis(15));
            if !session_bg.done.load(Ordering::Acquire) {
                send_mouse_wiggle();
            }
            if mode == DragRunMode::ImmediatePostRelease {
                for delay in [35u64, 65, 110] {
                    thread::sleep(Duration::from_millis(delay));
                    if session_bg.done.load(Ordering::Acquire) {
                        return;
                    }
                    if fb_bg.load(Ordering::Acquire) {
                        if synth_bg {
                            send_mouse_button_input(MOUSEEVENTF_LEFTUP);
                        }
                        send_mouse_wiggle();
                        return;
                    }
                    send_mouse_wiggle();
                }
                if !session_bg.done.load(Ordering::Acquire) && synth_bg {
                    send_mouse_button_input(MOUSEEVENTF_LEFTUP);
                }
            }
        });

        // Passing `ptr::null_mut()` preserves our custom `IDragSourceHelper2::InitializeFromBitmap`
        // drag image (avoiding Shell's `DI_GETDRAGIMAGE` override), while `starter_hwnd` staying
        // visible & foreground on this same STA thread grants `ole32!DoDragDrop` valid `SetCapture`.
        unsafe { SHDoDragDrop(ptr::null_mut(), data_obj, drop_src, DROPEFFECT_COPY, &mut effect) }
    } else {
        debug!(
            got_capture,
            cancelled = session.cancel_requested.load(Ordering::Acquire),
            "Could not capture starter click for SHDoDragDrop (likely elevated UIPI window); deferring/falling back"
        );
        DRAGDROP_S_CANCEL
    };
    let ole_elapsed_ms = ole_started.elapsed().as_millis() as u64;
    let lmb_still_down =
        (unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000) != 0;

    session.done.store(true, Ordering::Release);

    if !starter_hwnd.is_null() {
        unsafe {
            DestroyWindow(starter_hwnd);
        }
    }

    // Guarantee we NEVER leave LMB logically stuck down if we synthesized MOUSEEVENTF_LEFTDOWN.
    if session.synthesized_lmb_down.swap(false, Ordering::AcqRel)
        && (unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000) != 0
    {
        send_mouse_button_input(MOUSEEVENTF_LEFTUP);
    }

    unsafe {
        com_release(data_obj);
        com_release(drop_src);
        OleUninitialize();
    }

    trace!(
        ?mode,
        hr,
        effect,
        got_capture,
        lmb_still_down,
        ole_elapsed_ms,
        "Windows Shell SHDoDragDrop completed"
    );
    if hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE {
        shared.post_preview_hide();
        debug!(
            count = paths.len(),
            effect,
            "Delivered files directly via Windows Shell SHDoDragDrop"
        );
        return Ok(SessionResult::Outcome(
            DropDeliveryOutcome::DroppedIntoTarget,
        ));
    }

    let cancelled_or_escaped =
        session.cancel_requested.load(Ordering::Acquire) || user_escaped.load(Ordering::Acquire);
    if cancelled_or_escaped {
        shared.post_preview_hide();
        debug!("Windows Shell SHDoDragDrop cancelled cleanly");
        return Ok(SessionResult::Cancelled);
    }

    let drop_requested = session.drop_requested.load(Ordering::Acquire);
    if should_defer_live_ole_drag(
        mode == DragRunMode::Live,
        drop_requested,
        cancelled_or_escaped,
        got_capture,
        hr == DRAGDROP_S_DROP,
        lmb_still_down,
        ole_elapsed_ms,
    ) {
        debug!(
            got_capture,
            hr,
            effect,
            lmb_still_down,
            ole_elapsed_ms,
            "Live Windows OLE drag exited before real drop; keeping preview visible until LMB release"
        );
        // If GiveFeedback had briefly hidden our floating preview before SHDoDragDrop exited
        // early, restore it ONLY while this exact session is still the active `live_session`
        // (holding `live_session` across `preview` + `post_preview_show` serializes against
        // `on_incoming_drag_cancel` and `deliver_drop` so the preview can never get stuck).
        if feedback_seen.load(Ordering::Acquire) {
            let cur = shared.live_session.lock();
            if cur.as_ref().is_some_and(|s| Arc::ptr_eq(s, session))
                && !session.drop_requested.load(Ordering::Acquire)
                && !session.cancel_requested.load(Ordering::Acquire)
            {
                shared.preview.lock().visible = true;
                shared.post_preview_show();
            }
        }
        return Ok(SessionResult::LiveDeferred);
    }

    shared.post_preview_hide();
    debug!(
        hr,
        effect,
        "Windows OLE target did not accept drop; publishing CF_HDROP clipboard fallback"
    );
    set_clipboard_hdrop(paths)?;
    spawn_windows_fallback_hint();
    Ok(SessionResult::Outcome(
        DropDeliveryOutcome::ClipboardFallback,
    ))
}

/// Publishes `CF_HDROP` onto the Windows clipboard so the user can immediately press `Ctrl+V`.
fn set_clipboard_hdrop(paths: &[PathBuf]) -> Result<()> {
    let hglobal = build_hdrop_global(paths)?;
    let mut opened = false;
    for _ in 0..8 {
        if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        thread::sleep(Duration::from_millis(8));
    }
    if !opened {
        unsafe {
            GlobalFree(hglobal);
        }
        return Err(anyhow!("failed to open Windows clipboard for CF_HDROP fallback"));
    }

    let ok = unsafe {
        EmptyClipboard();
        !SetClipboardData(CF_HDROP as u32, hglobal as HANDLE).is_null()
    };
    unsafe {
        CloseClipboard();
    }
    if !ok {
        unsafe {
            GlobalFree(hglobal);
        }
        return Err(anyhow!("SetClipboardData(CF_HDROP) failed"));
    }
    debug!(
        count = paths.len(),
        "Published CF_HDROP file list to Windows clipboard"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Cursor-adjacent fallback hint popup (pure Win32 GDI, click-through, non-blocking)
// ---------------------------------------------------------------------------

const fn win_rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

const HINT_TIMER_ID: usize = 1;
const HINT_DURATION_MS: u32 = 3500;

/// Spawns a non-blocking, click-through, non-focus-stealing floating white card next to the
/// cursor informing the user the target app didn't allow the drop and the file was saved to
/// `Downloads/Glide` + copied for `Ctrl+V`. Auto-closes after 3.5 seconds.
fn spawn_windows_fallback_hint() {
    let _ = thread::Builder::new()
        .name("glide-win-dnd-hint".into())
        .spawn(|| {
            show_windows_fallback_hint_blocking();
        });
}

fn encode_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn hint_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use crate::dnd::{FALLBACK_HINT_SUBTITLE, FALLBACK_HINT_TITLE};
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
        EndPaint, RoundRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
        CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_LEFT, DT_NOPREFIX,
        DT_SINGLELINE, DT_VCENTER, FF_SWISS, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID,
        TRANSPARENT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClientRect, KillTimer, PostQuitMessage, WM_ERASEBKGND, WM_PAINT, WM_TIMER,
    };

    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            if !hdc.is_null() {
                let mut rc = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                unsafe {
                    GetClientRect(hwnd, &mut rc);
                }
                let w = (rc.right - rc.left).max(1);
                let h = (rc.bottom - rc.top).max(1);
                let scale = (h as f32 / 56.0).clamp(0.85, 3.0);
                let px = |v: f32| (v * scale).round() as i32;

                // White card background (#FFFFFF) + subtle hairline border (#E2E8F0)
                let bg_brush = unsafe { CreateSolidBrush(win_rgb(0xFF, 0xFF, 0xFF)) };
                let border_pen = unsafe { CreatePen(PS_SOLID, px(1.0).max(1), win_rgb(0xE2, 0xE8, 0xF0)) };
                let old_brush = unsafe { SelectObject(hdc, bg_brush) };
                let old_pen = unsafe { SelectObject(hdc, border_pen) };
                let corner = px(20.0);
                unsafe {
                    RoundRect(hdc, 0, 0, w, h, corner, corner);
                }

                // Small blue info circle (#2563EB) with centered white 'i'
                let icon_d = px(20.0);
                let icon_x = px(13.0);
                let icon_y = (h - icon_d) / 2;
                let blue_brush = unsafe { CreateSolidBrush(win_rgb(0x25, 0x63, 0xEB)) };
                let blue_pen = unsafe { CreatePen(PS_SOLID, 1, win_rgb(0x25, 0x63, 0xEB)) };
                unsafe {
                    SelectObject(hdc, blue_brush);
                    SelectObject(hdc, blue_pen);
                    Ellipse(hdc, icon_x, icon_y, icon_x + icon_d, icon_y + icon_d);
                }

                let face = encode_wide("Segoe UI");
                let icon_font = unsafe {
                    CreateFontW(
                        -px(13.0),
                        0,
                        0,
                        0,
                        700,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET as u32,
                        OUT_DEFAULT_PRECIS as u32,
                        CLIP_DEFAULT_PRECIS as u32,
                        CLEARTYPE_QUALITY as u32,
                        (DEFAULT_PITCH | FF_SWISS) as u32,
                        face.as_ptr(),
                    )
                };
                let title_font = unsafe {
                    CreateFontW(
                        -px(13.0),
                        0,
                        0,
                        0,
                        600,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET as u32,
                        OUT_DEFAULT_PRECIS as u32,
                        CLIP_DEFAULT_PRECIS as u32,
                        CLEARTYPE_QUALITY as u32,
                        (DEFAULT_PITCH | FF_SWISS) as u32,
                        face.as_ptr(),
                    )
                };
                let sub_font = unsafe {
                    CreateFontW(
                        -px(12.0),
                        0,
                        0,
                        0,
                        400,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET as u32,
                        OUT_DEFAULT_PRECIS as u32,
                        CLIP_DEFAULT_PRECIS as u32,
                        CLEARTYPE_QUALITY as u32,
                        (DEFAULT_PITCH | FF_SWISS) as u32,
                        face.as_ptr(),
                    )
                };

                unsafe {
                    SetBkMode(hdc, TRANSPARENT as i32);
                }
                let old_font = unsafe { SelectObject(hdc, icon_font) };
                unsafe {
                    SetTextColor(hdc, win_rgb(0xFF, 0xFF, 0xFF));
                }
                let mut icon_rc = RECT {
                    left: icon_x,
                    top: icon_y,
                    right: icon_x + icon_d,
                    bottom: icon_y + icon_d,
                };
                let glyph_i = encode_wide("i");
                unsafe {
                    DrawTextW(
                        hdc,
                        glyph_i.as_ptr(),
                        1,
                        &mut icon_rc,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }

                let text_x = icon_x + icon_d + px(10.0);
                let text_right = w - px(12.0);
                let title_w = encode_wide(FALLBACK_HINT_TITLE);
                let sub_w = encode_wide(FALLBACK_HINT_SUBTITLE);

                let mut title_rc = RECT {
                    left: text_x,
                    top: px(9.0),
                    right: text_right,
                    bottom: px(28.0),
                };
                unsafe {
                    SelectObject(hdc, title_font);
                    SetTextColor(hdc, win_rgb(0x0F, 0x17, 0x2A));
                    DrawTextW(
                        hdc,
                        title_w.as_ptr(),
                        -1,
                        &mut title_rc,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }

                let mut sub_rc = RECT {
                    left: text_x,
                    top: px(28.0),
                    right: text_right,
                    bottom: h - px(8.0),
                };
                unsafe {
                    SelectObject(hdc, sub_font);
                    SetTextColor(hdc, win_rgb(0x47, 0x55, 0x69));
                    DrawTextW(
                        hdc,
                        sub_w.as_ptr(),
                        -1,
                        &mut sub_rc,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }

                unsafe {
                    SelectObject(hdc, old_font);
                    SelectObject(hdc, old_brush);
                    SelectObject(hdc, old_pen);
                    DeleteObject(icon_font);
                    DeleteObject(title_font);
                    DeleteObject(sub_font);
                    DeleteObject(blue_brush);
                    DeleteObject(blue_pen);
                    DeleteObject(bg_brush);
                    DeleteObject(border_pen);
                    EndPaint(hwnd, &ps);
                }
            }
            0
        }
        WM_TIMER if wparam == HINT_TIMER_ID => {
            unsafe {
                KillTimer(hwnd, HINT_TIMER_ID);
                DestroyWindow(hwnd);
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn show_windows_fallback_hint_blocking() {
    use crate::dnd::clamp_hint_popup_pos;
    use windows_sys::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, SetTimer, WS_EX_TRANSPARENT,
    };

    init_dpi_awareness();
    let mut pt = POINT { x: 200, y: 200 };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let geom = query_desktop_geometry().unwrap_or_default();
    let scale = geom
        .monitors
        .iter()
        .find(|m| m.contains(pt.x, pt.y))
        .map(|m| m.scale)
        .unwrap_or_else(|| geom.primary_scale())
        .clamp(1.0, 3.0);

    let width = (336.0 * scale).round() as i32;
    let height = (56.0 * scale).round() as i32;
    let radius = (20.0 * scale).round() as i32;
    let (x, y) = clamp_hint_popup_pos(&geom, pt.x, pt.y, width, height);

    let class_name = encode_wide("GlideDndFallbackHintWindow");
    let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(hint_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    unsafe {
        RegisterClassW(&wc);
    }

    let ex_style =
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            class_name.as_ptr(),
            ptr::null(),
            WS_POPUP,
            x,
            y,
            width,
            height,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return;
    }

    unsafe {
        SetLayeredWindowAttributes(hwnd, 0, 248, LWA_ALPHA);
        let rgn = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius, radius);
        if !rgn.is_null() {
            SetWindowRgn(hwnd, rgn, 1);
        }
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, HINT_TIMER_ID, HINT_DURATION_MS, None);
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_windows_verbatim_prefix_normalizes_drive_and_unc_paths() {
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\C:\Users\Max\Downloads\Glide\a.pdf")),
            PathBuf::from(r"C:\Users\Max\Downloads\Glide\a.pdf")
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\UNC\nas\share\file.txt")),
            PathBuf::from(r"\\nas\share\file.txt")
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"C:\Users\Max\Desktop\ok.png")),
            PathBuf::from(r"C:\Users\Max\Desktop\ok.png")
        );
    }

    #[test]
    fn file_data_object_supports_hdrop_preferred_effect_and_set_data_roundtrip() {
        let sample = PathBuf::from(r"C:\Users\Max\Downloads\Glide\demo.txt");
        let obj = FileDataObject::new_raw(vec![sample.clone()]);
        let vtbl = unsafe { *(obj as *const *const IDataObjectVtbl) };

        // 1. CF_HDROP extraction roundtrips cleanly
        let extracted = unsafe { extract_hdrop_paths(obj) };
        assert_eq!(extracted, vec![sample]);

        // 2. Arbitrary Shell format SetData -> QueryGetData -> GetData roundtrips cleanly
        let custom_cf = preferred_drop_effect_format().saturating_add(7).max(0xC100);
        let hglobal = build_dword_global(0xDEAD_BEEF).expect("build_dword_global");
        let mut fmt = FORMATETC {
            cfFormat: custom_cf,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL as u32,
        };
        let mut med_in = STGMEDIUM {
            tymed: TYMED_HGLOBAL as u32,
            u: STGMEDIUM_0 { hGlobal: hglobal },
            pUnkForRelease: ptr::null_mut(),
        };
        assert_eq!(unsafe { ((*vtbl).set_data)(obj, &mut fmt, &mut med_in, 1) }, S_OK);
        assert_eq!(unsafe { ((*vtbl).query_get_data)(obj, &mut fmt) }, S_OK);

        let mut med_out: STGMEDIUM = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { ((*vtbl).get_data)(obj, &mut fmt, &mut med_out) }, S_OK);
        assert_eq!(med_out.tymed, TYMED_HGLOBAL as u32);
        let read_back = unsafe {
            let g = med_out.u.hGlobal;
            let p = GlobalLock(g) as *const u32;
            let v = *p;
            GlobalUnlock(g);
            v
        };
        assert_eq!(read_back, 0xDEAD_BEEF);
        unsafe {
            ReleaseStgMedium(&mut med_out);
            com_release(obj);
        }
    }
}
