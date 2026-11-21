//! Pure-Rust Windows 11 cross-edge file drag-and-drop backend via Win32 COM/OLE (`windows-sys`).
//!
//! - **Drag-out (Windows -> Peer):** Registers pure-Rust COM `IDropTarget` instances on
//!   invisible 1px topmost layered edge-strip windows (`WS_EX_LAYERED | WS_EX_TOPMOST |
//!   WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`) placed along active outer edges (inset by
//!   `dead_corner_px`). Strips remain hidden (`SW_HIDE`) at all times except while a local
//!   left-button drag is armed so normal clicks, scrollbars, and window resizing are never
//!   obstructed. Extracts dragged local paths from `CF_HDROP` (`DragQueryFileW`).
//! - **Drag-in (Peer -> Windows):** After files are saved to `%USERPROFILE%\Downloads\Glide\`,
//!   runs Windows OLE `DoDragDrop` with a pure-Rust `CF_HDROP` `IDataObject`, `IEnumFORMATETC`,
//!   and `IDropSource` under the current cursor, automatically falling back to `CF_HDROP` on the
//!   Windows clipboard if OLE drag-drop is rejected or blocked (e.g. non-drop window or UIPI).

use crate::dnd::{compute_edge_strip_rects, DndBackend, DropDeliveryOutcome, EdgeStripRect};
use crate::windows::{init_dpi_awareness, query_desktop_geometry, GLIDE_EXTRA_INFO};
use anyhow::{anyhow, Context, Result};
use glide_proto::{Edge, MAX_DRAG_FILES};
use parking_lot::Mutex;
use std::ffi::{c_void, OsString};
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
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
    OLE_E_ADVISENOTSUPPORTED, POINT, POINTL, S_FALSE, S_OK, WPARAM,
};
use windows_sys::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL, TYMED_NULL,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows_sys::Win32::System::Ole::{
    DoDragDrop, OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
    CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostThreadMessageW, RegisterClassW, SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
    TranslateMessage, HWND_MESSAGE, HWND_TOPMOST, LWA_ALPHA, MSG, SWP_NOACTIVATE, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_APP, WM_MOUSEMOVE, WM_QUIT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const WM_APP_SYNC_STRIPS: u32 = WM_APP + 21;
const WM_APP_HIDE_STRIPS: u32 = WM_APP + 22;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x0000_0000_0000_0000_c000_0000_0000_0046);
const IID_IDROPTARGET: GUID = GUID::from_u128(0x0000_0122_0000_0000_c000_0000_0000_0046);
const IID_IDROPSOURCE: GUID = GUID::from_u128(0x0000_0121_0000_0000_c000_0000_0000_0046);
const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000_010e_0000_0000_c000_0000_0000_0046);
const IID_IENUMFORMATETC: GUID = GUID::from_u128(0x0000_0103_0000_0000_c000_0000_0000_0046);

const STAGED_MAX_AGE: Duration = Duration::from_secs(4);
const STAGED_LEAVE_GRACE: Duration = Duration::from_millis(350);

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

// ---------------------------------------------------------------------------
// Shared state between DndBackend handle and STA OLE window thread
// ---------------------------------------------------------------------------

struct StagedDrag {
    edge: Edge,
    paths: Vec<PathBuf>,
    staged_at: Instant,
    left_at: Option<Instant>,
}

struct SharedDnd {
    enabled: AtomicBool,
    armed: AtomicBool,
    dead_corner_px: AtomicI32,
    ctrl_hwnd: AtomicIsize,
    active_edges: Mutex<Vec<Edge>>,
    staged: Mutex<Option<StagedDrag>>,
}

impl SharedDnd {
    fn post_hide_strips(&self) {
        let hwnd = self.ctrl_hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_HIDE_STRIPS, 0, 0);
            }
        }
    }
}

pub struct WindowsDndBackend {
    shared: Arc<SharedDnd>,
}

impl WindowsDndBackend {
    fn post_sync(&self) {
        let hwnd = self.shared.ctrl_hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_SYNC_STRIPS, 0, 0);
            }
        }
    }
}

impl Drop for WindowsDndBackend {
    fn drop(&mut self) {
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
            *self.shared.staged.lock() = None;
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
            // Delay clearing staged files slightly ONLY if left_at is unset; on full button up
            // the local drag has ended so clear stale state immediately.
            *self.shared.staged.lock() = None;
        }
        if prev != effective {
            self.post_sync();
        }
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
            return None;
        }
        if let Some(left) = staged.left_at {
            if left.elapsed() > STAGED_LEAVE_GRACE {
                *guard = None;
                return None;
            }
        }
        let paths = guard.take()?.paths;
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }

    fn cancel_local_drag(&self) {
        self.shared.armed.store(false, Ordering::SeqCst);
        *self.shared.staged.lock() = None;
        self.shared.post_hide_strips();
        // Send a synthetic Escape tap tagged with GLIDE_EXTRA_INFO so Explorer's modal
        // DoDragDrop loop cancels cleanly without moving/deleting files and our LL keyboard
        // hook ignores the injected key.
        send_synthetic_escape();
    }

    fn deliver_drop(&self, paths: Vec<PathBuf>) -> Result<DropDeliveryOutcome> {
        if paths.is_empty() {
            return Ok(DropDeliveryOutcome::ClipboardFallback);
        }
        // Run OLE DoDragDrop on a dedicated STA thread so OleInitialize + message pumping
        // never contend with the edge-strip thread or Tokio workers.
        let paths_clone = paths.clone();
        let outcome = thread::Builder::new()
            .name("glide-win-ole-drop".into())
            .spawn(move || perform_ole_drop_or_fallback(&paths_clone))
            .context("failed to spawn Windows OLE drop thread")?
            .join()
            .map_err(|_| anyhow!("Windows OLE drop thread panicked"))??;
        Ok(outcome)
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
// STA thread hosting invisible 1px topmost edge-strip IDropTarget windows
// ---------------------------------------------------------------------------

struct StripWindow {
    hwnd: HWND,
    rect: EdgeStripRect,
    visible: bool,
}

struct StripThreadCtx {
    shared: Arc<SharedDnd>,
    strip_class: Vec<u16>,
    strips: Vec<StripWindow>,
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
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
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

    if !enabled || !armed || edges.is_empty() {
        hide_all_strips(&mut ctx.strips);
        return;
    }

    let geom = match query_desktop_geometry() {
        Ok(g) => g,
        Err(_) => return,
    };
    let desired = compute_edge_strip_rects(&geom, &edges, dead_corner, 1);

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

    for s in &mut ctx.strips {
        unsafe {
            SetWindowPos(
                s.hwnd,
                HWND_TOPMOST,
                s.rect.x,
                s.rect.y,
                s.rect.width,
                s.rect.height,
                SWP_NOACTIVATE,
            );
            if !s.visible {
                ShowWindow(s.hwnd, SW_SHOWNOACTIVATE);
                s.visible = true;
            }
        }
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
    unsafe {
        RegisterClassW(&ctrl_wc);
        RegisterClassW(&strip_wc);
    }

    let ctrl_hwnd = unsafe {
        CreateWindowExW(
            0,
            ctrl_class.as_ptr(),
            ctrl_class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
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
    debug!("Windows OLE DnD edge-strip thread ready");

    let mut ctx = StripThreadCtx {
        shared,
        strip_class,
        strips: Vec::new(),
    };

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
        if msg.hwnd == ctrl_hwnd {
            match msg.message {
                WM_APP_SYNC_STRIPS => {
                    sync_strips(&mut ctx);
                    continue;
                }
                WM_APP_HIDE_STRIPS => {
                    hide_all_strips(&mut ctx.strips);
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
                    wVk: 0,
                    wScan: 0x01, // Escape scancode
                    dwFlags: KEYEVENTF_SCANCODE,
                    time: 0,
                    dwExtraInfo: GLIDE_EXTRA_INFO,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: 0x01,
                    dwFlags: KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP,
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
    let paths = unsafe { extract_hdrop_paths(data_obj) };
    if !paths.is_empty() {
        debug!(
            edge = ?me.edge,
            count = paths.len(),
            "Captured CF_HDROP files on Windows edge strip"
        );
        *me.shared.staged.lock() = Some(StagedDrag {
            edge: me.edge,
            paths,
            staged_at: Instant::now(),
            left_at: None,
        });
        if !pdw_effect.is_null() {
            unsafe {
                *pdw_effect = DROPEFFECT_COPY;
            }
        }
    } else {
        // Non-file OLE drag (e.g. selected text or browser tab): do not intercept,
        // disarm & hide the edge strip immediately so normal desktop dragging is unimpeded.
        me.shared.armed.store(false, Ordering::SeqCst);
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
    let mut guard = me.shared.staged.lock();
    let has_files = if let Some(ref mut staged) = *guard {
        if staged.edge == me.edge {
            staged.staged_at = Instant::now();
            staged.left_at = None;
            true
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
    let paths = unsafe { extract_hdrop_paths(data_obj) };
    if !paths.is_empty() {
        *me.shared.staged.lock() = Some(StagedDrag {
            edge: me.edge,
            paths,
            staged_at: Instant::now(),
            left_at: Some(Instant::now()),
        });
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
// Drag-in (Peer -> Windows): CF_HDROP IDataObject + IEnumFORMATETC + IDropSource
// ---------------------------------------------------------------------------

fn build_hdrop_global(paths: &[PathBuf]) -> Result<HGLOBAL> {
    let mut wide_list: Vec<u16> = Vec::new();
    for path in paths {
        let s: Vec<u16> = path.as_os_str().encode_wide().collect();
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
        return Err(anyhow!("GlobalAlloc failed for CF_HDROP ({total_bytes} bytes)"));
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

fn hdrop_formatetc() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    }
}

fn is_supported_hdrop_format(fmt: &FORMATETC) -> Result<(), HRESULT> {
    if fmt.cfFormat != CF_HDROP {
        return Err(DV_E_FORMATETC);
    }
    if (fmt.dwAspect & DVASPECT_CONTENT) == 0 {
        return Err(DV_E_FORMATETC);
    }
    if fmt.lindex != -1 && fmt.lindex != 0 {
        return Err(DV_E_FORMATETC);
    }
    if (fmt.tymed & (TYMED_HGLOBAL as u32)) == 0 {
        return Err(DV_E_TYMED);
    }
    Ok(())
}

// --- IEnumFORMATETC ---

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
struct SingleFormatEnum {
    vtbl: *const IEnumFormatEtcVtbl,
    ref_count: AtomicU32,
    index: AtomicU32,
}

static SINGLE_FORMAT_ENUM_VTBL: IEnumFormatEtcVtbl = IEnumFormatEtcVtbl {
    query_interface: enum_fmt_query_interface,
    add_ref: enum_fmt_add_ref,
    release: enum_fmt_release,
    next: enum_fmt_next,
    skip: enum_fmt_skip,
    reset: enum_fmt_reset,
    clone: enum_fmt_clone,
};

impl SingleFormatEnum {
    fn new_raw(index: u32) -> *mut c_void {
        Box::into_raw(Box::new(Self {
            vtbl: &SINGLE_FORMAT_ENUM_VTBL,
            ref_count: AtomicU32::new(1),
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
    let me = unsafe { &*(this as *const SingleFormatEnum) };
    me.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn enum_fmt_release(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const SingleFormatEnum) };
    let prev = me.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this as *mut SingleFormatEnum));
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
    let me = unsafe { &*(this as *const SingleFormatEnum) };
    let cur = me.index.load(Ordering::Relaxed);
    let fetched = if celt > 0 && cur == 0 {
        me.index.store(1, Ordering::Relaxed);
        unsafe {
            *rgelt = hdrop_formatetc();
        }
        1
    } else {
        0
    };
    if !pcelt_fetched.is_null() {
        unsafe {
            *pcelt_fetched = fetched;
        }
    }
    if fetched == celt {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn enum_fmt_skip(this: *mut c_void, celt: u32) -> HRESULT {
    let me = unsafe { &*(this as *const SingleFormatEnum) };
    let cur = me.index.load(Ordering::Relaxed);
    let next = cur.saturating_add(celt);
    me.index.store(next.min(1), Ordering::Relaxed);
    if next <= 1 {
        S_OK
    } else {
        S_FALSE
    }
}

unsafe extern "system" fn enum_fmt_reset(this: *mut c_void) -> HRESULT {
    let me = unsafe { &*(this as *const SingleFormatEnum) };
    me.index.store(0, Ordering::Relaxed);
    S_OK
}

unsafe extern "system" fn enum_fmt_clone(this: *mut c_void, ppenum: *mut *mut c_void) -> HRESULT {
    if ppenum.is_null() {
        return E_POINTER;
    }
    let me = unsafe { &*(this as *const SingleFormatEnum) };
    let idx = me.index.load(Ordering::Relaxed);
    unsafe {
        *ppenum = SingleFormatEnum::new_raw(idx);
    }
    S_OK
}

// --- IDataObject ---

#[repr(C)]
struct FileDataObject {
    vtbl: *const IDataObjectVtbl,
    ref_count: AtomicU32,
    paths: Vec<PathBuf>,
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
    if let Err(hr) = is_supported_hdrop_format(fmt) {
        return hr;
    }
    let me = unsafe { &*(this as *const FileDataObject) };
    match build_hdrop_global(&me.paths) {
        Ok(hglobal) => {
            unsafe {
                (*pmedium).tymed = TYMED_HGLOBAL as u32;
                (*pmedium).u = STGMEDIUM_0 { hGlobal: hglobal };
                (*pmedium).pUnkForRelease = ptr::null_mut();
            }
            S_OK
        }
        Err(_) => E_OUTOFMEMORY,
    }
}

unsafe extern "system" fn data_obj_get_data_here(
    _this: *mut c_void,
    _pformatetc: *mut FORMATETC,
    _pmedium: *mut STGMEDIUM,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn data_obj_query_get_data(
    _this: *mut c_void,
    pformatetc: *mut FORMATETC,
) -> HRESULT {
    if pformatetc.is_null() {
        return E_INVALIDARG;
    }
    let fmt = unsafe { &*pformatetc };
    match is_supported_hdrop_format(fmt) {
        Ok(()) => S_OK,
        Err(hr) => hr,
    }
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
    _this: *mut c_void,
    _pformatetc: *mut FORMATETC,
    _pmedium: *mut STGMEDIUM,
    _frelease: BOOL,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn data_obj_enum_format_etc(
    _this: *mut c_void,
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
        unsafe {
            *ppenum = SingleFormatEnum::new_raw(0);
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

// --- IDropSource ---

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
struct ImmediateDropSource {
    vtbl: *const IDropSourceVtbl,
    ref_count: AtomicU32,
    feedback_seen: AtomicBool,
    queries: AtomicU32,
    started: Instant,
}

static IMMEDIATE_DROP_SOURCE_VTBL: IDropSourceVtbl = IDropSourceVtbl {
    query_interface: drop_src_query_interface,
    add_ref: drop_src_add_ref,
    release: drop_src_release,
    query_continue_drag: drop_src_query_continue_drag,
    give_feedback: drop_src_give_feedback,
};

impl ImmediateDropSource {
    fn new_raw() -> *mut c_void {
        Box::into_raw(Box::new(Self {
            vtbl: &IMMEDIATE_DROP_SOURCE_VTBL,
            ref_count: AtomicU32::new(1),
            feedback_seen: AtomicBool::new(false),
            queries: AtomicU32::new(0),
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
    let me = unsafe { &*(this as *const ImmediateDropSource) };
    me.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn drop_src_release(this: *mut c_void) -> u32 {
    let me = unsafe { &*(this as *const ImmediateDropSource) };
    let prev = me.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this as *mut ImmediateDropSource));
        }
    }
    prev - 1
}

unsafe extern "system" fn drop_src_query_continue_drag(
    this: *mut c_void,
    escape_pressed: BOOL,
    _key_state: u32,
) -> HRESULT {
    if escape_pressed != 0 {
        return DRAGDROP_S_CANCEL;
    }
    let me = unsafe { &*(this as *const ImmediateDropSource) };
    let q = me.queries.fetch_add(1, Ordering::Relaxed);
    if me.feedback_seen.load(Ordering::Acquire) || q >= 1 {
        return DRAGDROP_S_DROP;
    }
    if me.started.elapsed() > Duration::from_millis(400) {
        return DRAGDROP_S_CANCEL;
    }
    S_OK
}

unsafe extern "system" fn drop_src_give_feedback(this: *mut c_void, _dw_effect: u32) -> HRESULT {
    let me = unsafe { &*(this as *const ImmediateDropSource) };
    me.feedback_seen.store(true, Ordering::Release);
    DRAGDROP_S_USEDEFAULTCURSORS
}

/// Attempts an OLE `DoDragDrop` drop handoff under the current cursor, falling back cleanly
/// to `CF_HDROP` on the Windows clipboard if the target window rejects or cannot receive OLE drops.
fn perform_ole_drop_or_fallback(paths: &[PathBuf]) -> Result<DropDeliveryOutcome> {
    let hr_init = unsafe { OleInitialize(ptr::null()) };
    if hr_init < 0 {
        warn!(hr = hr_init, "OleInitialize failed during drop handoff; using clipboard fallback");
        set_clipboard_hdrop(paths)?;
        spawn_windows_fallback_hint();
        return Ok(DropDeliveryOutcome::ClipboardFallback);
    }

    let tid = unsafe { GetCurrentThreadId() };
    let done = Arc::new(AtomicBool::new(false));
    let done_bg = Arc::clone(&done);
    // Wake DoDragDrop's modal loop cleanly if the user's pointer is stationary after releasing LMB.
    let _nudge = thread::spawn(move || {
        for delay in [15u64, 40, 80, 150] {
            thread::sleep(Duration::from_millis(delay));
            if done_bg.load(Ordering::Acquire) {
                return;
            }
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE,
                        time: 0,
                        dwExtraInfo: GLIDE_EXTRA_INFO,
                    },
                },
            };
            unsafe {
                SendInput(1, &input, size_of::<INPUT>() as i32);
                PostThreadMessageW(tid, WM_MOUSEMOVE, 0, 0);
            }
        }
    });

    let data_obj = FileDataObject::new_raw(paths.to_vec());
    let drop_src = ImmediateDropSource::new_raw();
    let mut effect: u32 = DROPEFFECT_NONE;

    let hr = unsafe { DoDragDrop(data_obj, drop_src, DROPEFFECT_COPY, &mut effect) };
    done.store(true, Ordering::Release);

    unsafe {
        com_release(data_obj);
        com_release(drop_src);
        OleUninitialize();
    }

    trace!(hr, effect, "Windows OLE DoDragDrop completed");
    if hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE {
        debug!(count = paths.len(), effect, "Delivered files via Windows OLE DoDragDrop");
        return Ok(DropDeliveryOutcome::DroppedIntoTarget);
    }

    debug!(
        hr,
        effect,
        "Windows OLE target did not accept drop; publishing CF_HDROP clipboard fallback"
    );
    set_clipboard_hdrop(paths)?;
    spawn_windows_fallback_hint();
    Ok(DropDeliveryOutcome::ClipboardFallback)
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
