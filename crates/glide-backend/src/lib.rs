//! Cross-platform OS input capture, input injection, multi-monitor geometry,
//! clipboard, and OS integration backends for Glide.
//!
//! Each OS implements [`CaptureControl`], [`InputInjector`], and [`ClipboardBackend`]
//! so `glide-core` and `glide-gui` stay 100% platform-agnostic.

pub mod dib_png;
pub mod geometry;
pub mod icon;
pub mod mock;
pub mod os_integration;

/// User actions emitted by the native OS system tray (Windows 11 & Linux StatusNotifierItem).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    OpenUi,
    ToggleLock,
    Quit,
}

#[cfg(target_os = "linux")]
pub mod linux_uinput;
#[cfg(target_os = "linux")]
pub mod linux_wayland;
#[cfg(target_os = "linux")]
pub mod linux_x11;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

use anyhow::Result;
use geometry::DesktopGeometry;
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{ClipboardPayload, Edge, LockState, OsKind};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Client input injection backend selector (Linux supports XTest, Wayland Portal/EIS, and kernel `uinput`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputBackendKind {
    /// Automatically choose the best native backend (`Portal/EIS` on Wayland, `XTest` on X11, `SendInput` on Windows).
    #[default]
    Auto,
    /// Explicitly use X11 XTest on Linux.
    XTest,
    /// Explicitly use xdg-desktop-portal RemoteDesktop / EIS on Linux Wayland.
    Portal,
    /// Linux virtual kernel HID device (`/dev/uinput`) for 100% native libinput acceleration & hi-res scroll.
    Uinput,
}

/// Runtime-tunable capture settings shared with OS capture threads.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Physical key that unconditionally returns focus to the local server desktop.
    pub escape_key: HidUsage,
    /// Pixels the user must push past an outer screen edge before switching.
    pub edge_threshold_px: i32,
    /// Corner dead-zone in pixels (prevents accidental switches on close `X` / taskbar corners).
    pub dead_corner_px: i32,
    /// Optional dwell time in milliseconds before crossing an edge (0 = instant).
    pub edge_dwell_ms: u64,
    /// Pause edge switching while an exclusive fullscreen app/game has focus on the server.
    pub auto_lock_fullscreen: bool,
    /// Enable `Ctrl + Alt + Arrow` direct screen switching and `Ctrl + Alt + L` cursor lock toggle.
    pub enable_hotkeys: bool,
    /// Keep the server's local CapsLock / NumLock state & keyboard LEDs in sync while controlling a client.
    pub sync_lock_keys: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            escape_key: HidUsage::SCROLL_LOCK,
            edge_threshold_px: 4,
            dead_corner_px: 24,
            edge_dwell_ms: 0,
            auto_lock_fullscreen: true,
            enable_hotkeys: true,
            sync_lock_keys: true,
        }
    }
}

/// Low-latency input events emitted by the server's OS capture thread.
#[derive(Debug, Clone)]
pub enum CaptureEvent {
    /// Local cursor pushed past an active outer screen edge.
    EdgeCrossed { edge: Edge, normalized_pos: f32 },
    /// Raw, unaccelerated hardware mouse delta captured while in remote mode.
    MouseDelta { dx: f32, dy: f32 },
    /// High-resolution mouse scroll delta (`120` = 1 notch).
    MouseScroll { dx_120: i16, dy_120: i16 },
    /// Physical mouse button transition while in remote mode.
    MouseButton { button: MouseButton, pressed: bool },
    /// Physical USB HID key transition while in remote mode.
    Key { usage: HidUsage, pressed: bool },
    /// Local escape hotkey pressed — unconditionally return control to server.
    EscapeHotkey,
    /// Keyboard hotkey (`Ctrl+Alt+Arrow`) requesting a direct jump in `direction`.
    HotkeySwitch { direction: Edge },
    /// Keyboard hotkey (`Ctrl+Alt+L`) toggling cursor lock to the current screen.
    HotkeyToggleLock,
    /// Display topology, resolution, or DPI scaling changed (monitor hotplug / scale update).
    GeometryChanged(DesktopGeometry),
}

/// Control handle to switch the OS capture backend between local and remote modes
/// and hot-update tuning parameters at runtime.
pub trait CaptureControl: Send + Sync {
    /// Updates the list of outer edges that have an active connected client.
    fn set_active_edges(&self, edges: &[Edge]);
    /// Hot-updates edge tuning & cursor lock without restarting capture.
    fn update_tuning(
        &self,
        edge_threshold_px: i32,
        dead_corner_px: i32,
        edge_dwell_ms: u64,
        auto_lock_fullscreen: bool,
        locked: bool,
    );
    /// Updates whether CapsLock / NumLock keys should also toggle local OS state & LEDs in remote mode.
    fn set_sync_lock_keys(&self, _enabled: bool) {}
    /// Enters remote mode: suppresses/grabs local input and parks/hides cursor cleanly.
    fn enter_remote(&self) -> Result<()>;
    /// Leaves remote mode: releases grab and warps cursor to `return_edge` at `normalized_pos`.
    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()>;
    /// Leaves remote mode via hotkey: restores cursor to saved normalized desktop coordinates.
    fn leave_remote_at(&self, norm_x: f32, norm_y: f32) -> Result<()>;
    /// Queries current multi-monitor desktop geometry.
    fn desktop_geometry(&self) -> DesktopGeometry;
    /// Reads current CapsLock / NumLock state on the server.
    fn lock_state(&self) -> LockState;
    /// Returns the last known normalized `(nx, ny)` local cursor position before leaving server.
    fn last_local_cursor_norm(&self) -> (f32, f32);
}

/// Pair returned when starting a server input capture backend.
pub struct CaptureHandle {
    pub control: Box<dyn CaptureControl>,
    pub events: mpsc::UnboundedReceiver<CaptureEvent>,
}

/// Client-side OS input injection trait.
pub trait InputInjector: Send {
    fn os_kind(&self) -> OsKind;

    /// Returns the current multi-monitor layout.
    fn desktop_geometry(&mut self) -> Result<DesktopGeometry>;

    /// Called when focus enters this client via a screen border.
    fn enter(&mut self, edge: Edge, normalized_pos: f32) -> Result<()>;

    /// Called when focus enters this client via keyboard hotkey (restoring cursor memory).
    fn enter_at(&mut self, norm_x: f32, norm_y: f32) -> Result<()>;

    /// Returns the client's current normalized `(nx, ny)` cursor position.
    fn cursor_norm(&mut self) -> (f32, f32);

    /// Synchronizes CapsLock / NumLock state on the client to match `desired`.
    fn sync_lock_state(&mut self, desired: LockState) -> Result<()>;

    /// Injects a raw relative pointer delta `(dx, dy)` and optionally detects if the user
    /// pushed back past `return_edge` (respecting `dead_corner_px` and `edge_threshold_px`).
    fn move_relative(
        &mut self,
        dx: f32,
        dy: f32,
        return_edge: Option<Edge>,
        edge_threshold_px: i32,
        dead_corner_px: i32,
    ) -> Result<Option<(Edge, f32)>>;

    /// Injects high-resolution scroll (`120` = 1 notch).
    fn scroll(&mut self, dx_120: i16, dy_120: i16) -> Result<()>;

    /// Injects a mouse button press/release.
    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()>;

    /// Injects a physical USB HID key press/release.
    fn key(&mut self, usage: HidUsage, pressed: bool) -> Result<()>;

    /// Releases every currently held key and mouse button (called on Leave/Disconnect).
    fn release_all(&mut self) -> Result<()>;

    /// Called when focus leaves this client back to the server: hides and parks the cursor
    /// so it does not sit visibly on the screen edge while another machine is active.
    fn leave(&mut self) -> Result<()> {
        Ok(())
    }

    /// Ensures the local cursor is visible again (called on disconnect or shutdown).
    fn show_cursor(&mut self) -> Result<()> {
        Ok(())
    }

    /// Checks whether the user physically moved a local mouse/touchpad while the cursor was
    /// hidden and reveals it immediately if so.
    fn reveal_if_moved_locally(&mut self) {}

    /// Polls whether the OS reported a display hotplug, resolution, or DPI scaling change since
    /// the last check, updating cached geometry and returning the new layout if it changed.
    fn poll_geometry_changed(&mut self) -> Option<DesktopGeometry> {
        None
    }
}

/// Bidirectional OS clipboard backend (event-driven, zero polling).
pub trait ClipboardBackend: Send + Sync {
    /// Applies a clipboard payload received from a remote peer without triggering an echo loop.
    fn set(&self, payload: ClipboardPayload) -> Result<()>;
}

/// Pair returned when starting the clipboard monitor/writer backend.
pub struct ClipboardHandle {
    pub backend: Box<dyn ClipboardBackend>,
    pub local_changes: mpsc::UnboundedReceiver<ClipboardPayload>,
}

/// Diagnostic summary returned by `glide doctor`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub os_kind: OsKind,
    pub display_info: String,
    pub geometry: DesktopGeometry,
    pub notes: Vec<String>,
}

/// Performs one-time OS process initialization (e.g. Per-Monitor DPI Awareness V2 on Windows).
pub fn init_platform() {
    #[cfg(target_os = "windows")]
    windows::init_dpi_awareness();
}

/// Returns the native [`OsKind`] of the current build target (detecting Wayland vs X11 on Linux).
pub fn current_os_kind() -> OsKind {
    #[cfg(target_os = "windows")]
    {
        OsKind::Windows
    }
    #[cfg(target_os = "linux")]
    {
        if linux_wayland::is_wayland_session() {
            OsKind::LinuxWayland
        } else {
            OsKind::LinuxX11
        }
    }
    #[cfg(target_os = "macos")]
    {
        OsKind::MacOs
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        OsKind::Unknown
    }
}

/// Starts the native OS server capture backend.
pub fn start_capture(config: CaptureConfig) -> Result<CaptureHandle> {
    #[cfg(target_os = "linux")]
    {
        if linux_wayland::is_wayland_session() {
            linux_wayland::capture::start_wayland_capture(config)
        } else {
            linux_x11::capture::start_x11_capture(config)
        }
    }
    #[cfg(target_os = "windows")]
    {
        windows::capture::start_windows_capture(config)
    }
    #[cfg(target_os = "macos")]
    {
        macos::start_macos_capture(config)
    }
}

/// Creates the native OS client input injector using default backend selection.
pub fn create_injector() -> Result<Box<dyn InputInjector>> {
    create_injector_with_kind(InputBackendKind::Auto)
}

/// Creates the native OS client input injector, auto-selecting Wayland Portal/EIS or X11 XTest
/// and honoring explicit `Portal`, `XTest`, or `Uinput` overrides on Linux.
pub fn create_injector_with_kind(kind: InputBackendKind) -> Result<Box<dyn InputInjector>> {
    #[cfg(target_os = "linux")]
    {
        let wayland = linux_wayland::is_wayland_session();

        if kind == InputBackendKind::Uinput {
            match linux_uinput::UinputInjector::connect() {
                Ok(inj) => return Ok(Box::new(inj)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        wayland,
                        "Requested /dev/uinput backend unavailable; falling back to default session backend"
                    );
                }
            }
        }

        if kind == InputBackendKind::Portal || (wayland && kind != InputBackendKind::XTest) {
            match linux_wayland::inject::WaylandInjector::connect() {
                Ok(inj) => return Ok(Box::new(inj)),
                Err(e) if kind == InputBackendKind::Portal => return Err(e),
                Err(e) => {
                    // On Wayland, if RemoteDesktop portal is unavailable, try /dev/uinput if writable
                    // before surfacing the portal error.
                    if let Ok(inj) = linux_uinput::UinputInjector::connect() {
                        tracing::warn!(
                            error = %e,
                            "Wayland RemoteDesktop portal unavailable; using /dev/uinput fallback"
                        );
                        return Ok(Box::new(inj));
                    }
                    return Err(e);
                }
            }
        }

        Ok(Box::new(linux_x11::inject::X11Injector::connect()?))
    }
    #[cfg(target_os = "windows")]
    {
        let _ = kind;
        Ok(Box::new(windows::inject::WindowsInjector::new()?))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = kind;
        macos::create_macos_injector()
    }
}

/// Starts the native OS bidirectional clipboard watcher & writer.
pub fn start_clipboard() -> Result<ClipboardHandle> {
    #[cfg(target_os = "linux")]
    {
        if linux_wayland::is_wayland_session() {
            linux_wayland::clipboard::start_wayland_clipboard()
        } else {
            linux_x11::clipboard::start_x11_clipboard()
        }
    }
    #[cfg(target_os = "windows")]
    {
        windows::clipboard::start_windows_clipboard()
    }
    #[cfg(target_os = "macos")]
    {
        macos::start_macos_clipboard()
    }
}

/// Runs platform diagnostics (`glide doctor`).
pub fn run_doctor() -> Result<DoctorReport> {
    #[cfg(target_os = "linux")]
    {
        if linux_wayland::is_wayland_session() {
            linux_wayland::run_doctor()
        } else {
            linux_x11::run_doctor()
        }
    }
    #[cfg(target_os = "windows")]
    {
        windows::run_doctor()
    }
    #[cfg(target_os = "macos")]
    {
        macos::run_doctor()
    }
}
