//! Pure-Rust Linux X11 backend powered by `x11rb` (zero system C dev dependencies).
//!
//! Uses:
//! - XRandR for multi-monitor & HiDPI virtual desktop geometry
//! - XInput2 (`XI_Raw*`) for raw unaccelerated hardware mouse deltas & evdev keycodes
//! - XTest for low-latency relative mouse injection, scroll, buttons, and raw keycodes
//! - XFixes + ICCCM selections for event-driven bidirectional UTF-8 & PNG clipboard sync

pub mod capture;
pub mod clipboard;
pub mod dnd;
pub mod inject;
pub mod tray;
pub mod window_brand;

use crate::geometry::{DesktopGeometry, MonitorRect};
use crate::DoctorReport;
use anyhow::{Context, Result};
use glide_proto::OsKind;
use x11rb::connection::Connection;
use x11rb::protocol::randr::{ConnectionExt as RandrExt, NotifyMask};
use x11rb::protocol::xfixes::ConnectionExt as XFixesExt;
use x11rb::protocol::xinput::ConnectionExt as XInputExt;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as XProtoExt,
    CreateWindowAux, EventMask, InputFocus, MapState, Window, WindowClass, CLIENT_MESSAGE_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// Subscribes `conn` to XRandR screen/CRTC/output change events on `root` (best-effort).
pub fn subscribe_randr_events(conn: &RustConnection, root: Window) -> bool {
    let mask = match conn
        .randr_query_version(1, 2)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        Some(v) if v.major_version > 1 || (v.major_version == 1 && v.minor_version >= 2) => {
            NotifyMask::SCREEN_CHANGE | NotifyMask::CRTC_CHANGE | NotifyMask::OUTPUT_CHANGE
        }
        Some(_) => NotifyMask::SCREEN_CHANGE,
        None => return false,
    };
    match conn.randr_select_input(root, mask) {
        Ok(cookie) => cookie.check().is_ok(),
        Err(_) => false,
    }
}

/// Unsubscribes `conn` from XRandR change events on `root` (best-effort).
pub fn unsubscribe_randr_events(conn: &RustConnection, root: Window) {
    let _ = conn.randr_select_input(root, NotifyMask::from(0u16));
}

/// Drains queued X11 events non-blockingly and returns `true` if any XRandR layout event arrived.
pub fn poll_randr_event(conn: &RustConnection) -> bool {
    let mut changed = false;
    while let Ok(Some(ev)) = conn.poll_for_event() {
        if matches!(
            ev,
            Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_)
        ) {
            changed = true;
        }
    }
    changed
}

/// Queries the active multi-monitor geometry via XRandR (falling back to root screen size).
pub fn query_desktop_geometry(conn: &RustConnection, screen_num: usize) -> Result<DesktopGeometry> {
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let mut monitors = Vec::new();
    if let Ok(res_cookie) = conn.randr_get_screen_resources_current(root) {
        let primary_cookie = conn.randr_get_output_primary(root).ok();
        if let Ok(res) = res_cookie.reply() {
            let primary_output = primary_cookie
                .and_then(|c| c.reply().ok())
                .map(|r| r.output)
                .unwrap_or(0);

            // Pipeline all GetOutputInfo requests in a single roundtrip.
            let out_cookies: Vec<_> = res
                .outputs
                .iter()
                .filter_map(|&output| {
                    conn.randr_get_output_info(output, res.config_timestamp)
                        .ok()
                        .map(|c| (output, c))
                })
                .collect();

            // Pipeline all GetCrtcInfo requests for connected outputs in a single roundtrip.
            let crtc_cookies: Vec<_> = out_cookies
                .into_iter()
                .filter_map(|(output, c)| {
                    let out_info = c.reply().ok()?;
                    if out_info.crtc == 0 {
                        return None;
                    }
                    let crtc_cookie = conn
                        .randr_get_crtc_info(out_info.crtc, res.config_timestamp)
                        .ok()?;
                    Some((output, out_info.name, crtc_cookie))
                })
                .collect();

            for (output, raw_name, crtc_cookie) in crtc_cookies {
                let Ok(crtc) = crtc_cookie.reply() else {
                    continue;
                };
                if crtc.width == 0 || crtc.height == 0 {
                    continue;
                }
                let name = String::from_utf8_lossy(&raw_name).into_owned();
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

/// Safe wrapper around XFixes `HideCursor` / `ShowCursor` (v4.0+).
///
/// XFixes tracks a per-client hide counter on the X server, so calling `hide` twice without
/// an intervening `show` would require two `show` calls to unhide. This helper guarantees
/// balanced hide/show calls and gracefully degrades if XFixes < 4.0.
pub struct X11CursorHider {
    xfixes_ready: bool,
    hidden: bool,
}

impl X11CursorHider {
    pub fn init(conn: &RustConnection) -> Self {
        let xfixes_ready = conn
            .xfixes_query_version(4, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.major_version >= 4);
        Self {
            xfixes_ready,
            hidden: false,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    pub fn hide(&mut self, conn: &RustConnection, root: x11rb::protocol::xproto::Window) {
        if self.xfixes_ready && !self.hidden {
            match conn.xfixes_hide_cursor(root) {
                Ok(cookie) => {
                    cookie.ignore_error();
                    let _ = conn.flush();
                    self.hidden = true;
                }
                Err(e) => tracing::warn!(error = %e, "XFixesHideCursor send failed"),
            }
        }
    }

    pub fn show(&mut self, conn: &RustConnection, root: x11rb::protocol::xproto::Window) {
        if self.xfixes_ready && self.hidden {
            match conn.xfixes_show_cursor(root) {
                Ok(cookie) => {
                    cookie.ignore_error();
                    let _ = conn.flush();
                    self.hidden = false;
                }
                Err(e) => tracing::warn!(error = %e, "XFixesShowCursor send failed"),
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SavedX11Focus {
    /// Managed top-level window read from EWMH `_NET_ACTIVE_WINDOW` on `root`.
    Ewmh(Window),
    /// Direct X11 focus window read from `GetInputFocus` (fallback on non-EWMH WMs).
    Raw(Window),
}

/// Clears keyboard focus on X11 when focus leaves this machine and optionally restores focus to
/// the previously active window when control returns.
///
/// Lazily creates an offscreen 1×1 `InputOnly` `override_redirect` window (allocating zero X11
/// resources when unused), maps it only while unfocused, and directs `SetInputFocus` to it with
/// `revert_to = PointerRoot` so WMs like KWin/Mutter do not immediately jump focus to another app.
/// Unmapping the sink window on restore or discard ensures keyboard focus is never trapped on the
/// invisible window if the saved app closed, the session disconnected, or the toggle was disabled.
#[derive(Default)]
pub struct X11FocusClearer {
    win: Option<Window>,
    mapped: bool,
    saved_focus: Option<SavedX11Focus>,
    /// Cached `_NET_ACTIVE_WINDOW` atom (`None` = not yet queried, `Some(NONE)` = unavailable).
    net_active_atom: Option<Atom>,
}

impl X11FocusClearer {
    pub fn new() -> Self {
        Self {
            win: None,
            mapped: false,
            saved_focus: None,
            net_active_atom: None,
        }
    }

    fn resolve_net_active_atom(&mut self, conn: &RustConnection) -> Option<Atom> {
        let atom = match self.net_active_atom {
            Some(a) => a,
            None => {
                let interned = conn
                    .intern_atom(false, b"_NET_ACTIVE_WINDOW")
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| r.atom)
                    .unwrap_or(x11rb::NONE);
                self.net_active_atom = Some(interned);
                interned
            }
        };
        (atom != x11rb::NONE).then_some(atom)
    }

    fn query_ewmh_active_window(&mut self, conn: &RustConnection, root: Window) -> Option<Window> {
        let atom = self.resolve_net_active_atom(conn)?;
        let reply = conn
            .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        if reply.format != 32 || reply.value.len() < 4 {
            return None;
        }
        let win = u32::from_ne_bytes([
            reply.value[0],
            reply.value[1],
            reply.value[2],
            reply.value[3],
        ]);
        if win == x11rb::NONE || win == root || Some(win) == self.win {
            return None;
        }
        Some(win)
    }

    fn unmap_sink(&mut self, conn: &RustConnection) {
        if !self.mapped {
            return;
        }
        self.mapped = false;
        if let Some(w) = self.win {
            // Because SetInputFocus used revert_to = PointerRoot, unmapping releases focus
            // cleanly ONLY if `w` still owns it, without disturbing any window the user focused.
            if let Ok(cookie) = conn.unmap_window(w) {
                cookie.ignore_error();
            }
            let _ = conn.flush();
        }
    }

    /// Discards any saved window and unmaps the focus-sink window so keyboard focus is never
    /// stranded on the invisible window after disconnect, suspend/resume, or toggle-off.
    pub fn discard(&mut self, conn: &RustConnection) {
        self.saved_focus = None;
        self.unmap_sink(conn);
    }

    pub fn clear_focus(&mut self, conn: &RustConnection, root: Window) {
        let pointer_root = u32::from(u8::from(InputFocus::POINTER_ROOT));
        match conn.get_input_focus().ok().and_then(|c| c.reply().ok()) {
            Some(reply) if Some(reply.focus) == self.win => {
                // Focus is already on our sink (e.g. duplicate Leave or remote-to-remote switch);
                // preserve any previously saved window.
            }
            Some(reply)
                if reply.focus != x11rb::NONE
                    && reply.focus != pointer_root
                    && reply.focus != root =>
            {
                // Prefer the managed top-level EWMH _NET_ACTIVE_WINDOW over a raw child
                // focus-proxy or transient override_redirect popup when available.
                if let Some(ewmh_win) = self.query_ewmh_active_window(conn, root) {
                    self.saved_focus = Some(SavedX11Focus::Ewmh(ewmh_win));
                } else {
                    self.saved_focus = Some(SavedX11Focus::Raw(reply.focus));
                }
            }
            _ => {
                self.saved_focus = None;
            }
        }

        let target_win = match self.win {
            Some(w) => {
                if !self.mapped {
                    if let Ok(cookie) = conn.map_window(w) {
                        cookie.ignore_error();
                        self.mapped = true;
                    }
                }
                w
            }
            None => match conn.generate_id() {
                Ok(w) => {
                    let aux = CreateWindowAux::new().override_redirect(1);
                    let created = conn
                        .create_window(
                            0,
                            w,
                            root,
                            -10,
                            -10,
                            1,
                            1,
                            0,
                            WindowClass::INPUT_ONLY,
                            0,
                            &aux,
                        )
                        .and_then(|c| {
                            c.ignore_error();
                            conn.map_window(w)
                        });
                    match created {
                        Ok(map_cookie) => {
                            map_cookie.ignore_error();
                            self.win = Some(w);
                            self.mapped = true;
                            w
                        }
                        Err(e) => {
                            tracing::debug!(
                                error = %e,
                                "Failed creating X11 focus sink window; falling back to root"
                            );
                            root
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "Failed generating X11 window ID for focus sink; falling back to root"
                    );
                    root
                }
            },
        };

        match conn.set_input_focus(InputFocus::POINTER_ROOT, target_win, x11rb::CURRENT_TIME) {
            Ok(cookie) => {
                cookie.ignore_error();
                let _ = conn.flush();
            }
            Err(e) => tracing::debug!(error = %e, "X11 SetInputFocus send failed"),
        }
    }

    pub fn restore_focus(&mut self, conn: &RustConnection, root: Window) {
        let saved = self.saved_focus.take();

        let Some(saved) = saved else {
            self.unmap_sink(conn);
            return;
        };

        let pointer_root = u32::from(u8::from(InputFocus::POINTER_ROOT));
        let Some(cur) = conn.get_input_focus().ok().and_then(|c| c.reply().ok()) else {
            self.unmap_sink(conn);
            return;
        };
        let focus_still_cleared = Some(cur.focus) == self.win
            || cur.focus == x11rb::NONE
            || cur.focus == pointer_root
            || cur.focus == root;
        if !focus_still_cleared {
            self.unmap_sink(conn);
            return;
        }

        let target_win = match saved {
            SavedX11Focus::Ewmh(w) | SavedX11Focus::Raw(w) => w,
        };
        let Some(attrs) = conn
            .get_window_attributes(target_win)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            self.unmap_sink(conn);
            return;
        };
        if attrs.map_state != MapState::VIEWABLE {
            self.unmap_sink(conn);
            return;
        }

        // Restore X11 input focus directly (using POINTER_ROOT so it never falls back to root)
        // so WMs that kept `active_client == target_win` across our override_redirect sink still
        // restore keyboard focus immediately.
        match conn.set_input_focus(InputFocus::POINTER_ROOT, target_win, x11rb::CURRENT_TIME) {
            Ok(cookie) => cookie.ignore_error(),
            Err(e) => tracing::debug!(error = %e, "X11 restore SetInputFocus send failed"),
        }

        // Also send EWMH `_NET_ACTIVE_WINDOW` (source indication 2 = pager) for managed windows
        // so Mutter/KWin/Xfwm4/Openbox refresh decorations and deliver `WM_TAKE_FOCUS` to
        // ICCCM Globally Active clients (e.g. JetBrains/Java/Qt/Wine).
        if let SavedX11Focus::Ewmh(w) = saved {
            if let Some(atom) = self.resolve_net_active_atom(conn) {
                let ev = ClientMessageEvent {
                    response_type: CLIENT_MESSAGE_EVENT,
                    format: 32,
                    sequence: 0,
                    window: w,
                    type_: atom,
                    data: ClientMessageData::from([2u32, x11rb::CURRENT_TIME, 0, 0, 0]),
                };
                if let Ok(cookie) = conn.send_event(
                    false,
                    root,
                    EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                    ev,
                ) {
                    cookie.ignore_error();
                }
            }
        }

        // Unmap the sink AFTER moving focus off it so it stays clean and never intercepts input.
        self.unmap_sink(conn);
        let _ = conn.flush();
    }

    pub fn destroy(&mut self, conn: &RustConnection) {
        self.saved_focus = None;
        self.mapped = false;
        if let Some(w) = self.win.take() {
            if let Ok(cookie) = conn.destroy_window(w) {
                cookie.ignore_error();
            }
            let _ = conn.flush();
        }
    }
}

/// Non-blocking pipe sender used to wake a background `poll(2)` thread without touching
/// any X11 or Wayland connection mutex.
pub(crate) struct WakeSender {
    fd: std::os::fd::OwnedFd,
}

impl WakeSender {
    /// Signals the paired [`WakeReceiver`] without ever blocking.
    ///
    /// If the pipe buffer is already full (`EAGAIN`/`EWOULDBLOCK`), a wakeup byte is already
    /// queued for the receiver, so returning immediately is lossless.
    pub(crate) fn wake(&self) {
        use std::os::fd::AsRawFd;

        let byte: u8 = 1;
        loop {
            let rc = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    (&byte as *const u8).cast::<libc::c_void>(),
                    1,
                )
            };
            if rc >= 0 {
                return;
            }
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
    }
}

/// Non-blocking pipe receiver paired with [`WakeSender`].
pub(crate) struct WakeReceiver {
    fd: std::os::fd::OwnedFd,
}

impl WakeReceiver {
    pub(crate) fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }

    /// Drains all pending wakeup bytes non-blockingly.
    /// Returns `true` if the paired [`WakeSender`] was dropped (EOF).
    pub(crate) fn drain(&self) -> bool {
        use std::os::fd::AsRawFd;

        let mut buf = [0u8; 64];
        loop {
            let rc = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if rc > 0 {
                continue;
            }
            if rc == 0 {
                return true;
            }
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => return false,
                _ => return true,
            }
        }
    }
}

/// Creates a non-blocking, close-on-exec OS pipe pair for waking a `poll(2)` event loop.
pub(crate) fn wake_pipe() -> Result<(WakeSender, WakeReceiver)> {
    use std::os::fd::FromRawFd;

    let mut fds = [-1 as libc::c_int; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to create wake pipe");
    }
    let rx = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };
    let tx = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
    Ok((WakeSender { fd: tx }, WakeReceiver { fd: rx }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PollStatus {
    pub(crate) fd_ready: bool,
    pub(crate) fd_hup: bool,
    pub(crate) fd_error: bool,
    pub(crate) wake_ready: bool,
    pub(crate) wake_closed: bool,
    pub(crate) timed_out: bool,
}

/// Blocks in `poll(2)` until `fd` is readable, `wake` is signaled/closed, or `timeout_ms` elapses.
/// Automatically drains pending bytes from `wake` when signaled.
pub(crate) fn poll_fd_and_wake(
    fd: std::os::fd::RawFd,
    wake: &WakeReceiver,
    timeout_ms: i32,
) -> std::io::Result<PollStatus> {
    let mut pfds = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if rc == 0 {
            return Ok(PollStatus {
                fd_ready: false,
                fd_hup: false,
                fd_error: false,
                wake_ready: false,
                wake_closed: false,
                timed_out: true,
            });
        }

        let fd_rev = pfds[0].revents;
        let wake_rev = pfds[1].revents;

        let mut wake_closed = (wake_rev & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0;
        let wake_ready = (wake_rev & libc::POLLIN) != 0;
        if wake_ready && wake.drain() {
            wake_closed = true;
        }

        let fd_hup = (fd_rev & libc::POLLHUP) != 0;
        let fd_ready = (fd_rev & (libc::POLLIN | libc::POLLHUP)) != 0;
        let fd_error = (fd_rev & (libc::POLLERR | libc::POLLNVAL)) != 0;

        return Ok(PollStatus {
            fd_ready,
            fd_hup,
            fd_error,
            wake_ready,
            wake_closed,
            timed_out: false,
        });
    }
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

        // Leave hides cursor via XFixes and parks it away from the screen edge.
        // Because leave() is fire-and-forget on `inj`'s X11 socket, complete one roundtrip on
        // `inj` (`desktop_geometry()`) before observing from the separate test connection `conn`.
        inj.leave().expect("leave hides & parks cursor");
        let (px, py) = inj.desktop_geometry().unwrap().primary_park_corner();
        let parked = conn.query_pointer(root).unwrap().reply().unwrap();
        assert_eq!(
            (parked.root_x as i32, parked.root_y as i32),
            (px, py),
            "expected cursor parked at bottom-right corner on leave"
        );
        // Re-entering restores cursor to the entry edge and unhides it
        inj.enter(Edge::Left, 0.5).expect("re-enter unhides cursor");
        inj.show_cursor().expect("show_cursor is idempotent");

        // Smoke-test XInput2 capture grab/ungrab lifecycle
        let cap = capture::start_x11_capture(CaptureConfig::default()).expect("X11Capture start");
        cap.control.set_active_edges(&[Edge::Right]);
        cap.control.enter_remote().expect("enter_remote");
        std::thread::sleep(std::time::Duration::from_millis(50));
        cap.control
            .leave_remote(Edge::Right, 0.5)
            .expect("leave_remote");
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Smoke-test XFixes clipboard ownership and live UTF8_STRING serving via WakePipe
        let clip = clipboard::start_x11_clipboard().expect("X11Clipboard start");
        let text = Bytes::from_static("Grüße äöüß € @ |".as_bytes());
        clip.backend
            .set(ClipboardPayload {
                format: ClipboardFormat::TextUtf8,
                sha256: sha256_bytes(&text),
                data: text.clone(),
            })
            .expect("clipboard set");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let atom_clip = conn.intern_atom(false, b"CLIPBOARD").unwrap().reply().unwrap().atom;
        let atom_utf8 = conn.intern_atom(false, b"UTF8_STRING").unwrap().reply().unwrap().atom;
        let atom_prop = conn.intern_atom(false, b"GLIDE_CLIP_TEST").unwrap().reply().unwrap().atom;
        let req_win = conn.generate_id().unwrap();
        conn.create_window(
            0,
            req_win,
            root,
            -10,
            -10,
            1,
            1,
            0,
            x11rb::protocol::xproto::WindowClass::INPUT_ONLY,
            0,
            &x11rb::protocol::xproto::CreateWindowAux::new(),
        )
        .unwrap();
        conn.convert_selection(req_win, atom_clip, atom_utf8, atom_prop, x11rb::CURRENT_TIME)
            .unwrap();
        conn.flush().unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut got = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Ok(Some(Event::SelectionNotify(sn))) = conn.poll_for_event() {
                if sn.requestor == req_win && sn.property != x11rb::NONE {
                    let reply = conn
                        .get_property(
                            true,
                            req_win,
                            sn.property,
                            x11rb::protocol::xproto::AtomEnum::ANY,
                            0,
                            1024,
                        )
                        .unwrap()
                        .reply()
                        .unwrap();
                    got = reply.value;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got.as_slice(), text.as_ref(), "expected X11 clipboard to serve UTF8_STRING");
        let _ = conn.destroy_window(req_win);
        let _ = conn.flush();
    }

    #[test]
    fn wake_pipe_wakes_coalesces_times_out_and_detects_close() {
        let (_dummy_tx, dummy_rx) = wake_pipe().expect("dummy pipe");
        let (tx, rx) = wake_pipe().expect("wake pipe");

        // No pending wake -> times out cleanly
        let st = poll_fd_and_wake(dummy_rx.as_raw_fd(), &rx, 1).expect("poll timeout");
        assert!(st.timed_out);
        assert!(!st.wake_ready);
        assert!(!st.wake_closed);

        // Multiple wakes coalesce into a single wakeup and drain cleanly
        tx.wake();
        tx.wake();
        tx.wake();
        let st = poll_fd_and_wake(dummy_rx.as_raw_fd(), &rx, 50).expect("poll wake");
        assert!(st.wake_ready);
        assert!(!st.wake_closed);
        assert!(!st.timed_out);

        // Pipe is now drained -> times out again
        let st = poll_fd_and_wake(dummy_rx.as_raw_fd(), &rx, 1).expect("poll drained");
        assert!(st.timed_out);

        // Dropping sender signals EOF (wake_closed) immediately
        drop(tx);
        let st = poll_fd_and_wake(dummy_rx.as_raw_fd(), &rx, 50).expect("poll closed");
        assert!(st.wake_closed);
    }
}
