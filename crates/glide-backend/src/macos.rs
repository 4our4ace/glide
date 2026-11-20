//! macOS CoreGraphics / IOHID backend placeholder.
//!
//! Planned implementation:
//! - Capture: `CGEventTap` (`kCGHIDEventTap`) + `CGAssociateMouseAndMouseCursorPosition(false)`
//!   for raw unaccelerated mouse deltas, plus `IOHIDManager` for extra HID keys.
//! - Injection: `CGEventPost` with virtual keycodes from `glide_hid::HidUsage::to_macos_keycode`.
//! - Clipboard: `NSPasteboard` change-count monitor + UTF-8/PNG read/write.

use crate::{CaptureConfig, CaptureHandle, ClipboardHandle, DoctorReport, InputInjector};
use anyhow::{bail, Result};

pub fn start_macos_capture(_config: CaptureConfig) -> Result<CaptureHandle> {
    bail!("macOS capture backend is not yet implemented (Linux X11 and Windows 11 are supported in v0.1)")
}

pub fn create_macos_injector() -> Result<Box<dyn InputInjector>> {
    bail!("macOS injector backend is not yet implemented (Linux X11 and Windows 11 are supported in v0.1)")
}

pub fn start_macos_clipboard() -> Result<ClipboardHandle> {
    bail!("macOS clipboard backend is not yet implemented (Linux X11 and Windows 11 are supported in v0.1)")
}

pub fn run_doctor() -> Result<DoctorReport> {
    bail!("macOS doctor backend is not yet implemented")
}
