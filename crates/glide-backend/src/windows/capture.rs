//! Windows 11 server input capture using Raw Input (`WM_INPUT`) for true unaccelerated
//! mouse deltas + high-priority `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks with German
//! `AltGr` fake-LCtrl filtering.

use crate::geometry::DesktopGeometry;
use crate::windows::{init_dpi_awareness, query_desktop_geometry, GLIDE_EXTRA_INFO};
use crate::{CaptureConfig, CaptureControl, CaptureEvent, CaptureHandle};
use anyhow::{anyhow, Context, Result};
use glide_hid::{is_windows_fake_altgr_lctrl, HidUsage, MouseButton, WinScan};
use glide_proto::Edge;
use parking_lot::Mutex;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW,
    PostMessageW, RegisterClassW, SetCursorPos, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, XBUTTON1, XBUTTON2,
};

const WM_APP_ENTER_REMOTE: u32 = WM_APP + 1;
const WM_APP_LEAVE_REMOTE: u32 = WM_APP + 2;

// Windows Virtual-Key constants used to disambiguate WH_KEYBOARD_LL edge cases.
const VK_PAUSE: u32 = 0x13;
const VK_SNAPSHOT: u32 = 0x2C;
const VK_NUMLOCK: u32 = 0x90;
const VK_RSHIFT: u32 = 0xA1;

struct HookState {
    config: CaptureConfig,
    tx: mpsc::UnboundedSender<CaptureEvent>,
    active_edges: Mutex<Vec<Edge>>,
    geometry: Mutex<DesktopGeometry>,
    remote_mode: AtomicBool,
    anchor_x: AtomicI32,
    anchor_y: AtomicI32,
    pending_leave: Mutex<Option<(Edge, f32)>>,
    edge_push: Mutex<(Option<Edge>, f32)>,
    start_instant: Instant,
    last_raw_mouse_ms: AtomicU64,
    last_geom_refresh_ms: AtomicU64,
}

impl HookState {
    fn elapsed_ms(&self) -> u64 {
        self.start_instant.elapsed().as_millis() as u64
    }

    fn maybe_refresh_geometry(&self) {
        let now = self.elapsed_ms();
        let prev = self.last_geom_refresh_ms.load(Ordering::Relaxed);
        if now.saturating_sub(prev) >= 5000
            && self
                .last_geom_refresh_ms
                .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            if let Ok(geom) = query_desktop_geometry() {
                *self.geometry.lock() = geom;
            }
        }
    }
}

static GLOBAL_HOOK_STATE: OnceLock<Mutex<Option<Arc<HookState>>>> = OnceLock::new();

fn with_hook_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&HookState) -> R,
{
    let cell = GLOBAL_HOOK_STATE.get()?;
    let guard = cell.lock();
    guard.as_deref().map(f)
}

struct WindowsCaptureControl {
    hwnd: AtomicIsize,
    state: Arc<HookState>,
}

impl CaptureControl for WindowsCaptureControl {
    fn set_active_edges(&self, edges: &[Edge]) {
        *self.state.active_edges.lock() = edges.to_vec();
    }

    fn enter_remote(&self) -> Result<()> {
        self.state.remote_mode.store(true, Ordering::SeqCst);
        let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_ENTER_REMOTE, 0, 0);
            }
        }
        Ok(())
    }

    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()> {
        *self.state.pending_leave.lock() = Some((return_edge, normalized_pos));
        self.state.remote_mode.store(false, Ordering::SeqCst);
        let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_LEAVE_REMOTE, 0, 0);
            }
        }
        Ok(())
    }

    fn desktop_geometry(&self) -> DesktopGeometry {
        self.state.geometry.lock().clone()
    }
}

fn translate_ll_key(vk_code: u32, scan_code: u32, flags: u32) -> Option<HidUsage> {
    let win_scan = match vk_code {
        VK_PAUSE => WinScan {
            code: 0x45,
            extended: false,
        },
        VK_NUMLOCK => WinScan {
            code: 0x45,
            extended: true,
        },
        VK_RSHIFT => WinScan {
            code: 0x36,
            extended: false,
        },
        VK_SNAPSHOT => WinScan {
            code: 0x37,
            extended: true,
        },
        _ => WinScan {
            code: (scan_code & 0xFF) as u16,
            extended: (flags & LLKHF_EXTENDED) != 0,
        },
    };
    HidUsage::from_win_scan(win_scan)
}

unsafe extern "system" fn keyboard_ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if kb.dwExtraInfo != GLIDE_EXTRA_INFO {
            let pressed = match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
                WM_KEYUP | WM_SYSKEYUP => Some(false),
                _ => None,
            };

            if let Some(pressed) = pressed {
                let suppress = with_hook_state(|state| {
                    let remote = state.remote_mode.load(Ordering::Acquire);

                    // CRITICAL FOR GERMAN / EU KEYBOARDS:
                    // Windows synthesizes a fake VK_LCONTROL (scanCode == 0x21D) immediately
                    // before VK_RMENU whenever AltGr is pressed. Swallow it in remote mode so
                    // Linux/macOS receives pure physical AltRight (HID 0xE6)!
                    if is_windows_fake_altgr_lctrl(kb.vkCode, kb.scanCode) {
                        return remote;
                    }

                    let Some(usage) = translate_ll_key(kb.vkCode, kb.scanCode, kb.flags) else {
                        trace!(
                            vk = kb.vkCode,
                            scan = kb.scanCode,
                            flags = kb.flags,
                            "Unmapped Win key"
                        );
                        return remote;
                    };

                    if usage == state.config.escape_key {
                        if pressed && remote {
                            state.remote_mode.store(false, Ordering::SeqCst);
                            let _ = state.tx.send(CaptureEvent::EscapeHotkey);
                            return true;
                        }
                        if remote {
                            return true;
                        }
                        return false;
                    }

                    if remote {
                        let _ = state.tx.send(CaptureEvent::Key { usage, pressed });
                        return true;
                    }

                    false
                })
                .unwrap_or(false);

                if suppress {
                    return 1;
                }
            }
        }
    }

    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if ms.dwExtraInfo != GLIDE_EXTRA_INFO {
            let msg = wparam as u32;
            let suppress = with_hook_state(|state| {
                let remote = state.remote_mode.load(Ordering::Acquire);
                if !remote {
                    return false;
                }

                match msg {
                    WM_MOUSEMOVE => {
                        // Primary motion path is Raw Input (WM_INPUT) for true unaccelerated deltas.
                        // Fallback for precision touchpads that don't emit relative WM_INPUT:
                        let now = state.elapsed_ms();
                        let last_raw = state.last_raw_mouse_ms.load(Ordering::Relaxed);
                        if now.saturating_sub(last_raw) > 100 {
                            let ax = state.anchor_x.load(Ordering::Relaxed);
                            let ay = state.anchor_y.load(Ordering::Relaxed);
                            let dx = (ms.pt.x - ax) as f32;
                            let dy = (ms.pt.y - ay) as f32;
                            if dx != 0.0 || dy != 0.0 {
                                let _ = state.tx.send(CaptureEvent::MouseDelta { dx, dy });
                            }
                        }
                        true
                    }
                    WM_LBUTTONDOWN => {
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Left,
                            pressed: true,
                        });
                        true
                    }
                    WM_LBUTTONUP => {
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Left,
                            pressed: false,
                        });
                        true
                    }
                    WM_RBUTTONDOWN => {
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Right,
                            pressed: true,
                        });
                        true
                    }
                    WM_RBUTTONUP => {
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Right,
                            pressed: false,
                        });
                        true
                    }
                    WM_MBUTTONDOWN => {
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Middle,
                            pressed: true,
                        });
                        true
                    }
                    WM_MBUTTONUP => {
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Middle,
                            pressed: false,
                        });
                        true
                    }
                    WM_XBUTTONDOWN | WM_XBUTTONUP => {
                        let pressed = msg == WM_XBUTTONDOWN;
                        let xbtn = ((ms.mouseData >> 16) & 0xFFFF) as u16;
                        let button = if xbtn == XBUTTON1 {
                            Some(MouseButton::Back)
                        } else if xbtn == XBUTTON2 {
                            Some(MouseButton::Forward)
                        } else {
                            None
                        };
                        if let Some(button) = button {
                            let _ = state.tx.send(CaptureEvent::MouseButton { button, pressed });
                        }
                        true
                    }
                    WM_MOUSEWHEEL => {
                        let delta = ((ms.mouseData >> 16) & 0xFFFF) as i16;
                        if delta != 0 {
                            let _ = state.tx.send(CaptureEvent::MouseScroll {
                                dx_120: 0,
                                dy_120: delta,
                            });
                        }
                        true
                    }
                    WM_MOUSEHWHEEL => {
                        let delta = ((ms.mouseData >> 16) & 0xFFFF) as i16;
                        if delta != 0 {
                            let _ = state.tx.send(CaptureEvent::MouseScroll {
                                dx_120: delta,
                                dy_120: 0,
                            });
                        }
                        true
                    }
                    _ => false,
                }
            })
            .unwrap_or(false);

            if suppress {
                return 1;
            }
        }
    }

    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

fn handle_raw_mouse_input(lparam: LPARAM) {
    let mut size: u32 = size_of::<RAWINPUT>() as u32;
    let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
    let res = unsafe {
        GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            &mut raw as *mut RAWINPUT as *mut _,
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if res == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE {
        return;
    }

    let mouse = unsafe { raw.data.mouse };
    // Ignore absolute tablet/RDP coordinates; only process true relative HID deltas.
    if (mouse.usFlags & MOUSE_MOVE_ABSOLUTE) != 0 {
        return;
    }

    let dx = mouse.lLastX as f32;
    let dy = mouse.lLastY as f32;
    if dx == 0.0 && dy == 0.0 {
        return;
    }

    with_hook_state(|state| {
        state
            .last_raw_mouse_ms
            .store(state.elapsed_ms(), Ordering::Relaxed);

        if state.remote_mode.load(Ordering::Acquire) {
            let _ = state.tx.send(CaptureEvent::MouseDelta { dx, dy });
            return;
        }

        // Local mode: check if the user pushed past an active outer monitor edge!
        state.maybe_refresh_geometry();
        let edges = state.active_edges.lock().clone();
        if edges.is_empty() {
            return;
        }

        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } == 0 {
            return;
        }
        let geom = state.geometry.lock().clone();

        let mut push = state.edge_push.lock();
        let mut crossed = None;
        for &edge in &edges {
            let outward = match edge {
                Edge::Left => -dx,
                Edge::Right => dx,
                Edge::Top => -dy,
                Edge::Bottom => dy,
            };
            if outward > 0.0 {
                if let Some(norm) = geom.hit_outer_edge(edge, pt.x, pt.y, 2) {
                    if push.0 == Some(edge) {
                        push.1 += outward;
                    } else {
                        *push = (Some(edge), outward);
                    }
                    if push.1 >= state.config.edge_threshold_px.max(1) as f32 {
                        crossed = Some((edge, norm));
                    }
                    break;
                }
            }
        }

        if let Some((edge, normalized_pos)) = crossed {
            *push = (None, 0.0);
            state.anchor_x.store(pt.x, Ordering::Relaxed);
            state.anchor_y.store(pt.y, Ordering::Relaxed);
            state.remote_mode.store(true, Ordering::SeqCst);
            let _ = state.tx.send(CaptureEvent::EdgeCrossed {
                edge,
                normalized_pos,
            });
        } else if let Some(prev_edge) = push.0 {
            if geom.hit_outer_edge(prev_edge, pt.x, pt.y, 4).is_none() {
                *push = (None, 0.0);
            }
        }
    });
}

unsafe extern "system" fn capture_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_INPUT => {
            handle_raw_mouse_input(lparam);
            0
        }
        WM_APP_ENTER_REMOTE => {
            with_hook_state(|state| {
                let mut pt = POINT { x: 0, y: 0 };
                if unsafe { GetCursorPos(&mut pt) } != 0 {
                    state.anchor_x.store(pt.x, Ordering::Relaxed);
                    state.anchor_y.store(pt.y, Ordering::Relaxed);
                }
            });
            0
        }
        WM_APP_LEAVE_REMOTE => {
            with_hook_state(|state| {
                if let Some((edge, norm)) = state.pending_leave.lock().take() {
                    let (x, y) = state.geometry.lock().point_on_edge(edge, norm, 6);
                    unsafe {
                        SetCursorPos(x, y);
                    }
                }
                *state.edge_push.lock() = (None, 0.0);
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Starts the Windows 11 server capture hook + Raw Input loop on a dedicated high-priority thread.
pub fn start_windows_capture(config: CaptureConfig) -> Result<CaptureHandle> {
    init_dpi_awareness();
    let geometry = query_desktop_geometry()?;
    let (tx, rx) = mpsc::unbounded_channel();

    let state = Arc::new(HookState {
        config,
        tx,
        active_edges: Mutex::new(Vec::new()),
        geometry: Mutex::new(geometry),
        remote_mode: AtomicBool::new(false),
        anchor_x: AtomicI32::new(0),
        anchor_y: AtomicI32::new(0),
        pending_leave: Mutex::new(None),
        edge_push: Mutex::new((None, 0.0)),
        start_instant: Instant::now(),
        last_raw_mouse_ms: AtomicU64::new(0),
        last_geom_refresh_ms: AtomicU64::new(0),
    });

    let cell = GLOBAL_HOOK_STATE.get_or_init(|| Mutex::new(None));
    *cell.lock() = Some(Arc::clone(&state));

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<isize>>();

    thread::Builder::new()
        .name("glide-win-capture".into())
        .spawn(move || {
            unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            }
            match init_capture_window_and_hooks() {
                Ok((hwnd, kb_hook, ms_hook)) => {
                    let _ = ready_tx.send(Ok(hwnd as isize));
                    debug!("Windows Raw Input + LL hooks active");

                    let mut msg: MSG = unsafe { std::mem::zeroed() };
                    while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
                        unsafe {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }

                    unsafe {
                        if !kb_hook.is_null() {
                            UnhookWindowsHookEx(kb_hook);
                        }
                        if !ms_hook.is_null() {
                            UnhookWindowsHookEx(ms_hook);
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to initialize Windows capture hooks");
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .context("failed to spawn glide-win-capture thread")?;

    let hwnd_val = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .context("timed out waiting for Windows capture hook thread")??;

    let control = WindowsCaptureControl {
        hwnd: AtomicIsize::new(hwnd_val),
        state,
    };

    Ok(CaptureHandle {
        control: Box::new(control),
        events: rx,
    })
}

fn init_capture_window_and_hooks() -> Result<(HWND, HHOOK, HHOOK)> {
    let class_name: Vec<u16> = "GlideRawInputCaptureWindow\0".encode_utf16().collect();
    let hinstance = unsafe { GetModuleHandleW(ptr::null()) };

    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(capture_wnd_proc),
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

    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
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
    if hwnd.is_null() {
        return Err(anyhow!("CreateWindowExW failed for Glide raw input window"));
    }

    // Register mouse Raw Input with RIDEV_INPUTSINK so we receive raw deltas even in background.
    let rid = RAWINPUTDEVICE {
        usUsagePage: 0x01, // Generic Desktop Controls
        usUsage: 0x02,     // Mouse
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    if unsafe { RegisterRawInputDevices(&rid, 1, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
        return Err(anyhow!("RegisterRawInputDevices failed for mouse"));
    }

    let kb_hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_ll_proc), hinstance, 0) };
    if kb_hook.is_null() {
        return Err(anyhow!("SetWindowsHookExW(WH_KEYBOARD_LL) failed"));
    }

    let ms_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), hinstance, 0) };
    if ms_hook.is_null() {
        unsafe {
            UnhookWindowsHookEx(kb_hook);
        }
        return Err(anyhow!("SetWindowsHookExW(WH_MOUSE_LL) failed"));
    }

    Ok((hwnd, kb_hook, ms_hook))
}
