//! Pure-Rust Linux Wayland backend (zero system C dev dependencies).
//!
//! Uses:
//! - `wayland-client` + `zxdg_output_manager_v1` for multi-monitor logical desktop geometry
//! - `org.freedesktop.portal.RemoteDesktop` + `libei` (`reis`) for client input injection
//!   (with automatic fallback to portal D-Bus `Notify*` methods on older compositors)
//! - `org.freedesktop.portal.InputCapture` + `libei` (`reis`) for server input capture & barriers
//! - `wl-clipboard-rs` (`ext-data-control` / `wlr-data-control`) for event-driven clipboard sync
//!   (with automatic XWayland fallback on GNOME < 49)

pub mod capture;
pub mod clipboard;
pub mod dnd;
pub mod inject;

use crate::geometry::{DesktopGeometry, MonitorRect};
use crate::DoctorReport;
use anyhow::{anyhow, Context, Result};
use glide_hid::MouseButton;
use glide_proto::OsKind;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection as WlConnection, Dispatch, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};
use zbus::blocking::{Connection as DbusConnection, MessageIterator, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

pub const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub const PORTAL_REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
pub const PORTAL_SESSION_IFACE: &str = "org.freedesktop.portal.Session";
pub const REMOTE_DESKTOP_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";
pub const INPUT_CAPTURE_IFACE: &str = "org.freedesktop.portal.InputCapture";

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Returns `true` when Glide is running inside a Wayland session.
///
/// Honors `GLIDE_SESSION=wayland|x11` as an explicit override for testing or troubleshooting.
pub fn is_wayland_session() -> bool {
    if let Ok(forced) = std::env::var("GLIDE_SESSION") {
        let f = forced.trim();
        if f.eq_ignore_ascii_case("wayland") {
            return true;
        }
        if f.eq_ignore_ascii_case("x11") || f.eq_ignore_ascii_case("xorg") {
            return false;
        }
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    std::env::var("XDG_SESSION_TYPE")
        .map(|s| s.trim().eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
}

pub fn mouse_button_to_evdev(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

pub fn evdev_to_mouse_button(code: u32) -> Option<MouseButton> {
    match code {
        BTN_LEFT => Some(MouseButton::Left),
        BTN_RIGHT => Some(MouseButton::Right),
        BTN_MIDDLE => Some(MouseButton::Middle),
        BTN_SIDE => Some(MouseButton::Back),
        BTN_EXTRA => Some(MouseButton::Forward),
        _ => None,
    }
}

pub fn next_portal_token(prefix: &str) -> String {
    let n = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("glide_{prefix}_{pid}_{n}")
}

/// Computes the deterministic portal request object path for `handle_token` on `conn`.
pub fn expected_request_path(conn: &DbusConnection, token: &str) -> Result<OwnedObjectPath> {
    let unique = conn
        .unique_name()
        .ok_or_else(|| anyhow!("D-Bus connection has no unique bus name"))?;
    let sender = unique.as_str().trim_start_matches(':').replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    Ok(OwnedObjectPath::try_from(path)?)
}

/// Calls an XDG Desktop Portal method that returns a `Request` handle and waits for its
/// `org.freedesktop.portal.Request::Response` signal without racing the reply.
pub fn call_portal_request<F>(
    conn: &DbusConnection,
    token: &str,
    timeout: Duration,
    invoke: F,
) -> Result<HashMap<String, OwnedValue>>
where
    F: FnOnce() -> Result<OwnedObjectPath>,
{
    let req_path = expected_request_path(conn, token)?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(PORTAL_DEST)?
        .interface(PORTAL_REQUEST_IFACE)?
        .member("Response")?
        .path(req_path.as_ref())?
        .build();

    let iter = MessageIterator::for_match_rule(rule, conn, Some(8))
        .context("failed to subscribe to XDG Desktop Portal Response signal")?;

    let returned_path = invoke()?;
    if returned_path != req_path {
        tracing::debug!(
            expected = %req_path,
            actual = %returned_path,
            "portal returned non-standard request path"
        );
    }

    let deadline = Instant::now() + timeout;
    for msg in iter {
        let msg = msg.context("D-Bus error waiting for XDG Desktop Portal response")?;
        if let Ok((code, results)) = msg.body().deserialize::<(u32, HashMap<String, OwnedValue>)>() {
            return match code {
                0 => Ok(results),
                1 => Err(anyhow!(
                    "Wayland permission request was cancelled by the user"
                )),
                other => Err(anyhow!(
                    "Wayland portal request failed with status code {other}"
                )),
            };
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for Wayland portal permission response"
            ));
        }
    }

    Err(anyhow!(
        "D-Bus stream closed before Wayland portal responded"
    ))
}

/// Executes a `zbus::blocking` closure safely even when the caller is on a Tokio runtime thread.
///
/// With `zbus`'s `tokio` feature enabled, `zbus::blocking` drives futures via Tokio's `block_on`,
/// which panics if invoked directly from an async Tokio worker thread.
pub fn on_non_tokio_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::Builder::new()
            .name("glide-wl-dbus-call".into())
            .spawn(f)
            .expect("failed to spawn Wayland D-Bus thread")
            .join()
            .expect("Wayland D-Bus helper thread panicked")
    } else {
        f()
    }
}

/// Closes a portal session handle cleanly (best-effort).
pub fn close_portal_session(conn: &DbusConnection, session_path: &ObjectPath<'_>) {
    if let Ok(proxy) = Proxy::new(conn, PORTAL_DEST, session_path.clone(), PORTAL_SESSION_IFACE) {
        let _ = proxy.call_noreply("Close", &());
    }
}

/// Extracts a D-Bus object path from a portal response value whether the compositor encoded it
/// as a `String` (`s`) or an `ObjectPath` (`o`).
pub fn extract_object_path(val: &OwnedValue) -> Option<OwnedObjectPath> {
    if let Ok(p) = val.downcast_ref::<ObjectPath<'_>>() {
        return OwnedObjectPath::try_from(p.as_str().to_owned()).ok();
    }
    if let Ok(s) = val.downcast_ref::<String>() {
        return OwnedObjectPath::try_from(s).ok();
    }
    None
}

fn restore_token_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("io", "glide", "glide")?;
    Some(dirs.config_dir().join("wayland_remote_desktop.token"))
}

pub fn load_restore_token() -> Option<String> {
    let path = restore_token_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn write_restore_token_file(path: &std::path::Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(token.as_bytes())?;
    Ok(())
}

pub fn save_restore_token(token: &str) {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(path) = restore_token_path() {
        if let Err(e) = write_restore_token_file(&path, trimmed) {
            tracing::debug!(error = %e, path = %path.display(), "failed to persist Wayland restore token");
        }
    }
}

pub fn clear_restore_token() {
    if let Some(path) = restore_token_path() {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Native Wayland multi-monitor logical geometry (wl_output + zxdg_output_v1)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct OutputEntry {
    global_name: u32,
    removed: bool,
    wl: Option<WlOutput>,
    xdg: Option<ZxdgOutputV1>,
    name: String,
    wl_x: i32,
    wl_y: i32,
    wl_w: i32,
    wl_h: i32,
    scale: i32,
    xdg_pos: Option<(i32, i32)>,
    xdg_size: Option<(i32, i32)>,
}

struct OutputQueryState {
    xdg_mgr: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputEntry>,
}

impl Dispatch<WlRegistry, ()> for OutputQueryState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &WlConnection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" {
                    let idx = state.outputs.len();
                    let wl = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, idx);
                    state.outputs.push(OutputEntry {
                        global_name: name,
                        removed: false,
                        wl: Some(wl),
                        scale: 1,
                        ..Default::default()
                    });
                } else if interface == "zxdg_output_manager_v1" && state.xdg_mgr.is_none() {
                    let mgr = registry.bind::<ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ());
                    state.xdg_mgr = Some(mgr);
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                for entry in &mut state.outputs {
                    if entry.global_name == name {
                        entry.removed = true;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, usize> for OutputQueryState {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        &idx: &usize,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.get_mut(idx) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                entry.wl_x = x;
                entry.wl_y = y;
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let is_current = match flags {
                    wayland_client::WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                    wayland_client::WEnum::Unknown(bits) => (bits & 0x1) != 0,
                };
                if is_current || entry.wl_w == 0 {
                    entry.wl_w = width;
                    entry.wl_h = height;
                }
            }
            wl_output::Event::Scale { factor } => {
                entry.scale = factor.max(1);
            }
            wl_output::Event::Name { name } if entry.name.is_empty() => {
                entry.name = name;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for OutputQueryState {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, usize> for OutputQueryState {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        &idx: &usize,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.get_mut(idx) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                entry.xdg_pos = Some((x, y));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                entry.xdg_size = Some((width, height));
            }
            zxdg_output_v1::Event::Name { name } if !name.is_empty() => {
                entry.name = name;
            }
            _ => {}
        }
    }
}

/// Computes an effective fractional scale factor from physical mode pixels and logical `xdg-output`
/// dimensions (rotation-invariant via area ratio), falling back to the integer `wl_output` scale.
pub(crate) fn compute_wayland_effective_scale(
    wl_w: i32,
    wl_h: i32,
    xdg_size: Option<(i32, i32)>,
    int_scale: i32,
) -> f32 {
    if let Some((lw, lh)) = xdg_size {
        if wl_w > 0 && wl_h > 0 && lw > 0 && lh > 0 {
            let ratio = ((wl_w as f64 * wl_h as f64) / (lw as f64 * lh as f64)).sqrt();
            if ratio.is_finite() {
                let rounded = (ratio * 100.0).round() / 100.0;
                return (rounded as f32).clamp(0.5, 4.0);
            }
        }
    }
    (int_scale.max(1) as f32).clamp(0.5, 4.0)
}

/// Queries the active multi-monitor logical layout directly from the Wayland compositor.
pub fn query_desktop_geometry() -> Result<DesktopGeometry> {
    let conn = WlConnection::connect_to_env()
        .context("failed to connect to Wayland display (is WAYLAND_DISPLAY set?)")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = OutputQueryState {
        xdg_mgr: None,
        outputs: Vec::new(),
    };
    queue
        .roundtrip(&mut state)
        .context("Wayland registry roundtrip failed")?;

    if let Some(mgr) = state.xdg_mgr.clone() {
        for (idx, entry) in state.outputs.iter_mut().enumerate() {
            if !entry.removed {
                if let Some(wl) = entry.wl.as_ref() {
                    entry.xdg = Some(mgr.get_xdg_output(wl, &qh, idx));
                }
            }
        }
        queue
            .roundtrip(&mut state)
            .context("Wayland xdg-output roundtrip failed")?;
    } else {
        let _ = queue.roundtrip(&mut state);
    }

    let mut monitors = Vec::new();
    for (idx, o) in state.outputs.into_iter().enumerate() {
        if o.removed {
            continue;
        }
        let (x, y) = o.xdg_pos.unwrap_or((o.wl_x, o.wl_y));
        let int_scale = o.scale.max(1) as f32;
        let (w, h) = o.xdg_size.unwrap_or((
            (o.wl_w as f32 / int_scale).round() as i32,
            (o.wl_h as f32 / int_scale).round() as i32,
        ));
        if w <= 0 || h <= 0 {
            continue;
        }
        let scale = compute_wayland_effective_scale(o.wl_w, o.wl_h, o.xdg_size, o.scale);
        let name = if o.name.is_empty() {
            format!("Wayland-{idx}")
        } else {
            o.name
        };
        monitors.push(MonitorRect {
            name,
            x,
            y,
            width: w,
            height: h,
            scale,
            primary: monitors.is_empty(),
        });
    }

    if monitors.is_empty() {
        return Ok(DesktopGeometry::fallback());
    }
    Ok(DesktopGeometry::new(monitors))
}

fn probe_portal_version(conn: &DbusConnection, iface: &str) -> Option<u32> {
    let proxy = Proxy::new(conn, PORTAL_DEST, PORTAL_PATH, iface).ok()?;
    proxy.get_property::<u32>("version").ok()
}

/// Runs Wayland diagnostics for `glide doctor`.
pub fn run_doctor() -> Result<DoctorReport> {
    let wayland_display =
        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "<unset>".into());
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into());

    let geometry = query_desktop_geometry().unwrap_or_else(|_| DesktopGeometry::fallback());
    let mut notes = vec![format!(
        "Native Wayland session detected (WAYLAND_DISPLAY={wayland_display}, desktop={desktop})."
    )];

    let dbus_notes = on_non_tokio_thread(|| {
        let mut out = Vec::new();
        match DbusConnection::session() {
            Ok(conn) => {
                if let Some(v) = probe_portal_version(&conn, REMOTE_DESKTOP_IFACE) {
                    out.push(format!(
                        "Portal RemoteDesktop (Client input): OK (v{v}{})",
                        if v >= 2 {
                            ", EIS supported"
                        } else {
                            ", D-Bus mode"
                        }
                    ));
                } else {
                    out.push(
                        "Portal RemoteDesktop (Client input): MISSING (install xdg-desktop-portal for your compositor, or use --backend uinput)"
                            .into(),
                    );
                }

                if let Some(v) = probe_portal_version(&conn, INPUT_CAPTURE_IFACE) {
                    out.push(format!(
                        "Portal InputCapture (Server capture): OK (v{v}, EIS supported)"
                    ));
                } else {
                    out.push(
                        "Portal InputCapture (Server capture): unavailable (requires GNOME 46+, KDE Plasma 6.1+, or Hyprland with xdg-desktop-portal >= 1.18)"
                            .into(),
                    );
                }
            }
            Err(e) => {
                out.push(format!("Session D-Bus unavailable: {e}"));
            }
        }
        out
    });
    notes.extend(dbus_notes);

    match wl_clipboard_rs::watch::Watcher::new(
        wl_clipboard_rs::watch::ClipboardType::Regular,
        wl_clipboard_rs::paste::Seat::Unspecified,
    ) {
        Ok(_) => notes.push(
            "Wayland clipboard (ext-data-control / wlr-data-control): OK (native)".into(),
        ),
        Err(_) if std::env::var_os("DISPLAY").is_some() => notes.push(
            "Wayland clipboard: using XWayland bridge (native ext-data-control not advertised by compositor)"
                .into(),
        ),
        Err(e) => notes.push(format!("Wayland clipboard: unavailable ({e})")),
    }

    notes.push(
        "Cross-edge file drop (Wayland): incoming files save to Downloads/Glide & copy file URIs for Ctrl+V (Wayland restricts synthetic external drops and hidden edge overlays)"
            .into(),
    );

    if std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .is_ok()
    {
        notes.push("Optional /dev/uinput hardware-native HID backend: READY".into());
    } else {
        notes.push(
            "Optional /dev/uinput backend: not writable (Portal works out of the box; install 99-glide-uinput.rules to opt in)"
                .into(),
        );
    }

    Ok(DoctorReport {
        os_kind: OsKind::LinuxWayland,
        display_info: format!("WAYLAND_DISPLAY={wayland_display} ({desktop})"),
        geometry,
        notes,
    })
}

/// Helper to insert a string option into an XDG Desktop Portal `a{sv}` map.
pub fn opt_str<'a>(map: &mut HashMap<&'a str, Value<'a>>, key: &'a str, val: &'a str) {
    map.insert(key, Value::from(val));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_button_evdev_roundtrip() {
        for btn in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
        ] {
            let code = mouse_button_to_evdev(btn);
            assert_eq!(evdev_to_mouse_button(code), Some(btn));
        }
        assert_eq!(evdev_to_mouse_button(0x999), None);
    }

    #[test]
    fn restore_token_file_created_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "glide-wl-token-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let token_path = dir.join("wayland_remote_desktop.token");
        write_restore_token_file(&token_path, "secret-restore-token").unwrap();

        let meta = std::fs::metadata(&token_path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&token_path).unwrap(),
            "secret-restore-token"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wayland_effective_scale_handles_fractional_and_rotated_outputs() {
        // 4K panel at 150% fractional scaling (landscape)
        assert!((compute_wayland_effective_scale(3840, 2160, Some((2560, 1440)), 2) - 1.5).abs() < 0.01);
        // Same panel rotated 90 degrees (portrait logical size)
        assert!((compute_wayland_effective_scale(3840, 2160, Some((1440, 2560)), 2) - 1.5).abs() < 0.01);
        // 125% fractional scaling on 1080p
        assert!((compute_wayland_effective_scale(1920, 1080, Some((1536, 864)), 2) - 1.25).abs() < 0.01);
        // Fallback to integer wl_output scale when xdg-output unavailable
        assert!((compute_wayland_effective_scale(3840, 2160, None, 2) - 2.0).abs() < 0.01);
        // Zero/degenerate sizes safely fall back without NaN
        assert!((compute_wayland_effective_scale(0, 0, Some((0, 0)), 1) - 1.0).abs() < 0.01);
    }
}
