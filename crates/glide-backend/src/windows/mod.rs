//! Native Windows 11 backend powered by `windows-sys` (pure Rust, zero C wrappers).
//!
//! Uses:
//! - Per-Monitor DPI Awareness V2 for exact physical multi-monitor coordinates
//! - Raw Input (`WM_INPUT`) for true unaccelerated mouse deltas & edge push detection
//! - High-priority `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks with German `AltGr` fake-LCtrl filtering
//! - `SendInput` with `KEYEVENTF_SCANCODE` & relative `MOUSEEVENTF_MOVE` + hi-res wheel
//! - `AddClipboardFormatListener` (`WM_CLIPBOARDUPDATE`) for instant bidirectional clipboard sync

pub mod capture;
pub mod clipboard;
pub mod dnd;
pub mod inject;
pub mod tray;

use crate::geometry::{DesktopGeometry, MonitorRect};
use crate::DoctorReport;
use anyhow::Result;
use glide_proto::OsKind;
use parking_lot::Mutex;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, ProcessPowerThrottling,
    SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClassNameW, GetForegroundWindow, GetLastActivePopup, GetShellWindow,
    GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible, PeekMessageW,
    SetForegroundWindow, MSG, PM_NOREMOVE,
};

/// Unique `dwExtraInfo` tag stamped onto events synthesized by Glide so our hooks
/// immediately ignore our own injected events (zero feedback loops).
pub(crate) const GLIDE_EXTRA_INFO: usize = 0x474C_4944;

const MONITORINFOF_PRIMARY: u32 = 1;

#[inline]
pub(crate) fn eq_utf16_ascii(wide: &[u16], ascii: &str) -> bool {
    wide.len() == ascii.len()
        && wide
            .iter()
            .zip(ascii.bytes())
            .all(|(&w, b)| w == u16::from(b))
}

/// Returns `true` if `hwnd` belongs to the Windows desktop shell or taskbar
/// (`Progman`, `WorkerW`, `Shell_TrayWnd`, `Shell_SecondaryTrayWnd`).
pub(crate) fn is_shell_window_class(hwnd: HWND) -> bool {
    let mut class_buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32) };
    if len <= 0 {
        return false;
    }
    let wide = &class_buf[..len as usize];
    eq_utf16_ascii(wide, "Progman")
        || eq_utf16_ascii(wide, "WorkerW")
        || eq_utf16_ascii(wide, "Shell_TrayWnd")
        || eq_utf16_ascii(wide, "Shell_SecondaryTrayWnd")
}

fn find_shell_hwnd() -> HWND {
    let mut shell_hwnd = unsafe { GetShellWindow() };
    if shell_hwnd.is_null() {
        let tray_class: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
        shell_hwnd = unsafe { FindWindowW(tray_class.as_ptr(), ptr::null()) };
    }
    shell_hwnd
}

/// Lock-free frontend / serialized blocking-pool coordinator for `unfocus_on_leave` and
/// re-focusing the saved local window when pointer/keyboard focus returns to this Windows PC.
///
/// The internal `saved` mutex is ONLY ever locked inside `spawn_blocking` workers — never on
/// low-level Windows hook threads or Tokio async worker threads.
pub(crate) struct WindowsFocusState {
    gen: AtomicU64,
    discard_epoch: AtomicU64,
    last_unfocus_gen: AtomicU64,
    cleared_through_gen: AtomicU64,
    saved: Mutex<Option<(u64, isize)>>,
}

impl WindowsFocusState {
    pub(crate) fn new() -> Self {
        Self {
            gen: AtomicU64::new(0),
            discard_epoch: AtomicU64::new(0),
            last_unfocus_gen: AtomicU64::new(0),
            cleared_through_gen: AtomicU64::new(0),
            saved: Mutex::new(None),
        }
    }

    /// Cancels any pending unfocus/restore and discards any saved window handle without locking.
    pub(crate) fn discard(&self) {
        self.discard_epoch.fetch_add(1, Ordering::SeqCst);
        let g = self.gen.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        self.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
    }

    /// Cancels an unstarted unfocus without discarding an already-saved HWND so an
    /// immediately-following `leave_remote_at` (e.g. `EscapeHotkey`) can still restore focus.
    pub(crate) fn bump_gen_only(&self) {
        self.gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Dispatches foreground-window unfocusing off the calling thread when inside a Tokio runtime
    /// so low-level hooks and QUIC `Enter`/`Leave` sends never block.
    pub(crate) fn schedule_unfocus(self: &Arc<Self>) {
        let epoch = self.discard_epoch.load(Ordering::Acquire);
        let g = self.gen.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        self.last_unfocus_gen.fetch_max(g, Ordering::AcqRel);
        let state = Arc::clone(self);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                unfocus_with_state(state, g, epoch);
            });
        } else {
            unfocus_with_state(state, g, epoch);
        }
    }

    /// Dispatches restoration of the previously unfocused foreground window off the calling
    /// thread. Returns immediately with zero blocking-pool overhead when nothing was unfocused.
    pub(crate) fn schedule_restore(self: &Arc<Self>) {
        let g = self.gen.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let last_unfocus = self.last_unfocus_gen.load(Ordering::Acquire);
        if last_unfocus == 0 || last_unfocus <= self.cleared_through_gen.load(Ordering::Acquire) {
            return;
        }
        let epoch = self.discard_epoch.load(Ordering::Acquire);
        let state = Arc::clone(self);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                restore_with_state(state, g, epoch);
            });
        } else {
            restore_with_state(state, g, epoch);
        }
    }
}

fn unfocus_with_state(state: Arc<WindowsFocusState>, g: u64, epoch: u64) {
    let mut saved = state.saved.lock();
    if state.gen.load(Ordering::Acquire) != g
        || state.discard_epoch.load(Ordering::Acquire) != epoch
    {
        return;
    }

    let fg = unsafe { GetForegroundWindow() };
    let shell_hwnd = find_shell_hwnd();

    // If foreground is already the desktop shell (e.g. a duplicate Leave arrived after our first
    // unfocus already moved focus to Progman), preserve a saved HWND from the current epoch
    // instead of wiping it.
    if !fg.is_null()
        && !shell_hwnd.is_null()
        && (fg == shell_hwnd || is_shell_window_class(fg))
    {
        if saved
            .as_ref()
            .is_some_and(|&(saved_epoch, _)| saved_epoch == epoch)
        {
            return;
        }
        *saved = None;
        state.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
        return;
    }

    *saved = None;

    if fg.is_null() || shell_hwnd.is_null() {
        state.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
        return;
    }

    if unsafe { IsHungAppWindow(fg) } != 0 {
        tracing::debug!("Skipping unfocus_on_leave for hung foreground window");
        state.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
        return;
    }

    // Ensure the blocking thread has a Win32 message queue before calling AttachThreadInput.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }

    let cur_tid = unsafe { GetCurrentThreadId() };
    let fg_tid = unsafe { GetWindowThreadProcessId(fg, ptr::null_mut()) };

    if state.gen.load(Ordering::Acquire) != g
        || state.discard_epoch.load(Ordering::Acquire) != epoch
    {
        return;
    }

    let attached = fg_tid != 0
        && fg_tid != cur_tid
        && unsafe { AttachThreadInput(cur_tid, fg_tid, 1) } != 0;

    let mut attempted = false;
    let mut focused_shell = false;
    if state.gen.load(Ordering::Acquire) == g
        && state.discard_epoch.load(Ordering::Acquire) == epoch
    {
        attempted = true;
        focused_shell = unsafe { SetForegroundWindow(shell_hwnd) } != 0;
    }

    if attached {
        unsafe {
            AttachThreadInput(cur_tid, fg_tid, 0);
        }
    }

    if focused_shell && state.discard_epoch.load(Ordering::Acquire) == epoch {
        *saved = Some((epoch, fg as isize));
    } else {
        if attempted && !focused_shell {
            tracing::debug!(
                "SetForegroundWindow(shell) returned 0 during unfocus_on_leave (e.g. UIPI elevated window)"
            );
        }
        state.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
    }
}

fn restore_with_state(state: Arc<WindowsFocusState>, g: u64, epoch: u64) {
    let mut saved = state.saved.lock();
    if state.gen.load(Ordering::Acquire) != g
        || state.discard_epoch.load(Ordering::Acquire) != epoch
    {
        return;
    }

    let taken = saved.take();
    state.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
    let Some((saved_epoch, raw)) = taken else {
        return;
    };
    if saved_epoch != epoch {
        return;
    }

    let shell_hwnd = find_shell_hwnd();
    let cur_fg = unsafe { GetForegroundWindow() };
    let fg_is_shell_or_null = cur_fg.is_null()
        || (!shell_hwnd.is_null() && cur_fg == shell_hwnd)
        || is_shell_window_class(cur_fg);
    if !fg_is_shell_or_null {
        return;
    }

    let mut target = raw as HWND;
    if target.is_null()
        || unsafe { IsWindow(target) } == 0
        || unsafe { IsWindowVisible(target) } == 0
        || unsafe { IsIconic(target) } != 0
        || unsafe { IsHungAppWindow(target) } != 0
    {
        return;
    }

    // Modal dialog safety: if the saved owner window is disabled by a modal dialog,
    // redirect focus to its active enabled popup rather than a disabled owner.
    if unsafe { IsWindowEnabled(target) } == 0 {
        let popup = unsafe { GetLastActivePopup(target) };
        if !popup.is_null()
            && popup != target
            && unsafe { IsWindow(popup) } != 0
            && unsafe { IsWindowVisible(popup) } != 0
            && unsafe { IsIconic(popup) } == 0
            && unsafe { IsWindowEnabled(popup) } != 0
            && unsafe { IsHungAppWindow(popup) } == 0
        {
            target = popup;
        } else {
            return;
        }
    }

    // Ensure the blocking thread has a Win32 message queue before calling AttachThreadInput.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }

    let cur_tid = unsafe { GetCurrentThreadId() };
    let fg_tid = if !cur_fg.is_null() && unsafe { IsHungAppWindow(cur_fg) } == 0 {
        unsafe { GetWindowThreadProcessId(cur_fg, ptr::null_mut()) }
    } else {
        0
    };

    if state.gen.load(Ordering::Acquire) != g
        || state.discard_epoch.load(Ordering::Acquire) != epoch
    {
        return;
    }

    let attached = fg_tid != 0
        && fg_tid != cur_tid
        && unsafe { AttachThreadInput(cur_tid, fg_tid, 1) } != 0;

    let mut restored = true;
    if state.gen.load(Ordering::Acquire) == g
        && state.discard_epoch.load(Ordering::Acquire) == epoch
    {
        restored = unsafe { SetForegroundWindow(target) } != 0;
    }

    if attached {
        unsafe {
            AttachThreadInput(cur_tid, fg_tid, 0);
        }
    }

    if !restored {
        tracing::debug!("SetForegroundWindow(saved) returned 0 during unfocus_on_leave restore");
    }
}

/// Enables Per-Monitor DPI Awareness V2 so Windows reports real physical coordinates
/// across mixed-DPI multi-monitor setups (e.g. 100% + 125% + 150% + 200%).
pub fn init_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Opts this process out of Windows EcoQoS (E-core efficiency throttling) and
/// Windows 11 minimized/background timer-resolution throttling so 1000Hz input hooks
/// and QUIC timers stay jitter-free even while minimized to the system tray.
///
/// Issued as two separate `SetProcessInformation` calls because Windows 10 supports
/// `PROCESS_POWER_THROTTLING_EXECUTION_SPEED` while `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`
/// (Windows 11 22000+) causes Windows 10 to reject a combined mask with `ERROR_INVALID_PARAMETER`.
pub fn init_windows_realtime_qos() {
    let process = unsafe { GetCurrentProcess() };
    for flag in [
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    ] {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: flag,
            StateMask: 0,
        };
        unsafe {
            let _ = SetProcessInformation(
                process,
                ProcessPowerThrottling,
                &state as *const PROCESS_POWER_THROTTLING_STATE as *const std::ffi::c_void,
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

unsafe extern "system" fn enum_monitor_cb(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _lprect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(lparam as *mut Vec<MonitorRect>) };

    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;

    if unsafe {
        GetMonitorInfoW(
            hmonitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
    } != 0
    {
        let rc = info.monitorInfo.rcMonitor;
        let primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;

        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        let _ = unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
        let scale = (dpi_x as f32 / 96.0).max(0.5);

        let name_end = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let name = String::from_utf16_lossy(&info.szDevice[..name_end]);

        let width = rc.right - rc.left;
        let height = rc.bottom - rc.top;
        if width > 0 && height > 0 {
            monitors.push(MonitorRect {
                name,
                x: rc.left,
                y: rc.top,
                width,
                height,
                scale,
                primary,
            });
        }
    }

    1 // TRUE: continue enumeration
}

/// Queries all active Windows monitors in physical virtual desktop coordinates.
pub fn query_desktop_geometry() -> Result<DesktopGeometry> {
    let mut monitors: Vec<MonitorRect> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(enum_monitor_cb),
            &mut monitors as *mut Vec<MonitorRect> as LPARAM,
        );
    }
    if monitors.is_empty() {
        return Ok(DesktopGeometry::fallback());
    }
    if !monitors.iter().any(|m| m.primary) {
        monitors[0].primary = true;
    }
    Ok(DesktopGeometry::new(monitors))
}

/// Runs Windows 11 diagnostics for `glide doctor`.
pub fn run_doctor() -> Result<DoctorReport> {
    init_dpi_awareness();
    let geometry = query_desktop_geometry()?;
    let notes = vec![
        "Per-Monitor DPI Awareness V2 enabled.".into(),
        "Raw Input (WM_INPUT) + low-level hooks ready for unaccelerated mouse deltas.".into(),
        "German/EU AltGr fake-LCtrl (scanCode 0x21D) filter active.".into(),
        "Tip: Allow UDP ports 24800 (QUIC) and 24801 (LAN auto-discovery) on Private networks in Windows Firewall.".into(),
    ];
    Ok(DoctorReport {
        os_kind: OsKind::Windows,
        display_info: format!("Windows Desktop ({} monitor(s))", geometry.monitors.len()),
        geometry,
        notes,
    })
}
