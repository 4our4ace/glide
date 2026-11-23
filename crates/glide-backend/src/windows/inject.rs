//! Windows 11 client input injector using `SendInput` (scancode keys + relative mouse + hi-res scroll),
//! cursor memory restoration, dead-corner guards, and CapsLock/NumLock sync.

use crate::geometry::DesktopGeometry;
use crate::windows::{
    init_dpi_awareness, query_desktop_geometry, WindowsFocusState, GLIDE_EXTRA_INFO,
};
use crate::InputInjector;
use anyhow::Result;
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{Edge, LockState, OsKind};
use std::collections::HashSet;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, trace};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VK_CAPITAL, VK_NUMLOCK,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    PostMessageW, RegisterClassW, SetCursorPos, TranslateMessage, MSG, PBT_APMRESUMEAUTOMATIC,
    PBT_APMRESUMECRITICAL, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, WM_DISPLAYCHANGE, WM_DPICHANGED,
    WM_POWERBROADCAST, WM_QUIT, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_OVERLAPPED, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK, XBUTTON1, XBUTTON2,
};

static INJECTOR_DISPLAY_GEN: AtomicU64 = AtomicU64::new(0);
static INJECTOR_SUSPEND_GEN: AtomicU64 = AtomicU64::new(0);
static INJECTOR_RESUME_GEN: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn injector_watch_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            INJECTOR_DISPLAY_GEN.fetch_add(1, Ordering::Release);
            0
        }
        WM_POWERBROADCAST => match wparam as u32 {
            PBT_APMSUSPEND => {
                INJECTOR_SUSPEND_GEN.fetch_add(1, Ordering::Release);
                1
            }
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL => {
                INJECTOR_DISPLAY_GEN.fetch_add(1, Ordering::Release);
                INJECTOR_RESUME_GEN.fetch_add(1, Ordering::Release);
                1
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        },
        WM_WTSSESSION_CHANGE => match wparam as u32 {
            WTS_SESSION_LOCK => {
                INJECTOR_SUSPEND_GEN.fetch_add(1, Ordering::Release);
                0
            }
            WTS_SESSION_UNLOCK => {
                INJECTOR_DISPLAY_GEN.fetch_add(1, Ordering::Release);
                INJECTOR_RESUME_GEN.fetch_add(1, Ordering::Release);
                0
            }
            _ => 0,
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Spawns a lightweight hidden top-level window listening for display/DPI changes,
/// sleep/wake power broadcasts, and WTS session lock/unlock events (best-effort).
fn spawn_display_watcher() -> AtomicIsize {
    let (tx, rx) = std::sync::mpsc::sync_channel::<isize>(1);
    let spawn_res = thread::Builder::new()
        .name("glide-win-display-watch".into())
        .spawn(move || {
            let class_name: Vec<u16> = "GlideInjectorWatchWindow\0".encode_utf16().collect();
            let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(injector_watch_wnd_proc),
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
            // Must be a hidden top-level window (NOT HWND_MESSAGE) to receive WM_DISPLAYCHANGE,
            // WM_POWERBROADCAST, WM_DPICHANGED, and WM_WTSSESSION_CHANGE.
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
                let _ = tx.send(0);
                return;
            }
            let wts_registered =
                unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } != 0;
            let _ = tx.send(hwnd as isize);
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            unsafe {
                if wts_registered {
                    WTSUnRegisterSessionNotification(hwnd);
                }
                DestroyWindow(hwnd);
            }
        });

    let hwnd_val = if spawn_res.is_ok() {
        rx.recv_timeout(Duration::from_secs(2)).unwrap_or(0)
    } else {
        0
    };
    AtomicIsize::new(hwnd_val)
}

pub struct WindowsInjector {
    geometry: DesktopGeometry,
    watch_hwnd: AtomicIsize,
    seen_display_gen: u64,
    seen_suspend_gen: u64,
    seen_resume_gen: u64,
    last_geom_refresh: Instant,
    rem_x: f32,
    rem_y: f32,
    edge_push_accum: f32,
    edge_push_start: Option<Instant>,
    pressed_keys: HashSet<HidUsage>,
    pressed_buttons: HashSet<MouseButton>,
    unfocus_on_leave: bool,
    focus_state: Arc<WindowsFocusState>,
}

impl WindowsInjector {
    pub fn new() -> Result<Self> {
        init_dpi_awareness();
        let watch_hwnd = spawn_display_watcher();
        let seen_display_gen = INJECTOR_DISPLAY_GEN.load(Ordering::Acquire);
        let seen_suspend_gen = INJECTOR_SUSPEND_GEN.load(Ordering::Acquire);
        let seen_resume_gen = INJECTOR_RESUME_GEN.load(Ordering::Acquire);
        let geometry = query_desktop_geometry()?;
        Ok(Self {
            geometry,
            watch_hwnd,
            seen_display_gen,
            seen_suspend_gen,
            seen_resume_gen,
            last_geom_refresh: Instant::now(),
            rem_x: 0.0,
            rem_y: 0.0,
            edge_push_accum: 0.0,
            edge_push_start: None,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            unfocus_on_leave: false,
            focus_state: Arc::new(WindowsFocusState::new()),
        })
    }

    fn refresh_geometry_if_needed(&mut self) -> bool {
        let current_gen = INJECTOR_DISPLAY_GEN.load(Ordering::Acquire);
        let dirty = current_gen != self.seen_display_gen;
        if !dirty && self.last_geom_refresh.elapsed() < Duration::from_secs(5) {
            return false;
        }
        self.seen_display_gen = current_gen;
        self.last_geom_refresh = Instant::now();
        if let Ok(new_geom) = query_desktop_geometry() {
            if new_geom != self.geometry {
                self.geometry = new_geom;
                self.edge_push_accum = 0.0;
                debug!(
                    monitors = self.geometry.monitors.len(),
                    primary_scale = self.geometry.primary_scale(),
                    dirty,
                    "WindowsInjector refreshed desktop geometry"
                );
                return true;
            }
        }
        false
    }

    fn send_mouse(&self, dx: i32, dy: i32, mouse_data: u32, flags: u32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data,
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

    fn send_key_scan(&self, scan: u16, extended: bool, pressed: bool) {
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan,
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
}

impl InputInjector for WindowsInjector {
    fn os_kind(&self) -> OsKind {
        OsKind::Windows
    }

    fn set_unfocus_on_leave(&mut self, enabled: bool) {
        self.unfocus_on_leave = enabled;
        if !enabled {
            self.focus_state.discard();
        }
    }

    fn discard_saved_focus(&mut self) {
        self.focus_state.discard();
    }

    fn desktop_geometry(&mut self) -> Result<DesktopGeometry> {
        self.seen_display_gen = INJECTOR_DISPLAY_GEN.load(Ordering::Acquire);
        self.last_geom_refresh = Instant::now();
        self.geometry = query_desktop_geometry()?;
        self.last_geom_refresh = Instant::now();
        Ok(self.geometry.clone())
    }

    fn poll_geometry_changed(&mut self) -> Option<DesktopGeometry> {
        if self.refresh_geometry_if_needed() {
            Some(self.geometry.clone())
        } else {
            None
        }
    }

    fn poll_system_suspended(&mut self) -> bool {
        let gen = INJECTOR_SUSPEND_GEN.load(Ordering::Acquire);
        if gen != self.seen_suspend_gen {
            self.seen_suspend_gen = gen;
            self.focus_state.discard();
            let _ = self.release_all();
            return true;
        }
        false
    }

    fn poll_system_resumed(&mut self) -> bool {
        let gen = INJECTOR_RESUME_GEN.load(Ordering::Acquire);
        if gen != self.seen_resume_gen {
            self.seen_resume_gen = gen;
            self.focus_state.discard();
            return true;
        }
        false
    }

    fn enter(&mut self, edge: Edge, normalized_pos: f32) -> Result<()> {
        let _ = self.refresh_geometry_if_needed();
        let (x, y) = self.geometry.point_on_edge(edge, normalized_pos, 4);
        unsafe {
            SetCursorPos(x, y);
        }
        self.rem_x = 0.0;
        self.rem_y = 0.0;
        self.edge_push_accum = 0.0;
        self.edge_push_start = None;
        if self.unfocus_on_leave {
            self.focus_state.schedule_restore();
        } else {
            self.focus_state.discard();
        }
        debug!(
            ?edge,
            normalized_pos, x, y, "WindowsInjector entered screen"
        );
        Ok(())
    }

    fn enter_at(&mut self, norm_x: f32, norm_y: f32) -> Result<()> {
        let _ = self.refresh_geometry_if_needed();
        let (x, y) = self.geometry.denormalize_point(norm_x, norm_y);
        unsafe {
            SetCursorPos(x, y);
        }
        self.rem_x = 0.0;
        self.rem_y = 0.0;
        self.edge_push_accum = 0.0;
        self.edge_push_start = None;
        if self.unfocus_on_leave {
            self.focus_state.schedule_restore();
        } else {
            self.focus_state.discard();
        }
        Ok(())
    }

    fn cursor_norm(&mut self) -> (f32, f32) {
        self.refresh_geometry_if_needed();
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } != 0 {
            self.geometry.normalize_point(pt.x, pt.y)
        } else {
            (0.5, 0.5)
        }
    }

    fn sync_lock_state(&mut self, desired: LockState) -> Result<()> {
        let cur_caps = (unsafe { GetKeyState(VK_CAPITAL as i32) } & 1) != 0;
        let cur_num = (unsafe { GetKeyState(VK_NUMLOCK as i32) } & 1) != 0;
        if cur_caps != desired.caps_lock {
            self.send_key_scan(0x3A, false, true);
            self.send_key_scan(0x3A, false, false);
        }
        if cur_num != desired.num_lock {
            self.send_key_scan(0x45, true, true);
            self.send_key_scan(0x45, true, false);
        }
        Ok(())
    }

    fn move_relative(
        &mut self,
        dx: f32,
        dy: f32,
        return_edge: Option<Edge>,
        edge_threshold_px: i32,
        dead_corner_px: i32,
    ) -> Result<Option<(Edge, f32)>> {
        self.refresh_geometry_if_needed();

        self.rem_x += dx;
        self.rem_y += dy;
        let ix = self.rem_x.trunc() as i32;
        let iy = self.rem_y.trunc() as i32;
        self.rem_x -= ix as f32;
        self.rem_y -= iy as f32;

        if ix != 0 || iy != 0 {
            self.send_mouse(ix, iy, 0, MOUSEEVENTF_MOVE);
        }

        let Some(edge) = return_edge else {
            return Ok(None);
        };

        let outward = match edge {
            Edge::Left => -dx,
            Edge::Right => dx,
            Edge::Top => -dy,
            Edge::Bottom => dy,
        };

        if outward <= 0.0 {
            self.edge_push_accum = 0.0;
            self.edge_push_start = None;
            return Ok(None);
        }

        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } != 0 {
            if let Some(norm) =
                self.geometry
                    .hit_outer_edge_guarded(edge, pt.x, pt.y, 2, dead_corner_px)
            {
                let lmb_held = self.pressed_buttons.contains(&MouseButton::Left);
                let start = *self.edge_push_start.get_or_insert_with(Instant::now);
                self.edge_push_accum += outward;
                let drag_dwell_ok = !lmb_held || start.elapsed() >= Duration::from_millis(25);
                if self.edge_push_accum >= edge_threshold_px.max(1) as f32 && drag_dwell_ok {
                    self.edge_push_accum = 0.0;
                    self.edge_push_start = None;
                    return Ok(Some((edge, norm)));
                }
            } else {
                self.edge_push_accum = 0.0;
                self.edge_push_start = None;
            }
        }

        Ok(None)
    }

    fn scroll(&mut self, dx_120: i16, dy_120: i16) -> Result<()> {
        if dy_120 != 0 {
            self.send_mouse(0, 0, (dy_120 as i32) as u32, MOUSEEVENTF_WHEEL);
        }
        if dx_120 != 0 {
            self.send_mouse(0, 0, (dx_120 as i32) as u32, MOUSEEVENTF_HWHEEL);
        }
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        let (flags, data) = match (button, pressed) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
        };
        self.send_mouse(0, 0, data, flags);
        if pressed {
            self.pressed_buttons.insert(button);
        } else {
            self.pressed_buttons.remove(&button);
        }
        Ok(())
    }

    fn key(&mut self, usage: HidUsage, pressed: bool) -> Result<()> {
        let Some(scan) = usage.to_win_scan() else {
            trace!(?usage, "Unmapped HID usage for Windows");
            return Ok(());
        };
        self.send_key_scan(scan.code, scan.extended, pressed);
        if pressed {
            self.pressed_keys.insert(usage);
        } else {
            self.pressed_keys.remove(&usage);
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<HidUsage> = self.pressed_keys.drain().collect();
        let btns: Vec<MouseButton> = self.pressed_buttons.drain().collect();
        for k in keys {
            if let Some(scan) = k.to_win_scan() {
                self.send_key_scan(scan.code, scan.extended, false);
            }
        }
        for b in btns {
            let _ = self.button(b, false);
        }
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        self.refresh_geometry_if_needed();
        let (px, py) = self.geometry.primary_park_corner();
        unsafe {
            SetCursorPos(px, py);
        }
        self.edge_push_accum = 0.0;
        self.edge_push_start = None;
        if self.unfocus_on_leave {
            self.focus_state.schedule_unfocus();
        } else {
            self.focus_state.discard();
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        self.focus_state.discard();
        Ok(())
    }
}

impl Drop for WindowsInjector {
    fn drop(&mut self) {
        self.focus_state.shutdown();
        let _ = self.release_all();
        let hwnd = self.watch_hwnd.swap(0, Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_QUIT, 0, 0);
            }
        }
    }
}
