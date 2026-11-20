//! Cross-platform OS input capture, input injection, multi-monitor geometry,
//! and clipboard backends for Glide.
//!
//! Each OS implements the clean [`CaptureBackend`], [`InputInjector`], and
//! [`ClipboardBackend`] traits so `glide-core` stays 100% platform-agnostic.

pub mod geometry;
pub mod mock;

#[cfg(target_os = "linux")]
pub mod linux_x11;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

use anyhow::Result;
use geometry::DesktopGeometry;
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{ClipboardPayload, Edge, OsKind};
use tokio::sync::mpsc;

/// Configuration shared with platform capture backends.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Physical key that unconditionally returns focus to the local server desktop.
    pub escape_key: HidUsage,
    /// Pixels the user must push past an outer screen edge before switching (anti-accidental switch).
    pub edge_threshold_px: i32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            escape_key: HidUsage::SCROLL_LOCK,
            edge_threshold_px: 4,
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
}

/// Control handle to switch the OS capture backend between local and remote modes.
pub trait CaptureControl: Send + Sync {
    /// Updates the list of outer edges that have an active connected client.
    fn set_active_edges(&self, edges: &[Edge]);
    /// Enters remote mode: suppresses/grabs local input and streams raw events.
    fn enter_remote(&self) -> Result<()>;
    /// Leaves remote mode: releases local input grab and warps cursor to `return_edge`.
    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()>;
    /// Queries current multi-monitor desktop geometry.
    fn desktop_geometry(&self) -> DesktopGeometry;
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

    /// Called when focus enters this client at `edge` and `normalized_pos ∈ [0.0, 1.0]`.
    /// Places the OS cursor cleanly inside the corresponding monitor edge.
    fn enter(&mut self, edge: Edge, normalized_pos: f32) -> Result<()>;

    /// Injects a raw relative pointer delta `(dx, dy)` and optionally detects if the user
    /// pushed back past `return_edge` (the edge facing the server).
    ///
    /// Returns `Ok(Some((return_edge, normalized_pos)))` when the cursor should hand
    /// focus back to the server.
    fn move_relative(
        &mut self,
        dx: f32,
        dy: f32,
        return_edge: Option<Edge>,
        edge_threshold_px: i32,
    ) -> Result<Option<(Edge, f32)>>;

    /// Injects high-resolution scroll (`120` = 1 notch).
    fn scroll(&mut self, dx_120: i16, dy_120: i16) -> Result<()>;

    /// Injects a mouse button press/release.
    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()>;

    /// Injects a physical USB HID key press/release.
    fn key(&mut self, usage: HidUsage, pressed: bool) -> Result<()>;

    /// Releases every currently held key and mouse button (called on Leave/Disconnect).
    fn release_all(&mut self) -> Result<()>;
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
#[derive(Debug, Clone)]
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

/// Returns the native [`OsKind`] of the current build target.
pub fn current_os_kind() -> OsKind {
    #[cfg(target_os = "windows")]
    {
        OsKind::Windows
    }
    #[cfg(target_os = "linux")]
    {
        OsKind::LinuxX11
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
        linux_x11::capture::start_x11_capture(config)
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

/// Creates the native OS client input injector.
pub fn create_injector() -> Result<Box<dyn InputInjector>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux_x11::inject::X11Injector::connect()?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::inject::WindowsInjector::new()?))
    }
    #[cfg(target_os = "macos")]
    {
        macos::create_macos_injector()
    }
}

/// Starts the native OS bidirectional clipboard watcher & writer.
pub fn start_clipboard() -> Result<ClipboardHandle> {
    #[cfg(target_os = "linux")]
    {
        linux_x11::clipboard::start_x11_clipboard()
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
        linux_x11::run_doctor()
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
