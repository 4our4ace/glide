//! Pure-Rust Linux X11 backend powered by `x11rb` (zero system C dev dependencies).
//!
//! Uses:
//! - XRandR for multi-monitor & HiDPI virtual desktop geometry
//! - XInput2 (`XI_Raw*`) for raw unaccelerated hardware mouse deltas & evdev keycodes
//! - XTest for low-latency relative mouse injection, scroll, buttons, and raw keycodes
//! - XFixes + ICCCM selections for event-driven bidirectional UTF-8 & PNG clipboard sync

pub mod capture;
pub mod clipboard;
pub mod inject;

use crate::geometry::{DesktopGeometry, MonitorRect};
use crate::DoctorReport;
use anyhow::{Context, Result};
use glide_proto::OsKind;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xinput::ConnectionExt as XInputExt;
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::rust_connection::RustConnection;

/// Queries the active multi-monitor geometry via XRandR (falling back to root screen size).
pub fn query_desktop_geometry(conn: &RustConnection, screen_num: usize) -> Result<DesktopGeometry> {
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let mut monitors = Vec::new();
    if let Ok(res_cookie) = conn.randr_get_screen_resources_current(root) {
        if let Ok(res) = res_cookie.reply() {
            let primary_output = conn
                .randr_get_output_primary(root)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.output)
                .unwrap_or(0);

            for &output in &res.outputs {
                let Ok(out_cookie) = conn.randr_get_output_info(output, res.config_timestamp)
                else {
                    continue;
                };
                let Ok(out_info) = out_cookie.reply() else {
                    continue;
                };
                if out_info.crtc == 0 {
                    continue;
                }
                let Ok(crtc_cookie) = conn.randr_get_crtc_info(out_info.crtc, res.config_timestamp)
                else {
                    continue;
                };
                let Ok(crtc) = crtc_cookie.reply() else {
                    continue;
                };
                if crtc.width == 0 || crtc.height == 0 {
                    continue;
                }
                let name = String::from_utf8_lossy(&out_info.name).into_owned();
                monitors.push(MonitorRect {
                    name,
                    x: crtc.x as i32,
                    y: crtc.y as i32,
                    width: crtc.width as i32,
                    height: crtc.height as i32,
                    scale: 1.0,
                    primary: output == primary_output,
                });
            }
        }
    }

    if monitors.is_empty() {
        monitors.push(MonitorRect {
            name: "X11-root".into(),
            x: 0,
            y: 0,
            width: screen.width_in_pixels as i32,
            height: screen.height_in_pixels as i32,
            scale: 1.0,
            primary: true,
        });
    } else if !monitors.iter().any(|m| m.primary) {
        monitors[0].primary = true;
    }

    Ok(DesktopGeometry::new(monitors))
}

/// Runs X11 diagnostics for `glide doctor`.
pub fn run_doctor() -> Result<DoctorReport> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".into());
    let (conn, screen_num) = RustConnection::connect(None)
        .with_context(|| format!("failed to connect to X11 display (DISPLAY={display})"))?;

    let geometry = query_desktop_geometry(&conn, screen_num)?;
    let mut notes = Vec::new();

    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        notes.push(
            "WAYLAND_DISPLAY is set: running under XWayland. For full global capture/injection on Ubuntu 22.04, select an 'Ubuntu on Xorg' session at login.".into()
        );
    } else {
        notes.push("Native X11 session detected (no WAYLAND_DISPLAY).".into());
    }

    match conn.xtest_get_version(2, 2).map(|c| c.reply()) {
        Ok(Ok(v)) => notes.push(format!(
            "XTest extension: OK (v{}.{})",
            v.major_version, v.minor_version
        )),
        _ => notes.push("XTest extension: MISSING (input injection will fail)".into()),
    }

    match conn.xinput_xi_query_version(2, 2).map(|c| c.reply()) {
        Ok(Ok(v)) => notes.push(format!(
            "XInput2 extension: OK (v{}.{})",
            v.major_version, v.minor_version
        )),
        _ => notes.push("XInput2 extension: MISSING (raw mouse/key capture will fail)".into()),
    }

    match conn.xfixes_query_version(5, 0).map(|c| c.reply()) {
        Ok(Ok(v)) => notes.push(format!(
            "XFixes extension: OK (v{}.{})",
            v.major_version, v.minor_version
        )),
        _ => notes.push("XFixes extension: MISSING (clipboard notifications will fail)".into()),
    }

    if std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .is_ok()
    {
        notes.push("Optional /dev/uinput hardware-native HID backend: READY".into());
    } else {
        notes.push("Optional /dev/uinput backend: not writable (XTest works out of the box; install 99-glide-uinput.rules to opt in)".into());
    }

    Ok(DoctorReport {
        os_kind: OsKind::LinuxX11,
        display_info: format!("DISPLAY={display} (screen {screen_num})"),
        geometry,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureConfig, InputInjector};
    use bytes::Bytes;
    use glide_hid::{HidUsage, MouseButton};
    use glide_net::sha256_bytes;
    use glide_proto::{ClipboardFormat, ClipboardPayload, Edge};
    use x11rb::protocol::xproto::ConnectionExt as XProtoExt;

    #[test]
    #[ignore = "requires an active X11 DISPLAY (e.g. Xvfb :99)"]
    fn live_x11_injector_capture_and_clipboard_smoke() {
        let mut inj = inject::X11Injector::connect().expect("X11Injector connect");
        inj.enter(Edge::Left, 0.5).expect("enter left 0.5");

        // Move right 100px, down 20px and verify the real X11 cursor actually moved!
        let ret = inj
            .move_relative(100.0, 20.0, Some(Edge::Left), 4, 24)
            .expect("move right");
        assert!(ret.is_none());

        let (conn, screen_num) = RustConnection::connect(None).unwrap();
        let root = conn.setup().roots[screen_num].root;
        let ptr = conn.query_pointer(root).unwrap().reply().unwrap();
        assert!(
            ptr.root_x >= 90 && ptr.root_x <= 120,
            "expected cursor near x~104, got {}",
            ptr.root_x
        );

        // Inject German AltGr + ISO < > | + scroll + click + release_all cleanly
        inj.key(HidUsage::ALT_RIGHT, true).unwrap();
        inj.key(HidUsage::ISO_BACKSLASH, true).unwrap();
        inj.button(MouseButton::Left, true).unwrap();
        inj.scroll(0, -120).unwrap();
        inj.release_all().unwrap();

        // Push cursor back past the Left edge and verify edge-return triggers!
        let _ = inj
            .move_relative(-150.0, 0.0, Some(Edge::Left), 4, 24)
            .unwrap();
        let crossed = inj
            .move_relative(-20.0, 0.0, Some(Edge::Left), 4, 24)
            .unwrap();
        assert!(
            crossed.is_some(),
            "expected pushing past Left edge to trigger return"
        );
        let (edge, norm) = crossed.unwrap();
        assert_eq!(edge, Edge::Left);
        assert!(
            norm > 0.3 && norm < 0.7,
            "expected norm near 0.5, got {norm}"
        );

        // Smoke-test XInput2 capture grab/ungrab lifecycle
        let cap = capture::start_x11_capture(CaptureConfig::default()).expect("X11Capture start");
        cap.control.set_active_edges(&[Edge::Right]);
        cap.control.enter_remote().expect("enter_remote");
        std::thread::sleep(std::time::Duration::from_millis(50));
        cap.control
            .leave_remote(Edge::Right, 0.5)
            .expect("leave_remote");
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Smoke-test XFixes clipboard ownership
        let clip = clipboard::start_x11_clipboard().expect("X11Clipboard start");
        let text = Bytes::from_static("Grüße äöüß € @ |".as_bytes());
        clip.backend
            .set(ClipboardPayload {
                format: ClipboardFormat::TextUtf8,
                sha256: sha256_bytes(&text),
                data: text,
            })
            .expect("clipboard set");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
