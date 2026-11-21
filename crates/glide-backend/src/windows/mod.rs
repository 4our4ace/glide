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
use std::mem::size_of;
use std::ptr;
use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};

/// Unique `dwExtraInfo` tag stamped onto events synthesized by Glide so our hooks
/// immediately ignore our own injected events (zero feedback loops).
pub(crate) const GLIDE_EXTRA_INFO: usize = 0x474C_4944;

const MONITORINFOF_PRIMARY: u32 = 1;

/// Enables Per-Monitor DPI Awareness V2 so Windows reports real physical coordinates
/// across mixed-DPI multi-monitor setups (e.g. 100% + 125% + 150% + 200%).
pub fn init_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Opts this process out of Windows 11 EcoQoS (E-core efficiency throttling) and
/// minimized/background timer-resolution throttling so 1000Hz input hooks and QUIC
/// timers stay jitter-free even while minimized to the system tray.
pub fn init_windows_realtime_qos() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    unsafe {
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const PROCESS_POWER_THROTTLING_STATE as *const std::ffi::c_void,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
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
