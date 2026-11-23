//! Cross-platform OS input capture, input injection, multi-monitor geometry,
//! clipboard, and OS integration backends for Glide.
//!
//! Each OS implements [`CaptureControl`], [`InputInjector`], and [`ClipboardBackend`]
//! so `glide-core` and `glide-gui` stay 100% platform-agnostic.

pub mod clipboard_privacy;
pub mod dib_png;
pub mod dnd;
pub mod fullscreen;
pub mod geometry;
pub mod hotkeys;
pub mod icon;
pub mod mock;
pub mod os_integration;

pub use fullscreen::{
    classify_fullscreen_app, classify_fullscreen_identifiers, is_cursor_rect_confined,
    FullscreenAppKind,
};
pub use hotkeys::{CaptureHotkeys, HotkeyChord, HotkeyMods};

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
use glide_hid::{HidUsage, ModifierMask, MouseButton};
use glide_proto::{ClipboardPayload, Edge, LockState, OsKind};
use std::sync::atomic::{AtomicU8, Ordering};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

/// All four screen edges in deterministic iteration order.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const ALL_EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

#[inline]
pub(crate) const fn edge_bit(edge: Edge) -> u8 {
    match edge {
        Edge::Left => 1 << 0,
        Edge::Right => 1 << 1,
        Edge::Top => 1 << 2,
        Edge::Bottom => 1 << 3,
    }
}

/// Lock-free bitmask of active screen edges (replaces `Mutex<Vec<Edge>>` on 1000Hz hot paths).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) struct AtomicEdgeMask(AtomicU8);

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl AtomicEdgeMask {
    pub(crate) const fn empty() -> Self {
        Self(std::sync::atomic::AtomicU8::new(0))
    }

    pub(crate) fn set(&self, edges: &[Edge]) {
        let mut mask = 0u8;
        for &e in edges {
            mask |= edge_bit(e);
        }
        self.0.store(mask, std::sync::atomic::Ordering::Release);
    }

    #[inline]
    pub(crate) fn load(&self) -> u8 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Lock-free packed normalized `(f32, f32)` cursor coordinates.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) struct AtomicNormPoint(std::sync::atomic::AtomicU64);

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl AtomicNormPoint {
    pub(crate) fn new(nx: f32, ny: f32) -> Self {
        Self(std::sync::atomic::AtomicU64::new(Self::pack(nx, ny)))
    }

    #[inline]
    fn pack(nx: f32, ny: f32) -> u64 {
        ((nx.to_bits() as u64) << 32) | (ny.to_bits() as u64)
    }

    #[inline]
    pub(crate) fn store(&self, nx: f32, ny: f32) {
        self.0
            .store(Self::pack(nx, ny), std::sync::atomic::Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn load(&self) -> (f32, f32) {
        let bits = self.0.load(std::sync::atomic::Ordering::Relaxed);
        (
            f32::from_bits((bits >> 32) as u32),
            f32::from_bits(bits as u32),
        )
    }
}

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
    /// Customizable/disableable global shortcuts (screen switch, cursor lock, escape).
    pub hotkeys: CaptureHotkeys,
    /// Pixels the user must push past an outer screen edge before switching.
    pub edge_threshold_px: i32,
    /// Corner dead-zone in pixels (prevents accidental switches on close `X` / taskbar corners).
    pub dead_corner_px: i32,
    /// Optional dwell time in milliseconds before crossing an edge (0 = instant).
    pub edge_dwell_ms: u64,
    /// Pause edge switching while an exclusive fullscreen app/game has focus on the server.
    pub auto_lock_fullscreen: bool,
    /// Allow edge switching past fullscreen video players and non-pointer-locked fullscreen browsers
    /// while `auto_lock_fullscreen` is enabled (games and VMs stay locked).
    pub allow_fullscreen_videos: bool,
    /// Keep the server's local CapsLock / NumLock state & keyboard LEDs in sync while controlling a client.
    pub sync_lock_keys: bool,
    /// Enable cross-edge file drag-and-drop detection along active outer edges.
    pub drag_drop_files: bool,
    /// Keep USB Consumer Control media/volume keys on the local server PC instead of forwarding to clients.
    pub pin_media_keys_local: bool,
    /// Unfocus the active local window when pointer/keyboard focus leaves this machine.
    pub unfocus_on_leave: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            hotkeys: CaptureHotkeys::default(),
            edge_threshold_px: 4,
            dead_corner_px: 24,
            edge_dwell_ms: 0,
            auto_lock_fullscreen: true,
            allow_fullscreen_videos: true,
            sync_lock_keys: true,
            drag_drop_files: true,
            pin_media_keys_local: true,
            unfocus_on_leave: false,
        }
    }
}

/// Returns whether a physical key event should be suppressed locally and forwarded to the active
/// remote client, taking `pin_media_keys_local` into account.
#[inline]
pub const fn should_forward_key_in_remote(usage: HidUsage, pin_media_keys_local: bool) -> bool {
    !(pin_media_keys_local && usage.is_media_key())
}

/// Tracks media/consumer keys currently held down on the remote client so that toggling
/// `pin_media_keys_local` from `false` -> `true` (or `true` -> `false`) while a media key
/// is held still routes the matching release event to the same side (preventing stuck keys).
#[derive(Debug, Default)]
pub struct RemoteMediaKeyTracker {
    forwarded: Vec<HidUsage>,
}

impl RemoteMediaKeyTracker {
    pub const fn new() -> Self {
        Self {
            forwarded: Vec::new(),
        }
    }

    /// Decides whether `usage` (`pressed`) should be forwarded to the remote client,
    /// updating tracked forwarded media keys.
    pub fn should_forward(
        &mut self,
        usage: HidUsage,
        pressed: bool,
        pin_media_keys_local: bool,
    ) -> bool {
        if !usage.is_media_key() {
            return true;
        }
        if pressed {
            if should_forward_key_in_remote(usage, pin_media_keys_local) {
                if !self.forwarded.contains(&usage) {
                    self.forwarded.push(usage);
                }
                true
            } else {
                // Keep routing auto-repeat presses to the remote until released if the initial
                // press was already forwarded before `pin_media_keys_local` was toggled on.
                self.forwarded.contains(&usage)
            }
        } else if let Some(idx) = self.forwarded.iter().position(|&u| u == usage) {
            self.forwarded.swap_remove(idx);
            true
        } else {
            // Release of a media key that was never forwarded to the remote stays local so
            // local press/release stay balanced even if `pin_media_keys_local` toggled `true -> false`.
            false
        }
    }

    /// Returns `true` if `usage` was forwarded to the remote client and has not yet been released.
    #[inline]
    pub fn is_forwarded(&self, usage: HidUsage) -> bool {
        self.forwarded.contains(&usage)
    }

    /// Clears all tracked keys (called when leaving remote mode, where the client runs `release_all`).
    pub fn clear(&mut self) {
        self.forwarded.clear();
    }
}

/// Returns `true` if an unmapped Windows low-level keyboard event belongs to consumer, media,
/// browser, app-launch, sleep, or vendor/ACPI brightness keys that should pass through locally
/// when `pin_media_keys_local` is enabled.
#[inline]
pub const fn is_windows_consumer_or_media_vk(vk_code: u32, scan_code: u32, flags: u32) -> bool {
    const LLKHF_EXTENDED_FLAG: u32 = 0x01;
    // VK_BROWSER_BACK (0xA6) ..= VK_LAUNCH_APP2 (0xB7), VK_SLEEP (0x5F), vendor/ACPI Fn/brightness (0xFF)
    if matches!(vk_code, 0xA6..=0xB7 | 0x5F | 0xFF) {
        return true;
    }
    // Any remaining unmapped E0-extended scan code (all standard keyboard extended keys are in KEY_TABLE)
    (flags & LLKHF_EXTENDED_FLAG) != 0 && scan_code != 0
}

/// Windows virtual-key code for `L` (`VK_L`).
///
/// `win32k` triggers Lock Workstation on the active keyboard layout's `VK_L` (so this
/// matches `Win+L` across QWERTY, German QWERTZ, AZERTY, Dvorak, and Colemak).
pub const VK_L_CODE: u32 = 0x4C;

/// Returns `true` when the keystroke triggers Windows' unhookable kernel Lock Workstation
/// shortcut (`Win+L`, without Ctrl/Alt/Shift).
#[inline]
pub const fn is_windows_workstation_lock_chord(
    mask: ModifierMask,
    vk_code: u32,
    pressed: bool,
) -> bool {
    pressed
        && vk_code == VK_L_CODE
        && mask.meta()
        && !mask.ctrl()
        && !mask.alt()
        && !mask.shift()
}

/// Lock-free deferral state for bare Windows `MetaLeft` / `MetaRight` key-down events
/// while in remote mode.
///
/// Pressing `Win+L` triggers Windows' kernel workstation lock below `WH_KEYBOARD_LL`.
/// Deferring a fresh bare Meta key-down until the next non-repeat key, button, or scroll
/// event ensures `Win+L` never leaks a lone `Super` down+up tap to a Linux client (which
/// would open the GNOME/KDE Activities Overview), while genuine bare `Super` taps,
/// `Win+E`, `Win+Shift+S`, `Super+Click`, and `Super+Scroll` continue working seamlessly.
#[derive(Debug, Default)]
pub struct WindowsPendingMeta(AtomicU8);

impl WindowsPendingMeta {
    pub const fn new() -> Self {
        Self(AtomicU8::new(0))
    }

    #[inline]
    pub fn clear(&self) {
        self.0.store(0, Ordering::Release);
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }

    /// Evaluates whether a key event in remote mode should be deferred as a pending bare Meta press.
    ///
    /// Returns `true` if the caller should suppress forwarding `usage` for now (either a fresh
    /// bare `MetaLeft`/`MetaRight` press, or a typematic repeat while still deferred).
    #[inline]
    pub fn try_defer(
        &self,
        prev_mask: ModifierMask,
        mask: ModifierMask,
        usage: HidUsage,
        pressed: bool,
    ) -> bool {
        if !pressed || (usage != HidUsage::META_LEFT && usage != HidUsage::META_RIGHT) {
            return false;
        }
        let bit = usage.modifier_bit();
        let current = self.0.load(Ordering::Acquire);
        if (current & bit) != 0 {
            // Typematic WM_KEYDOWN repeat while this Meta key is still deferred.
            return true;
        }
        if !prev_mask.contains(bit) && !mask.ctrl() && !mask.alt() && !mask.shift() {
            self.0.fetch_or(bit, Ordering::AcqRel);
            return true;
        }
        false
    }

    /// Atomically takes any deferred `MetaLeft` / `MetaRight` key-down usages to flush to the client.
    #[inline]
    pub fn take_pending(&self) -> [Option<HidUsage>; 2] {
        if self.0.load(Ordering::Acquire) == 0 {
            return [None, None];
        }
        let bits = self.0.swap(0, Ordering::AcqRel);
        [
            ((bits & ModifierMask::META_LEFT) != 0).then_some(HidUsage::META_LEFT),
            ((bits & ModifierMask::META_RIGHT) != 0).then_some(HidUsage::META_RIGHT),
        ]
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
    /// Physical left mouse button transition while in local mode (never suppresses local clicks;
    /// used to arm edge drop targets and detect local drop releases).
    LocalLeftButton { pressed: bool },
    /// Hints that the local cursor with LMB held is approaching (`true`) or moved away from
    /// (`false`) an active outer edge, so invisible edge-strip drop targets are only shown
    /// while a local drag is armed.
    DragStripArmed(bool),
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
    /// OS is suspending or locking the interactive session — unconditionally release remote
    /// control, restore local cursor visibility, and release held keys on remote peers.
    SystemSuspended,
    /// OS woke from sleep/suspend or unlocked the session — refresh geometry, local LAN IPs,
    /// and connection health immediately.
    SystemResumed,
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
    /// Updates whether fullscreen video players and non-pointer-locked fullscreen browsers are
    /// exempt from `auto_lock_fullscreen`.
    fn set_allow_fullscreen_videos(&self, _enabled: bool) {}
    /// Updates whether CapsLock / NumLock keys should also toggle local OS state & LEDs in remote mode.
    fn set_sync_lock_keys(&self, _enabled: bool) {}
    /// Updates whether cross-edge file drag-and-drop assistance is enabled.
    fn set_drag_drop_files(&self, _enabled: bool) {}
    /// Attaches the active DnD backend so capture can perform lock-free staged-file checks
    /// before crossing an outer screen edge while LMB is held.
    fn set_dnd_probe(&self, _dnd: Arc<dyn dnd::DndBackend>) {}
    /// Updates whether USB Consumer Control media/volume keys remain pinned to the local server OS.
    fn set_pin_media_keys_local(&self, _enabled: bool) {}
    /// Updates whether the active local window should be unfocused when focus leaves to a remote client.
    fn set_unfocus_on_leave(&self, _enabled: bool) {}
    /// Discards any saved previously-focused local window so a subsequent `leave_remote*` does not
    /// restore focus to it (used on suspend, wake, peer disconnect, and shutdown).
    fn discard_saved_focus(&self) {}
    /// Hot-updates global capture shortcuts (screen switch, cursor lock, escape) without restarting capture.
    fn set_hotkeys(&self, _hotkeys: CaptureHotkeys) {}
    /// Temporarily suspends/resumes global shortcut interception (e.g. while recording a shortcut in UI).
    fn set_hotkeys_suspended(&self, _suspended: bool) {}
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

    /// Updates whether the active local window should be unfocused when focus leaves this client.
    fn set_unfocus_on_leave(&mut self, _enabled: bool) {}

    /// Discards any saved previously-focused local window so a subsequent `enter*` does not
    /// restore focus to it (used on suspend, wake, disconnect, and shutdown).
    fn discard_saved_focus(&mut self) {}

    /// Ensures the local cursor is visible again (called on disconnect or shutdown).
    fn show_cursor(&mut self) -> Result<()> {
        Ok(())
    }

    /// Returns `true` when the backend currently has the local cursor hidden after leaving the screen.
    fn is_cursor_hidden(&self) -> bool {
        false
    }

    /// Checks whether the user physically moved a local mouse/touchpad while the cursor was
    /// hidden and reveals it immediately if so.
    fn reveal_if_moved_locally(&mut self) {}

    /// Polls whether the OS reported a display hotplug, resolution, or DPI scaling change since
    /// the last check, updating cached geometry and returning the new layout if it changed.
    fn poll_geometry_changed(&mut self) -> Option<DesktopGeometry> {
        None
    }

    /// Returns `true` once if the OS entered sleep/suspend or locked the session since the last
    /// poll (callers should immediately release held keys and clear focus).
    fn poll_system_suspended(&mut self) -> bool {
        false
    }

    /// Returns `true` once if the OS resumed from sleep/suspend or unlocked the session since the
    /// last poll (callers should probe connection health and reset reconnect backoff).
    fn poll_system_resumed(&mut self) -> bool {
        false
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

/// Performs one-time OS process initialization (Per-Monitor DPI Awareness V2 and
/// Windows 11 EcoQoS / background timer-resolution throttling opt-out on Windows).
pub fn init_platform() {
    #[cfg(target_os = "windows")]
    {
        windows::init_dpi_awareness();
        windows::init_windows_realtime_qos();
    }
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

/// Starts the native OS cross-edge file drag-and-drop backend.
///
/// Falls back cleanly to [`dnd::NoopDndBackend`] if the platform display server does not
/// support it, so KVM input capture/injection never fails to start.
pub fn start_dnd() -> Arc<dyn dnd::DndBackend> {
    #[cfg(target_os = "linux")]
    {
        if linux_wayland::is_wayland_session() {
            linux_wayland::dnd::start_wayland_dnd()
        } else {
            match linux_x11::dnd::start_x11_dnd() {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "X11 DnD backend unavailable; falling back to no-op");
                    Arc::new(dnd::NoopDndBackend)
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        match windows::dnd::start_windows_dnd() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "Windows OLE DnD backend unavailable; falling back to no-op");
                Arc::new(dnd::NoopDndBackend)
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Arc::new(dnd::NoopDndBackend)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_keys_stay_local_when_pinned_and_forward_when_unpinned() {
        let media_names = [
            "AudioVolumeMute",
            "AudioVolumeDown",
            "AudioVolumeUp",
            "MediaPlayPause",
            "MediaTrackNext",
            "MediaTrackPrev",
            "MediaStop",
        ];
        for name in media_names {
            let usage = HidUsage::from_name(name).expect("known media key");
            assert_eq!(usage.page(), HidUsage::PAGE_CONSUMER);
            assert!(usage.is_media_key());
            assert!(
                !should_forward_key_in_remote(usage, true),
                "{name} must stay local when pin_media_keys_local is true"
            );
            assert!(
                should_forward_key_in_remote(usage, false),
                "{name} must forward when pin_media_keys_local is false"
            );
        }

        // Standard keyboard keys always forward regardless of pin_media_keys_local.
        for key in [
            HidUsage::from_name("KeyA").unwrap(),
            HidUsage::from_name("Space").unwrap(),
            HidUsage::ALT_RIGHT,
            HidUsage::ISO_BACKSLASH,
        ] {
            assert!(!key.is_media_key());
            assert!(should_forward_key_in_remote(key, true));
            assert!(should_forward_key_in_remote(key, false));
        }

        // Default CaptureConfig pins media keys locally, allows fullscreen videos, and keeps unfocus_on_leave disabled.
        assert!(CaptureConfig::default().pin_media_keys_local);
        assert!(CaptureConfig::default().allow_fullscreen_videos);
        assert!(!CaptureConfig::default().unfocus_on_leave);
    }

    #[test]
    fn media_key_tracker_forwards_release_after_live_pin_toggle_and_avoids_stuck_keys() {
        let mut tracker = RemoteMediaKeyTracker::new();
        let vol_up = HidUsage::from_name("AudioVolumeUp").unwrap();
        let key_a = HidUsage::from_name("KeyA").unwrap();

        // Standard keys always forward without being tracked
        assert!(tracker.should_forward(key_a, true, true));
        assert!(tracker.should_forward(key_a, false, true));

        // 1. Media key pressed with pin=false -> forwarded and tracked
        assert!(tracker.should_forward(vol_up, true, false));
        assert!(tracker.is_forwarded(vol_up));

        // 2. Toggle pin_media_keys_local false -> true while held:
        //    repeat press & release MUST still forward to remote (preventing stuck key!)
        assert!(tracker.should_forward(vol_up, true, true));
        assert!(tracker.should_forward(vol_up, false, true));
        assert!(!tracker.is_forwarded(vol_up));

        // 3. Subsequent press with pin=true stays local
        assert!(!tracker.should_forward(vol_up, true, true));
        assert!(!tracker.is_forwarded(vol_up));

        // 4. Toggle pin_media_keys_local true -> false while held locally:
        //    release stays local (never sends orphaned release to remote)
        assert!(!tracker.should_forward(vol_up, false, false));

        // 5. clear() resets tracked state on leaving remote mode
        assert!(tracker.should_forward(vol_up, true, false));
        assert!(tracker.is_forwarded(vol_up));
        tracker.clear();
        assert!(!tracker.is_forwarded(vol_up));
        assert!(!tracker.should_forward(vol_up, false, true));
    }

    #[test]
    fn windows_unmapped_consumer_and_browser_vks_are_detected() {
        // Browser / Media / Launch VKs (0xA6..=0xB7)
        for vk in 0xA6..=0xB7 {
            assert!(
                is_windows_consumer_or_media_vk(vk, 0, 0),
                "VK 0x{vk:02X} should be recognized as consumer/media"
            );
        }
        // Sleep & vendor/ACPI Fn brightness
        assert!(is_windows_consumer_or_media_vk(0x5F, 0, 0));
        assert!(is_windows_consumer_or_media_vk(0xFF, 0x6F, 0));
        // Unmapped extended scan code (e.g. consumer E0 scan code with non-consumer VK)
        assert!(is_windows_consumer_or_media_vk(0x00, 0x65, 0x01));
        // Regular non-extended unmapped key is NOT misclassified
        assert!(!is_windows_consumer_or_media_vk(0x41, 0x1E, 0));
        assert!(!is_windows_consumer_or_media_vk(0x00, 0x00, 0x01));
    }

    #[test]
    fn windows_workstation_lock_chord_matches_only_bare_win_l_press() {
        const VK_E: u32 = 0x45;

        assert!(is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_LEFT),
            VK_L_CODE,
            true
        ));
        assert!(is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_RIGHT),
            VK_L_CODE,
            true
        ));
        assert!(is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_LEFT | ModifierMask::META_RIGHT),
            VK_L_CODE,
            true
        ));
        // Release, other keys, or extra modifiers (Ctrl/Alt/Shift) must NOT match
        assert!(!is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_LEFT),
            VK_L_CODE,
            false
        ));
        assert!(!is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_LEFT),
            VK_E,
            true
        ));
        assert!(!is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_LEFT | ModifierMask::CONTROL_LEFT),
            VK_L_CODE,
            true
        ));
        assert!(!is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_LEFT | ModifierMask::CONTROL_RIGHT),
            VK_L_CODE,
            true
        ));
        assert!(!is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_LEFT | ModifierMask::SHIFT_LEFT),
            VK_L_CODE,
            true
        ));
        assert!(!is_windows_workstation_lock_chord(
            ModifierMask(ModifierMask::META_LEFT | ModifierMask::ALT_LEFT),
            VK_L_CODE,
            true
        ));
        assert!(!is_windows_workstation_lock_chord(
            ModifierMask::EMPTY,
            VK_L_CODE,
            true
        ));
    }

    #[test]
    fn windows_pending_meta_prevents_win_l_super_leak_and_preserves_chords() {
        const VK_E: u32 = 0x45;
        const VK_S: u32 = 0x53;
        const VK_LEFT: u32 = 0x25;
        const VK_LWIN: u32 = 0x5B;
        const VK_RWIN: u32 = 0x5C;
        const VK_LCTRL: u32 = 0xA2;
        const VK_LSHIFT: u32 = 0xA0;

        let key_l = HidUsage::from_name("KeyL").unwrap();
        let key_p = HidUsage::from_name("KeyP").unwrap(); // physical KeyP maps to VK_L on Dvorak
        let key_e = HidUsage::from_name("KeyE").unwrap();
        let key_s = HidUsage::from_name("KeyS").unwrap();
        let key_left = HidUsage::from_name("ArrowLeft").unwrap();

        struct SimHook {
            mods: ModifierMask,
            pending: WindowsPendingMeta,
            emitted: Vec<(HidUsage, bool)>,
            suspended: usize,
        }

        impl SimHook {
            fn new() -> Self {
                Self {
                    mods: ModifierMask::EMPTY,
                    pending: WindowsPendingMeta::new(),
                    emitted: Vec::new(),
                    suspended: 0,
                }
            }

            fn feed(&mut self, usage: HidUsage, vk_code: u32, pressed: bool) {
                let prev_mask = self.mods;
                self.mods.update(usage, pressed);
                let mask = self.mods;
                if is_windows_workstation_lock_chord(mask, vk_code, pressed) {
                    self.pending.clear();
                    self.mods = ModifierMask::EMPTY;
                    self.suspended += 1;
                    return;
                }
                if self.pending.try_defer(prev_mask, mask, usage, pressed) {
                    return;
                }
                for pending_meta in self.pending.take_pending().into_iter().flatten() {
                    self.emitted.push((pending_meta, true));
                }
                self.emitted.push((usage, pressed));
            }
        }

        // 1. Bare Win+L (QWERTY/QWERTZ/AZERTY and Dvorak where physical KeyP produces VK_L):
        //    ZERO Meta events emitted!
        for (meta_usage, meta_vk, phys_l) in [
            (HidUsage::META_LEFT, VK_LWIN, key_l),
            (HidUsage::META_RIGHT, VK_RWIN, key_l),
            (HidUsage::META_LEFT, VK_LWIN, key_p),
        ] {
            let mut sim = SimHook::new();
            sim.feed(meta_usage, meta_vk, true);
            assert!(sim.emitted.is_empty(), "bare Meta down must be deferred");
            assert!(!sim.pending.is_empty());
            sim.feed(phys_l, VK_L_CODE, true);
            assert_eq!(sim.suspended, 1);
            assert!(sim.emitted.is_empty(), "Win+L must never leak Super to client");
            assert!(sim.pending.is_empty());
        }

        // 2. Win hold typematic repeats + L -> still ZERO Meta emitted!
        {
            let mut sim = SimHook::new();
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            assert!(sim.emitted.is_empty());
            sim.feed(key_l, VK_L_CODE, true);
            assert_eq!(sim.suspended, 1);
            assert!(sim.emitted.is_empty());
            assert!(sim.pending.is_empty());
        }

        // 3. Genuine bare Win tap (down then up) -> flushes down then emits up!
        {
            let mut sim = SimHook::new();
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            assert!(sim.emitted.is_empty());
            sim.feed(HidUsage::META_LEFT, VK_LWIN, false);
            assert_eq!(
                sim.emitted,
                vec![(HidUsage::META_LEFT, true), (HidUsage::META_LEFT, false)]
            );
            assert!(sim.pending.is_empty());
            assert_eq!(sim.suspended, 0);
        }

        // 4. Win+E, Win+Left, Win+Shift+S all flush Meta down immediately before next key
        {
            let mut sim = SimHook::new();
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            sim.feed(key_e, VK_E, true);
            sim.feed(key_e, VK_E, false);
            sim.feed(HidUsage::META_LEFT, VK_LWIN, false);
            assert_eq!(
                sim.emitted,
                vec![
                    (HidUsage::META_LEFT, true),
                    (key_e, true),
                    (key_e, false),
                    (HidUsage::META_LEFT, false),
                ]
            );
        }
        {
            let mut sim = SimHook::new();
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            sim.feed(key_left, VK_LEFT, true);
            assert_eq!(
                sim.emitted,
                vec![(HidUsage::META_LEFT, true), (key_left, true)]
            );
        }
        {
            let mut sim = SimHook::new();
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            sim.feed(HidUsage::SHIFT_LEFT, VK_LSHIFT, true);
            sim.feed(key_s, VK_S, true);
            assert_eq!(
                sim.emitted,
                vec![
                    (HidUsage::META_LEFT, true),
                    (HidUsage::SHIFT_LEFT, true),
                    (key_s, true),
                ]
            );
        }

        // 5. Ctrl+Win+L and Shift+Win+L in BOTH key orders -> NOT lock, all forwarded!
        for (mod_key, mod_vk) in [
            (HidUsage::CONTROL_LEFT, VK_LCTRL),
            (HidUsage::SHIFT_LEFT, VK_LSHIFT),
        ] {
            // Order A: modifier first, then Win, then L
            let mut sim_a = SimHook::new();
            sim_a.feed(mod_key, mod_vk, true);
            sim_a.feed(HidUsage::META_LEFT, VK_LWIN, true);
            // Meta must NOT be deferred because Ctrl/Shift is already held!
            assert_eq!(
                sim_a.emitted,
                vec![(mod_key, true), (HidUsage::META_LEFT, true)]
            );
            assert!(sim_a.pending.is_empty());
            sim_a.feed(key_l, VK_L_CODE, true);
            assert_eq!(sim_a.suspended, 0);
            assert_eq!(
                sim_a.emitted,
                vec![(mod_key, true), (HidUsage::META_LEFT, true), (key_l, true)]
            );

            // Order B: Win first (deferred), then modifier (flushes Win), then L
            let mut sim_b = SimHook::new();
            sim_b.feed(HidUsage::META_LEFT, VK_LWIN, true);
            assert!(sim_b.emitted.is_empty());
            sim_b.feed(mod_key, mod_vk, true);
            assert_eq!(
                sim_b.emitted,
                vec![(HidUsage::META_LEFT, true), (mod_key, true)]
            );
            sim_b.feed(key_l, VK_L_CODE, true);
            assert_eq!(sim_b.suspended, 0);
            assert_eq!(
                sim_b.emitted,
                vec![(HidUsage::META_LEFT, true), (mod_key, true), (key_l, true)]
            );
        }

        // 6. MetaLeft + MetaRight held together
        {
            let mut sim = SimHook::new();
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            sim.feed(HidUsage::META_RIGHT, VK_RWIN, true);
            assert!(sim.emitted.is_empty());
            sim.feed(key_l, VK_L_CODE, true);
            assert_eq!(sim.suspended, 1);
            assert!(sim.emitted.is_empty());
            assert!(sim.pending.is_empty());
        }
        {
            let mut sim = SimHook::new();
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            sim.feed(HidUsage::META_RIGHT, VK_RWIN, true);
            sim.feed(key_e, VK_E, true);
            assert_eq!(
                sim.emitted,
                vec![
                    (HidUsage::META_LEFT, true),
                    (HidUsage::META_RIGHT, true),
                    (key_e, true),
                ]
            );
        }

        // 7. Post-flush Meta repeat is NOT re-deferred
        {
            let mut sim = SimHook::new();
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            sim.feed(key_e, VK_E, true);
            assert!(sim.pending.is_empty());
            // Typematic repeat of MetaLeft while still held after already flushed:
            sim.feed(HidUsage::META_LEFT, VK_LWIN, true);
            assert!(sim.pending.is_empty());
            assert_eq!(
                sim.emitted,
                vec![
                    (HidUsage::META_LEFT, true),
                    (key_e, true),
                    (HidUsage::META_LEFT, true),
                ]
            );
        }

        // 8. clear() on suspend/leave/escape drops deferred Meta without flushing
        {
            let pending = WindowsPendingMeta::new();
            let prev = ModifierMask::EMPTY;
            let mask = ModifierMask(ModifierMask::META_LEFT);
            assert!(pending.try_defer(prev, mask, HidUsage::META_LEFT, true));
            assert!(!pending.is_empty());
            pending.clear();
            assert!(pending.is_empty());
            assert_eq!(pending.take_pending(), [None, None]);
        }
    }
}
