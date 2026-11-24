//! Linux Wayland client input injector via `org.freedesktop.portal.RemoteDesktop`.
//!
//! Prefers direct `ConnectToEIS` (`reis` Sender) for ultra-low-latency motion, scroll,
//! buttons, and raw evdev keycodes, with automatic fallback to portal D-Bus `Notify*`
//! methods on older Wayland compositors (e.g. Ubuntu 22.04 GNOME 42).

use crate::geometry::DesktopGeometry;
use crate::linux_wayland::{
    call_portal_request, clear_restore_token, close_portal_session, extract_object_path,
    load_restore_token, mouse_button_to_evdev, next_portal_token, opt_str, save_restore_token,
    WaylandGeometryWatcher, PORTAL_DEST, PORTAL_PATH, REMOTE_DESKTOP_IFACE,
};
use crate::{InjectorEvent, InputInjector};
use anyhow::{anyhow, Context, Result};
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{Edge, LockState, OsKind};
use parking_lot::Mutex;
use reis::ei;
use reis::event::{Connection as EiConnection, Device as EiDevice, DeviceCapability, EiEvent};
use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::blocking::{Connection as DbusConnection, Proxy};
use zbus::zvariant::{OwnedFd, OwnedObjectPath, Value};

const DEVICE_TYPE_KEYBOARD: u32 = 1;
const DEVICE_TYPE_POINTER: u32 = 2;
const PERSIST_MODE_REVOKED: u32 = 2;

/// Returns `CLOCK_MONOTONIC` microseconds via vDSO as required by the `libei` protocol.
fn now_micros() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0 {
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add((ts.tv_nsec as u64) / 1_000)
    } else {
        0
    }
}

#[derive(Default)]
struct EisDevices {
    pointer_dev: Option<EiDevice>,
    pointer: Option<ei::Pointer>,
    pointer_abs_dev: Option<EiDevice>,
    pointer_abs: Option<ei::PointerAbsolute>,
    button_dev: Option<EiDevice>,
    button: Option<ei::Button>,
    scroll_dev: Option<EiDevice>,
    scroll: Option<ei::Scroll>,
    keyboard_dev: Option<EiDevice>,
    keyboard: Option<ei::Keyboard>,
    emulating: HashSet<EiDevice>,
    sequence: u32,
}

impl EisDevices {
    fn register_device(&mut self, dev: EiDevice) {
        if let Some(p) = dev.interface::<ei::Pointer>() {
            self.pointer = Some(p);
            self.pointer_dev = Some(dev.clone());
        }
        if let Some(pa) = dev.interface::<ei::PointerAbsolute>() {
            self.pointer_abs = Some(pa);
            self.pointer_abs_dev = Some(dev.clone());
        }
        if let Some(b) = dev.interface::<ei::Button>() {
            self.button = Some(b);
            self.button_dev = Some(dev.clone());
        }
        if let Some(sc) = dev.interface::<ei::Scroll>() {
            self.scroll = Some(sc);
            self.scroll_dev = Some(dev.clone());
        }
        if let Some(kb) = dev.interface::<ei::Keyboard>() {
            self.keyboard = Some(kb);
            self.keyboard_dev = Some(dev);
        }
    }

    fn has_core_devices(&self) -> bool {
        (self.pointer.is_some() || self.pointer_abs.is_some()) && self.keyboard.is_some()
    }

    fn unique_devices(&self) -> Vec<EiDevice> {
        let mut out: Vec<EiDevice> = Vec::new();
        for dev in [
            &self.pointer_dev,
            &self.pointer_abs_dev,
            &self.button_dev,
            &self.scroll_dev,
            &self.keyboard_dev,
        ]
        .into_iter()
        .flatten()
        {
            if !out.iter().any(|d| d == dev) {
                out.push(dev.clone());
            }
        }
        out
    }

    fn start_emulating_all(&mut self, ei_conn: &EiConnection) {
        let serial = ei_conn.serial();
        for dev in self.unique_devices() {
            if self.emulating.insert(dev.clone()) {
                self.sequence = self.sequence.wrapping_add(1).max(1);
                dev.device().start_emulating(serial, self.sequence);
            }
        }
        let _ = ei_conn.flush();
    }
}

struct EisSender {
    ei_conn: EiConnection,
    devices: Arc<Mutex<EisDevices>>,
}

impl EisSender {
    fn connect(fd: OwnedFd) -> Result<Self> {
        let std_fd: std::os::unix::io::OwnedFd = fd.into();
        let stream = UnixStream::from(std_fd);
        let devices = Arc::new(Mutex::new(EisDevices::default()));
        let bg_devs = Arc::clone(&devices);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<EiConnection>>(1);

        std::thread::Builder::new()
            .name("glide-wl-eis-pump".into())
            .spawn(move || {
                let ctx = match ei::Context::new(stream) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow!(
                            "failed to create EIS client context: {e}"
                        )));
                        return;
                    }
                };
                let (ei_conn, mut events) = match ctx
                    .handshake_blocking("Glide", ei::handshake::ContextType::Sender)
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow!("EIS sender handshake failed: {e}")));
                        return;
                    }
                };

                // Drain initial seat + device setup events (bounded time so a silent server never hangs).
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut bound_seat = false;
                while Instant::now() < deadline {
                    match events.next() {
                        Some(Ok(EiEvent::SeatAdded(seat_ev))) => {
                            seat_ev.seat.bind_capabilities(
                                DeviceCapability::Pointer
                                    | DeviceCapability::PointerAbsolute
                                    | DeviceCapability::Button
                                    | DeviceCapability::Scroll
                                    | DeviceCapability::Keyboard,
                            );
                            let _ = ei_conn.flush();
                            bound_seat = true;
                        }
                        Some(Ok(EiEvent::DeviceAdded(dev_ev))) => {
                            bg_devs.lock().register_device(dev_ev.device);
                        }
                        Some(Ok(EiEvent::DeviceResumed(dev_ev))) => {
                            let mut d = bg_devs.lock();
                            d.register_device(dev_ev.device);
                            if d.has_core_devices() {
                                break;
                            }
                        }
                        Some(Ok(EiEvent::Disconnected(_))) | None => {
                            let _ = ready_tx.send(Err(anyhow!(
                                "EIS server disconnected during device discovery"
                            )));
                            return;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            let _ =
                                ready_tx.send(Err(anyhow!("EIS event error during setup: {e}")));
                            return;
                        }
                    }
                    if bound_seat && bg_devs.lock().has_core_devices() {
                        break;
                    }
                }

                {
                    let mut d = bg_devs.lock();
                    if d.pointer.is_none() && d.pointer_abs.is_none() && d.keyboard.is_none() {
                        let _ = ready_tx.send(Err(anyhow!(
                            "EIS server offered neither pointer nor keyboard devices"
                        )));
                        return;
                    }
                    d.start_emulating_all(&ei_conn);
                }

                if ready_tx.send(Ok(ei_conn.clone())).is_err() {
                    return;
                }

                // Keep reading EIS server events so `ei_connection.ping` is always answered,
                // server serials stay up to date, and hotplugged/resumed devices start emulating.
                for ev in events {
                    match ev {
                        Ok(EiEvent::SeatAdded(s)) => {
                            s.seat.bind_capabilities(
                                DeviceCapability::Pointer
                                    | DeviceCapability::PointerAbsolute
                                    | DeviceCapability::Button
                                    | DeviceCapability::Scroll
                                    | DeviceCapability::Keyboard,
                            );
                            let _ = ei_conn.flush();
                        }
                        Ok(EiEvent::DeviceAdded(dev_ev)) => {
                            let mut d = bg_devs.lock();
                            d.register_device(dev_ev.device);
                            d.start_emulating_all(&ei_conn);
                        }
                        Ok(EiEvent::DeviceResumed(dev_ev)) => {
                            let mut d = bg_devs.lock();
                            d.register_device(dev_ev.device);
                            d.start_emulating_all(&ei_conn);
                        }
                        Ok(EiEvent::DevicePaused(dev_ev)) => {
                            bg_devs.lock().emulating.remove(&dev_ev.device);
                        }
                        Ok(EiEvent::Disconnected(_)) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            })?;

        let ei_conn = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("timed out waiting for EIS sender handshake"))??;

        Ok(Self { ei_conn, devices })
    }

    fn frame(&self, dev: &Option<EiDevice>) {
        if let Some(d) = dev {
            d.device().frame(self.ei_conn.serial(), now_micros());
        }
        let _ = self.ei_conn.flush();
    }

    fn motion_relative(&self, dx: f32, dy: f32) {
        let d = self.devices.lock();
        if let Some(p) = &d.pointer {
            p.motion_relative(dx, dy);
            self.frame(&d.pointer_dev);
        }
    }

    fn motion_absolute(&self, x: f32, y: f32) -> bool {
        let d = self.devices.lock();
        if let Some(pa) = &d.pointer_abs {
            pa.motion_absolute(x, y);
            self.frame(&d.pointer_abs_dev);
            true
        } else {
            false
        }
    }

    fn button(&self, evdev_btn: u32, pressed: bool) {
        let d = self.devices.lock();
        if let Some(b) = &d.button {
            let state = if pressed {
                ei::button::ButtonState::Press
            } else {
                ei::button::ButtonState::Released
            };
            b.button(evdev_btn, state);
            self.frame(&d.button_dev);
        }
    }

    fn scroll(&self, dx_120: i16, dy_120: i16) {
        let d = self.devices.lock();
        if let Some(sc) = &d.scroll {
            // libei scroll_discrete uses 120 = 1 detent, positive = down/right;
            // Glide dy_120 uses positive = up (Windows/X11 convention).
            sc.scroll_discrete(dx_120 as i32, -(dy_120 as i32));
            self.frame(&d.scroll_dev);
        }
    }

    fn key(&self, evdev_code: u32, pressed: bool) {
        let d = self.devices.lock();
        if let Some(kb) = &d.keyboard {
            let state = if pressed {
                ei::keyboard::KeyState::Press
            } else {
                ei::keyboard::KeyState::Released
            };
            kb.key(evdev_code, state);
            self.frame(&d.keyboard_dev);
        }
    }
}

impl Drop for EisSender {
    fn drop(&mut self) {
        let serial = self.ei_conn.serial();
        let mut d = self.devices.lock();
        for dev in d.emulating.drain() {
            dev.device().stop_emulating(serial);
        }
        self.ei_conn.connection().disconnect();
        let _ = self.ei_conn.flush();
    }
}

#[derive(Debug, PartialEq)]
enum DbusCmd {
    Motion { dx: f64, dy: f64 },
    /// Non-coalesced relative motion step used for two-step sweep-to-origin cursor warps.
    WarpMotion { dx: f64, dy: f64 },
    Button { code: i32, pressed: bool },
    Key { code: i32, pressed: bool },
    ScrollDiscrete { axis: u32, steps: i32 },
    Close,
}

/// Receives the next [`DbusCmd`], coalescing any immediately queued consecutive
/// `DbusCmd::Motion` messages into a single summed `(dx, dy)` while preserving exact
/// FIFO ordering before any `WarpMotion`, `Button`, `Key`, `ScrollDiscrete`, or `Close`.
fn recv_coalesced_dbus_cmd(
    rx: &std::sync::mpsc::Receiver<DbusCmd>,
    stashed: &mut Option<DbusCmd>,
) -> Option<DbusCmd> {
    let first = match stashed.take() {
        Some(cmd) => cmd,
        None => rx.recv().ok()?,
    };
    match first {
        DbusCmd::Motion { mut dx, mut dy } => {
            while let Ok(next) = rx.try_recv() {
                match next {
                    DbusCmd::Motion { dx: ndx, dy: ndy } => {
                        dx += ndx;
                        dy += ndy;
                    }
                    other => {
                        *stashed = Some(other);
                        break;
                    }
                }
            }
            Some(DbusCmd::Motion { dx, dy })
        }
        other => Some(other),
    }
}

enum Transport {
    Eis(EisSender),
    DbusPortal,
}

type SetupResult = Result<(Transport, DesktopGeometry, Option<WaylandGeometryWatcher>)>;
type InnerSetupResult = Result<(
    DbusConnection,
    OwnedObjectPath,
    Transport,
    DesktopGeometry,
    Option<WaylandGeometryWatcher>,
)>;

/// Wayland client input injector backed by `org.freedesktop.portal.RemoteDesktop` / EIS.
///
/// Leaves [`InputInjector::set_unfocus_on_leave`] as the default no-op because Wayland
/// compositors prohibit cross-client window focus clearing.
pub struct WaylandInjector {
    dbus_tx: std::sync::mpsc::Sender<DbusCmd>,
    transport: Transport,
    geometry: DesktopGeometry,
    geom_watcher: Option<WaylandGeometryWatcher>,
    event_rx: Option<mpsc::UnboundedReceiver<InjectorEvent>>,
    est_x: f32,
    est_y: f32,
    scroll_rem_x: i32,
    scroll_rem_y: i32,
    edge_push_accum: f32,
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u32>,
}

impl WaylandInjector {
    /// Opens an `org.freedesktop.portal.RemoteDesktop` session and connects to EIS when supported.
    pub fn connect() -> Result<Self> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<DbusCmd>();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<SetupResult>(1);
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Run all zbus::blocking portal setup, D-Bus Notify* fallback calls, and session teardown
        // on a dedicated non-Tokio OS thread so calling from inside Tokio never panics.
        std::thread::Builder::new()
            .name("glide-wl-rd-dbus".into())
            .spawn(move || {
                let setup = (|| -> InnerSetupResult {
                    let conn = DbusConnection::session().context(
                        "failed to connect to session D-Bus for Wayland RemoteDesktop portal",
                    )?;
                    let proxy = Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, REMOTE_DESKTOP_IFACE)
                        .context("failed to create RemoteDesktop portal proxy")?;

                    // 1. CreateSession
                    let handle_tok = next_portal_token("rd_create");
                    let sess_tok = next_portal_token("rd_sess");
                    let create_res =
                        call_portal_request(&conn, &handle_tok, Duration::from_secs(30), || {
                            let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
                            opt_str(&mut opts, "handle_token", &handle_tok);
                            opt_str(&mut opts, "session_handle_token", &sess_tok);
                            let p: OwnedObjectPath = proxy.call("CreateSession", &(opts,))?;
                            Ok(p)
                        })
                        .context("Wayland RemoteDesktop CreateSession failed")?;

                    let session_path = create_res
                        .get("session_handle")
                        .and_then(extract_object_path)
                        .ok_or_else(|| {
                            anyhow!("RemoteDesktop CreateSession returned no session_handle")
                        })?;

                    // 2. SelectDevices (with persistent restore_token so the user only approves once)
                    let saved_token = load_restore_token();
                    let select_tok = next_portal_token("rd_select");
                    let select_res =
                        call_portal_request(&conn, &select_tok, Duration::from_secs(30), || {
                            let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
                            opt_str(&mut opts, "handle_token", &select_tok);
                            opts.insert(
                                "types",
                                Value::from(DEVICE_TYPE_KEYBOARD | DEVICE_TYPE_POINTER),
                            );
                            opts.insert("persist_mode", Value::from(PERSIST_MODE_REVOKED));
                            if let Some(ref rt) = saved_token {
                                opt_str(&mut opts, "restore_token", rt);
                            }
                            let p: OwnedObjectPath =
                                proxy.call("SelectDevices", &(session_path.as_ref(), opts))?;
                            Ok(p)
                        });
                    if let Err(e) = select_res {
                        close_portal_session(&conn, &session_path.as_ref());
                        return Err(e).context("Wayland RemoteDesktop SelectDevices failed");
                    }

                    // 3. Start (may show one-time OS permission dialog)
                    let start_tok = next_portal_token("rd_start");
                    let start_res =
                        call_portal_request(&conn, &start_tok, Duration::from_secs(120), || {
                            let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
                            opt_str(&mut opts, "handle_token", &start_tok);
                            let p: OwnedObjectPath =
                                proxy.call("Start", &(session_path.as_ref(), "", opts))?;
                            Ok(p)
                        });
                    let start_res = match start_res {
                        Ok(r) => r,
                        Err(e) => {
                            // If a stale restore_token caused rejection, clear it so the next attempt prompts cleanly.
                            if saved_token.is_some() {
                                clear_restore_token();
                            }
                            close_portal_session(&conn, &session_path.as_ref());
                            return Err(e).context("Wayland RemoteDesktop Start failed");
                        }
                    };

                    if let Some(Ok(new_tok)) = start_res
                        .get("restore_token")
                        .map(|v| v.downcast_ref::<String>())
                    {
                        save_restore_token(&new_tok);
                    }

                    // 4. Prefer ConnectToEIS (libei), falling back to D-Bus Notify* on older portals.
                    let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
                    let transport = match proxy.call::<_, _, OwnedFd>(
                        "ConnectToEIS",
                        &(session_path.as_ref(), empty_opts),
                    ) {
                        Ok(fd) => match EisSender::connect(fd) {
                            Ok(eis) => {
                                info!("Wayland RemoteDesktop connected via native EIS (libei)");
                                Transport::Eis(eis)
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "EIS handshake failed; falling back to Wayland RemoteDesktop D-Bus mode"
                                );
                                Transport::DbusPortal
                            }
                        },
                        Err(e) => {
                            info!(
                                error = %e,
                                "ConnectToEIS unavailable on this compositor; using Wayland RemoteDesktop D-Bus mode"
                            );
                            Transport::DbusPortal
                        }
                    };

                    let (geom_watcher, geometry) =
                        match WaylandGeometryWatcher::start_with_events(Some(event_tx)) {
                        Ok(watcher) => {
                            let g = watcher.current();
                            (Some(watcher), g)
                        }
                        Err(e) => {
                            debug!(
                                error = %e,
                                "Wayland geometry observer unavailable; keeping fallback geometry"
                            );
                            (None, DesktopGeometry::fallback())
                        }
                    };
                    Ok((conn, session_path, transport, geometry, geom_watcher))
                })();

                let (conn, session_path, transport, geometry, geom_watcher) = match setup {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                if ready_tx
                    .send(Ok((transport, geometry, geom_watcher)))
                    .is_err()
                {
                    close_portal_session(&conn, &session_path.as_ref());
                    return;
                }

                let proxy = Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, REMOTE_DESKTOP_IFACE).ok();
                let mut stashed: Option<DbusCmd> = None;
                while let Some(cmd) = recv_coalesced_dbus_cmd(&cmd_rx, &mut stashed) {
                    match cmd {
                        DbusCmd::Motion { dx, dy } => {
                            if dx != 0.0 || dy != 0.0 {
                                if let Some(ref p) = proxy {
                                    let opts: HashMap<&str, Value<'_>> = HashMap::new();
                                    let _ = p.call_noreply(
                                        "NotifyPointerMotion",
                                        &(session_path.as_ref(), opts, dx, dy),
                                    );
                                }
                            }
                        }
                        DbusCmd::WarpMotion { dx, dy } => {
                            if let Some(ref p) = proxy {
                                let opts: HashMap<&str, Value<'_>> = HashMap::new();
                                let _ = p.call_noreply(
                                    "NotifyPointerMotion",
                                    &(session_path.as_ref(), opts, dx, dy),
                                );
                            }
                        }
                        DbusCmd::Button { code, pressed } => {
                            if let Some(ref p) = proxy {
                                let opts: HashMap<&str, Value<'_>> = HashMap::new();
                                let _ = p.call_noreply(
                                    "NotifyPointerButton",
                                    &(session_path.as_ref(), opts, code, u32::from(pressed)),
                                );
                            }
                        }
                        DbusCmd::Key { code, pressed } => {
                            if let Some(ref p) = proxy {
                                let opts: HashMap<&str, Value<'_>> = HashMap::new();
                                let _ = p.call_noreply(
                                    "NotifyKeyboardKeycode",
                                    &(session_path.as_ref(), opts, code, u32::from(pressed)),
                                );
                            }
                        }
                        DbusCmd::ScrollDiscrete { axis, steps } => {
                            if let Some(ref p) = proxy {
                                let opts: HashMap<&str, Value<'_>> = HashMap::new();
                                let _ = p.call_noreply(
                                    "NotifyPointerAxisDiscrete",
                                    &(session_path.as_ref(), opts, axis, steps),
                                );
                            }
                        }
                        DbusCmd::Close => {
                            close_portal_session(&conn, &session_path.as_ref());
                            break;
                        }
                    }
                }
            })?;

        let (transport, geometry, geom_watcher) = ready_rx
            .recv()
            .map_err(|_| anyhow!("Wayland RemoteDesktop setup thread exited unexpectedly"))??;
        let (cx, cy) = geometry.primary_center();

        Ok(Self {
            dbus_tx: cmd_tx,
            transport,
            geometry,
            geom_watcher,
            event_rx: Some(event_rx),
            est_x: cx as f32,
            est_y: cy as f32,
            scroll_rem_x: 0,
            scroll_rem_y: 0,
            edge_push_accum: 0.0,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
        })
    }

    fn maybe_refresh_geometry(&mut self) {
        let _ = self.poll_geometry_changed();
    }

    fn warp_to(&mut self, tx: i32, ty: i32) -> Result<()> {
        let (min_x, min_y, max_x, max_y) = self.geometry.bounds();
        let clamped_x = tx.clamp(min_x, max_x - 1);
        let clamped_y = ty.clamp(min_y, max_y - 1);

        let warped_abs = match &self.transport {
            Transport::Eis(eis) => eis.motion_absolute(clamped_x as f32, clamped_y as f32),
            Transport::DbusPortal => false,
        };

        if !warped_abs {
            // Compositor does not expose absolute pointer: sweep to top-left origin then move
            // to the exact target offset relative to (min_x, min_y). Use non-coalesced warp
            // steps so the two motions never sum together in the D-Bus fallback queue.
            let span_x = (max_x - min_x).max(1) as f32;
            let span_y = (max_y - min_y).max(1) as f32;
            self.emit_warp_step(-span_x * 1.5, -span_y * 1.5)?;
            let off_x = (clamped_x - min_x) as f32;
            let off_y = (clamped_y - min_y) as f32;
            if off_x != 0.0 || off_y != 0.0 {
                self.emit_warp_step(off_x, off_y)?;
            }
        }

        self.est_x = clamped_x as f32;
        self.est_y = clamped_y as f32;
        self.edge_push_accum = 0.0;
        Ok(())
    }

    fn emit_warp_step(&self, dx: f32, dy: f32) -> Result<()> {
        match &self.transport {
            Transport::Eis(eis) => {
                eis.motion_relative(dx, dy);
                Ok(())
            }
            Transport::DbusPortal => {
                let _ = self.dbus_tx.send(DbusCmd::WarpMotion {
                    dx: dx as f64,
                    dy: dy as f64,
                });
                Ok(())
            }
        }
    }

    fn emit_relative(&self, dx: f32, dy: f32) -> Result<()> {
        match &self.transport {
            Transport::Eis(eis) => {
                eis.motion_relative(dx, dy);
                Ok(())
            }
            Transport::DbusPortal => {
                let _ = self.dbus_tx.send(DbusCmd::Motion {
                    dx: dx as f64,
                    dy: dy as f64,
                });
                Ok(())
            }
        }
    }

    fn emit_evdev_button(&self, code: u32, pressed: bool) -> Result<()> {
        match &self.transport {
            Transport::Eis(eis) => {
                eis.button(code, pressed);
                Ok(())
            }
            Transport::DbusPortal => {
                let _ = self.dbus_tx.send(DbusCmd::Button {
                    code: code as i32,
                    pressed,
                });
                Ok(())
            }
        }
    }

    fn emit_evdev_key(&self, code: u32, pressed: bool) -> Result<()> {
        match &self.transport {
            Transport::Eis(eis) => {
                eis.key(code, pressed);
                Ok(())
            }
            Transport::DbusPortal => {
                let _ = self.dbus_tx.send(DbusCmd::Key {
                    code: code as i32,
                    pressed,
                });
                Ok(())
            }
        }
    }
}

impl InputInjector for WaylandInjector {
    fn os_kind(&self) -> OsKind {
        OsKind::LinuxWayland
    }

    fn take_event_rx(&mut self) -> Option<mpsc::UnboundedReceiver<InjectorEvent>> {
        self.event_rx.take()
    }

    fn apply_geometry(&mut self, geometry: DesktopGeometry) {
        if geometry != self.geometry {
            self.geometry = geometry;
            self.edge_push_accum = 0.0;
            let (min_x, min_y, max_x, max_y) = self.geometry.bounds();
            self.est_x = self.est_x.clamp(min_x as f32, (max_x - 1) as f32);
            self.est_y = self.est_y.clamp(min_y as f32, (max_y - 1) as f32);
            debug!(
                monitors = self.geometry.monitors.len(),
                primary_scale = self.geometry.primary_scale(),
                "WaylandInjector applied event-driven desktop geometry update"
            );
        }
    }

    fn desktop_geometry(&mut self) -> Result<DesktopGeometry> {
        if let Some(ref watcher) = self.geom_watcher {
            let _ = watcher.poll_changed();
            self.geometry = watcher.current();
        }
        Ok(self.geometry.clone())
    }

    fn poll_geometry_changed(&mut self) -> Option<DesktopGeometry> {
        let new_geom = self.geom_watcher.as_ref()?.poll_changed()?;
        if new_geom != self.geometry {
            self.apply_geometry(new_geom);
            return Some(self.geometry.clone());
        }
        None
    }

    fn enter(&mut self, edge: Edge, normalized_pos: f32) -> Result<()> {
        self.maybe_refresh_geometry();
        let (x, y) = self.geometry.point_on_edge(edge, normalized_pos, 4);
        self.warp_to(x, y)
    }

    fn enter_at(&mut self, norm_x: f32, norm_y: f32) -> Result<()> {
        self.maybe_refresh_geometry();
        let (x, y) = self.geometry.denormalize_point(norm_x, norm_y);
        self.warp_to(x, y)
    }

    fn cursor_norm(&mut self) -> (f32, f32) {
        self.geometry
            .normalize_point(self.est_x.round() as i32, self.est_y.round() as i32)
    }

    fn sync_lock_state(&mut self, _desired: LockState) -> Result<()> {
        // Wayland compositors do not expose global lock-LED query APIs to clients;
        // CapsLock / NumLock keypresses themselves pass through directly via evdev keycodes.
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
        if dx != 0.0 || dy != 0.0 {
            self.emit_relative(dx, dy)?;
        }

        let (min_x, min_y, max_x, max_y) = self.geometry.bounds();
        self.est_x = (self.est_x + dx).clamp(min_x as f32, (max_x - 1) as f32);
        self.est_y = (self.est_y + dy).clamp(min_y as f32, (max_y - 1) as f32);

        let Some(edge) = return_edge else {
            self.edge_push_accum = 0.0;
            return Ok(None);
        };

        let outward = match edge {
            Edge::Left => -dx,
            Edge::Right => dx,
            Edge::Top => -dy,
            Edge::Bottom => dy,
        };

        if outward > 0.0 {
            if let Some(norm) = self.geometry.hit_outer_edge_guarded(
                edge,
                self.est_x.round() as i32,
                self.est_y.round() as i32,
                2,
                dead_corner_px,
            ) {
                self.edge_push_accum += outward;
                if self.edge_push_accum >= edge_threshold_px.max(1) as f32 {
                    self.edge_push_accum = 0.0;
                    return Ok(Some((edge, norm)));
                }
            } else {
                self.edge_push_accum = 0.0;
            }
        } else {
            self.edge_push_accum = 0.0;
        }

        Ok(None)
    }

    fn scroll(&mut self, dx_120: i16, dy_120: i16) -> Result<()> {
        match &self.transport {
            Transport::Eis(eis) => {
                if dx_120 != 0 || dy_120 != 0 {
                    eis.scroll(dx_120, dy_120);
                }
            }
            Transport::DbusPortal => {
                self.scroll_rem_y += dy_120 as i32;
                let steps_y = self.scroll_rem_y / 120;
                if steps_y != 0 {
                    self.scroll_rem_y -= steps_y * 120;
                    // Portal axis 0 = vertical, positive = down -> negate Glide's positive-up
                    let _ = self.dbus_tx.send(DbusCmd::ScrollDiscrete {
                        axis: 0,
                        steps: -steps_y,
                    });
                }
                self.scroll_rem_x += dx_120 as i32;
                let steps_x = self.scroll_rem_x / 120;
                if steps_x != 0 {
                    self.scroll_rem_x -= steps_x * 120;
                    let _ = self.dbus_tx.send(DbusCmd::ScrollDiscrete {
                        axis: 1,
                        steps: steps_x,
                    });
                }
            }
        }
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        let code = mouse_button_to_evdev(button);
        if pressed {
            self.pressed_buttons.insert(code);
        } else {
            self.pressed_buttons.remove(&code);
        }
        self.emit_evdev_button(code, pressed)
    }

    fn key(&mut self, usage: HidUsage, pressed: bool) -> Result<()> {
        let Some(evdev) = usage.to_evdev() else {
            debug!(usage = ?usage, "unmapped HID usage on Wayland injector");
            return Ok(());
        };
        let code = evdev as u32;
        if pressed {
            if usage.modifier_bit() != 0 && self.pressed_keys.contains(&code) {
                return Ok(());
            }
        } else if !self.pressed_keys.contains(&code) {
            return Ok(());
        }
        self.emit_evdev_key(code, pressed)?;
        if pressed {
            self.pressed_keys.insert(code);
        } else {
            self.pressed_keys.remove(&code);
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<u32> = self.pressed_keys.drain().collect();
        for code in keys {
            let _ = self.emit_evdev_key(code, false);
        }
        let buttons: Vec<u32> = self.pressed_buttons.drain().collect();
        for code in buttons {
            let _ = self.emit_evdev_button(code, false);
        }
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        let (px, py) = self.geometry.primary_park_corner();
        let _ = self.warp_to(px, py);
        Ok(())
    }
}

impl Drop for WaylandInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
        let _ = self.dbus_tx.send(DbusCmd::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbus_motion_coalesces_consecutive_deltas_and_preserves_ordering() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(DbusCmd::Motion { dx: 1.5, dy: -2.0 }).unwrap();
        tx.send(DbusCmd::Motion { dx: 3.0, dy: 4.5 }).unwrap();
        tx.send(DbusCmd::Motion { dx: -0.5, dy: 1.5 }).unwrap();
        tx.send(DbusCmd::Button {
            code: 0x110,
            pressed: true,
        })
        .unwrap();
        tx.send(DbusCmd::Motion { dx: 10.0, dy: 0.0 }).unwrap();
        tx.send(DbusCmd::Motion { dx: 5.0, dy: -3.0 }).unwrap();
        tx.send(DbusCmd::Key {
            code: 30,
            pressed: true,
        })
        .unwrap();
        // Two-step sweep-to-origin warp must NOT coalesce!
        tx.send(DbusCmd::WarpMotion {
            dx: -2880.0,
            dy: -1620.0,
        })
        .unwrap();
        tx.send(DbusCmd::WarpMotion {
            dx: 4.0,
            dy: 540.0,
        })
        .unwrap();
        tx.send(DbusCmd::ScrollDiscrete { axis: 0, steps: -1 })
            .unwrap();
        tx.send(DbusCmd::Close).unwrap();
        drop(tx);

        let mut stashed = None;
        let mut out = Vec::new();
        while let Some(cmd) = recv_coalesced_dbus_cmd(&rx, &mut stashed) {
            out.push(cmd);
        }

        assert_eq!(
            out,
            vec![
                DbusCmd::Motion { dx: 4.0, dy: 4.0 },
                DbusCmd::Button {
                    code: 0x110,
                    pressed: true
                },
                DbusCmd::Motion { dx: 15.0, dy: -3.0 },
                DbusCmd::Key {
                    code: 30,
                    pressed: true
                },
                DbusCmd::WarpMotion {
                    dx: -2880.0,
                    dy: -1620.0
                },
                DbusCmd::WarpMotion {
                    dx: 4.0,
                    dy: 540.0
                },
                DbusCmd::ScrollDiscrete { axis: 0, steps: -1 },
                DbusCmd::Close,
            ]
        );
    }
}
