//! Windows 11 server input capture using Raw Input (`WM_INPUT`) for true unaccelerated
//! mouse deltas + high-priority `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks with:
//! - German `AltGr` fake-LCtrl (`0x21D`) filtering
//! - Invisible bottom-right corner cursor parking in remote mode (with guaranteed restore)
//! - Dead-corner guard, edge dwell, fullscreen app auto-lock, and manual cursor lock
//! - Direct keyboard screen-switching (`Ctrl+Alt+Arrow`) and lock toggle (`Ctrl+Alt+L`)
//! - CapsLock / NumLock state synchronization

use crate::geometry::DesktopGeometry;
use crate::windows::{init_dpi_awareness, query_desktop_geometry, GLIDE_EXTRA_INFO};
use crate::{CaptureConfig, CaptureControl, CaptureEvent, CaptureHandle};
use anyhow::{anyhow, Context, Result};
use glide_hid::{is_windows_fake_altgr_lctrl, HidUsage, ModifierMask, MouseButton, WinScan};
use glide_proto::{Edge, LockState};
use parking_lot::Mutex;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowRect, PostMessageW,
    RegisterClassW, SetCursorPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED, XBUTTON1, XBUTTON2,
};

const WM_APP_ENTER_REMOTE: u32 = WM_APP + 1;
const WM_APP_LEAVE_REMOTE: u32 = WM_APP + 2;

const VK_PAUSE_CODE: u32 = 0x13;
const VK_SNAPSHOT_CODE: u32 = 0x2C;
const VK_NUMLOCK_CODE: u32 = 0x90;
const VK_RSHIFT_CODE: u32 = 0xA1;

enum LeaveTarget {
    Edge(Edge, f32),
    Point(f32, f32),
}

struct HookState {
    config: CaptureConfig,
    tx: mpsc::UnboundedSender<CaptureEvent>,
    active_edges: Mutex<Vec<Edge>>,
    geometry: Mutex<DesktopGeometry>,
    remote_mode: AtomicBool,
    locked: AtomicBool,
    auto_lock_fullscreen: AtomicBool,
    sync_lock_keys: AtomicBool,
    edge_threshold_px: AtomicI32,
    dead_corner_px: AtomicI32,
    edge_dwell_ms: AtomicU64,
    anchor_x: AtomicI32,
    anchor_y: AtomicI32,
    last_local_norm: Mutex<(f32, f32)>,
    pending_leave: Mutex<Option<LeaveTarget>>,
    edge_push: Mutex<(Option<Edge>, f32, Option<Instant>)>,
    mods: AtomicU8,
    start_instant: Instant,
    last_raw_mouse_ms: AtomicU64,
    last_geom_refresh_ms: AtomicU64,
}

impl HookState {
    fn elapsed_ms(&self) -> u64 {
        self.start_instant.elapsed().as_millis() as u64
    }

    fn refresh_geometry_now(&self) -> bool {
        self.last_geom_refresh_ms
            .store(self.elapsed_ms(), Ordering::Relaxed);
        let Ok(new_geom) = query_desktop_geometry() else {
            return false;
        };
        let changed = {
            let mut geom = self.geometry.lock();
            if *geom != new_geom {
                *geom = new_geom.clone();
                true
            } else {
                false
            }
        };
        if changed {
            *self.edge_push.lock() = (None, 0.0, None);
            if self.remote_mode.load(Ordering::Acquire) {
                self.park_cursor_invisibly();
            }
            debug!(
                monitors = new_geom.monitors.len(),
                primary_scale = new_geom.primary_scale(),
                "Refreshed Windows desktop geometry"
            );
            let _ = self.tx.send(CaptureEvent::GeometryChanged(new_geom));
        }
        changed
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
            let _ = self.refresh_geometry_now();
        }
    }

    fn park_cursor_invisibly(&self) {
        let (px, py) = self.geometry.lock().primary_park_corner();
        self.anchor_x.store(px, Ordering::Relaxed);
        self.anchor_y.store(py, Ordering::Relaxed);
        unsafe {
            SetCursorPos(px, py);
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

impl Drop for WindowsCaptureControl {
    fn drop(&mut self) {
        // Always restore cursor & stop hook loop cleanly on shutdown
        if self.state.remote_mode.swap(false, Ordering::SeqCst) {
            let (nx, ny) = *self.state.last_local_norm.lock();
            let (x, y) = self.state.geometry.lock().denormalize_point(nx, ny);
            unsafe {
                SetCursorPos(x, y);
            }
        }
        let hwnd = self.hwnd.swap(0, Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_QUIT, 0, 0);
            }
        }
        if let Some(cell) = GLOBAL_HOOK_STATE.get() {
            *cell.lock() = None;
        }
    }
}

impl CaptureControl for WindowsCaptureControl {
    fn set_active_edges(&self, edges: &[Edge]) {
        *self.state.active_edges.lock() = edges.to_vec();
    }

    fn update_tuning(
        &self,
        edge_threshold_px: i32,
        dead_corner_px: i32,
        edge_dwell_ms: u64,
        auto_lock_fullscreen: bool,
        locked: bool,
    ) {
        self.state
            .edge_threshold_px
            .store(edge_threshold_px.max(1), Ordering::Relaxed);
        self.state
            .dead_corner_px
            .store(dead_corner_px.max(0), Ordering::Relaxed);
        self.state
            .edge_dwell_ms
            .store(edge_dwell_ms, Ordering::Relaxed);
        self.state
            .auto_lock_fullscreen
            .store(auto_lock_fullscreen, Ordering::Relaxed);
        self.state.locked.store(locked, Ordering::Relaxed);
    }

    fn set_sync_lock_keys(&self, enabled: bool) {
        self.state.sync_lock_keys.store(enabled, Ordering::Relaxed);
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
        *self.state.pending_leave.lock() = Some(LeaveTarget::Edge(return_edge, normalized_pos));
        self.state.remote_mode.store(false, Ordering::SeqCst);
        let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_LEAVE_REMOTE, 0, 0);
            }
        }
        Ok(())
    }

    fn leave_remote_at(&self, norm_x: f32, norm_y: f32) -> Result<()> {
        *self.state.pending_leave.lock() = Some(LeaveTarget::Point(norm_x, norm_y));
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

    fn lock_state(&self) -> LockState {
        LockState {
            caps_lock: (unsafe { GetKeyState(VK_CAPITAL as i32) } & 1) != 0,
            num_lock: (unsafe { GetKeyState(VK_NUMLOCK_CODE as i32) } & 1) != 0,
        }
    }

    fn last_local_cursor_norm(&self) -> (f32, f32) {
        *self.state.last_local_norm.lock()
    }
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

fn translate_ll_key(vk_code: u32, scan_code: u32, flags: u32) -> Option<HidUsage> {
    let win_scan = match vk_code {
        VK_PAUSE_CODE => WinScan {
            code: 0x45,
            extended: false,
        },
        VK_NUMLOCK_CODE => WinScan {
            code: 0x45,
            extended: true,
        },
        VK_RSHIFT_CODE => WinScan {
            code: 0x36,
            extended: false,
        },
        VK_SNAPSHOT_CODE => WinScan {
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

/// Checks whether the active foreground window covers an entire monitor (e.g. fullscreen game/VM),
/// excluding Windows desktop shell windows (`Progman`, `WorkerW`, `Shell_TrayWnd`).
fn is_foreground_fullscreen(geom: &DesktopGeometry) -> bool {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return false;
    }
    let mut class_buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32) };
    if len > 0 {
        let class_name = String::from_utf16_lossy(&class_buf[..len as usize]);
        if matches!(
            class_name.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            return false;
        }
    }

    let mut rect: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return false;
    }

    geom.monitors.iter().any(|m| {
        rect.left <= m.left()
            && rect.top <= m.top()
            && rect.right >= m.right()
            && rect.bottom >= m.bottom()
    })
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
                    // Swallow Windows' synthetic fake VK_LCONTROL (scanCode == 0x21D) before AltGr!
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

                    // Track modifiers atomically for local hotkey chords
                    let mut mask = ModifierMask(state.mods.load(Ordering::Relaxed));
                    mask.update(usage, pressed);
                    state.mods.store(mask.0, Ordering::Relaxed);

                    if usage == state.config.escape_key {
                        if pressed && remote {
                            state.remote_mode.store(false, Ordering::SeqCst);
                            let _ = state.tx.send(CaptureEvent::EscapeHotkey);
                            return true;
                        }
                        return remote;
                    }

                    // Direct screen-switch hotkeys (Ctrl + LeftAlt + Arrow) & Cursor Lock (Ctrl + LeftAlt + L).
                    // Specifically checks ALT_LEFT (never ALT_RIGHT / AltGr) so German typing never collides!
                    if pressed
                        && state.config.enable_hotkeys
                        && mask.ctrl()
                        && mask.contains(ModifierMask::ALT_LEFT)
                    {
                        if let Some(direction) = arrow_to_edge(usage) {
                            let _ = state.tx.send(CaptureEvent::HotkeySwitch { direction });
                            return true;
                        }
                        if usage == HidUsage::keyboard(0x0F) {
                            // KeyL
                            let _ = state.tx.send(CaptureEvent::HotkeyToggleLock);
                            return true;
                        }
                    }

                    if remote {
                        let _ = state.tx.send(CaptureEvent::Key { usage, pressed });
                        // Keep the physical keyboard's CapsLock / NumLock LED and Windows'
                        // toggle state in sync with the client when lock-key sync is enabled.
                        if state.sync_lock_keys.load(Ordering::Relaxed)
                            && (kb.vkCode == VK_CAPITAL as u32 || kb.vkCode == VK_NUMLOCK_CODE)
                        {
                            return false;
                        }
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
                        // Primary motion path is Raw Input (WM_INPUT). Fallback for touchpads:
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

        if state.locked.load(Ordering::Relaxed) {
            return;
        }

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
        *state.last_local_norm.lock() = geom.normalize_point(pt.x, pt.y);

        // Step 4: Fullscreen app auto-lock
        if state.auto_lock_fullscreen.load(Ordering::Relaxed) && is_foreground_fullscreen(&geom) {
            *state.edge_push.lock() = (None, 0.0, None);
            return;
        }

        let threshold = state.edge_threshold_px.load(Ordering::Relaxed).max(1) as f32;
        let dead_corner = state.dead_corner_px.load(Ordering::Relaxed).max(0);
        let dwell_ms = state.edge_dwell_ms.load(Ordering::Relaxed);

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
                if let Some(norm) = geom.hit_outer_edge_guarded(edge, pt.x, pt.y, 2, dead_corner) {
                    if push.0 == Some(edge) {
                        push.1 += outward;
                    } else {
                        *push = (Some(edge), outward, Some(Instant::now()));
                    }
                    let dwell_ok = dwell_ms == 0
                        || push
                            .2
                            .is_some_and(|t| t.elapsed() >= Duration::from_millis(dwell_ms));
                    if push.1 >= threshold && dwell_ok {
                        crossed = Some((edge, norm));
                    }
                    break;
                }
            }
        }

        if let Some((edge, normalized_pos)) = crossed {
            *push = (None, 0.0, None);
            state.remote_mode.store(true, Ordering::SeqCst);
            // Step 2: Park cursor cleanly in the invisible bottom-right corner!
            state.park_cursor_invisibly();
            let _ = state.tx.send(CaptureEvent::EdgeCrossed {
                edge,
                normalized_pos,
            });
        } else if let Some(prev_edge) = push.0 {
            if geom
                .hit_outer_edge_guarded(prev_edge, pt.x, pt.y, 4, dead_corner)
                .is_none()
            {
                *push = (None, 0.0, None);
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
                // Save current local cursor before hiding/parking (for hotkey return memory)
                let mut pt = POINT { x: 0, y: 0 };
                if unsafe { GetCursorPos(&mut pt) } != 0 {
                    let norm = state.geometry.lock().normalize_point(pt.x, pt.y);
                    *state.last_local_norm.lock() = norm;
                }
                state.park_cursor_invisibly();
            });
            0
        }
        WM_APP_LEAVE_REMOTE => {
            with_hook_state(|state| {
                if let Some(target) = state.pending_leave.lock().take() {
                    let geom = state.geometry.lock().clone();
                    let (x, y) = match target {
                        LeaveTarget::Edge(edge, norm) => geom.point_on_edge(edge, norm, 6),
                        LeaveTarget::Point(nx, ny) => geom.denormalize_point(nx, ny),
                    };
                    unsafe {
                        SetCursorPos(x, y);
                    }
                }
                *state.edge_push.lock() = (None, 0.0, None);
            });
            0
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            with_hook_state(|state| {
                state.refresh_geometry_now();
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
        edge_threshold_px: AtomicI32::new(config.edge_threshold_px.max(1)),
        dead_corner_px: AtomicI32::new(config.dead_corner_px.max(0)),
        edge_dwell_ms: AtomicU64::new(config.edge_dwell_ms),
        auto_lock_fullscreen: AtomicBool::new(config.auto_lock_fullscreen),
        sync_lock_keys: AtomicBool::new(config.sync_lock_keys),
        config,
        tx,
        active_edges: Mutex::new(Vec::new()),
        geometry: Mutex::new(geometry),
        remote_mode: AtomicBool::new(false),
        locked: AtomicBool::new(false),
        anchor_x: AtomicI32::new(0),
        anchor_y: AtomicI32::new(0),
        last_local_norm: Mutex::new((0.5, 0.5)),
        pending_leave: Mutex::new(None),
        edge_push: Mutex::new((None, 0.0, None)),
        mods: AtomicU8::new(0),
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
                        DestroyWindow(hwnd);
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

    // Use a hidden top-level window (NOT HWND_MESSAGE): message-only windows never receive
    // broadcast notifications like WM_DISPLAYCHANGE or WM_POWERBROADCAST. Because WS_VISIBLE
    // is omitted and ShowWindow is never called, the window remains invisible and never appears
    // in the taskbar or Alt+Tab.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            class_name.as_ptr(),
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
    if hwnd.is_null() {
        return Err(anyhow!("CreateWindowExW failed for Glide raw input window"));
    }

    let rid = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    if unsafe { RegisterRawInputDevices(&rid, 1, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(anyhow!("RegisterRawInputDevices failed for mouse"));
    }

    let kb_hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_ll_proc), hinstance, 0) };
    if kb_hook.is_null() {
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(anyhow!("SetWindowsHookExW(WH_KEYBOARD_LL) failed"));
    }

    let ms_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), hinstance, 0) };
    if ms_hook.is_null() {
        unsafe {
            UnhookWindowsHookEx(kb_hook);
            DestroyWindow(hwnd);
        }
        return Err(anyhow!("SetWindowsHookExW(WH_MOUSE_LL) failed"));
    }

    Ok((hwnd, kb_hook, ms_hook))
}
