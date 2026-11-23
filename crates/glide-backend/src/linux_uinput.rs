//! Optional Linux `/dev/uinput` virtual kernel HID injector (Step 6).
//!
//! Creates a real virtual USB mouse + keyboard device in the Linux kernel so
//! `libinput` applies Ubuntu's 100% native pointer acceleration, true 1/120th-detent
//! high-resolution scrolling (`REL_WHEEL_HI_RES` / `REL_HWHEEL_HI_RES`), and
//! works even over lock screens and raw-input games.
//!
//! Uses X11 RandR only for monitor geometry, cursor warp on screen entry, and
//! edge-return boundary checks.

use crate::geometry::DesktopGeometry;
use crate::linux_wayland::WaylandGeometryWatcher;
use crate::linux_x11::inject::should_sync_x11_pointer;
use crate::linux_x11::{
    poll_randr_event, query_desktop_geometry, subscribe_randr_events, unsubscribe_randr_events,
    X11CursorHider, X11FocusClearer,
};
use crate::InputInjector;
use anyhow::{anyhow, Context, Result};
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{Edge, LockState, OsKind};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as XProtoExt, Window};
use x11rb::rust_connection::RustConnection;

// Linux uinput ioctl numbers (x86_64 / generic asm-generic/ioctl.h)
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405C_5503;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0B;
const REL_HWHEEL_HI_RES: u16 = 0x0C;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

const BUS_USB: u16 = 0x03;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(deprecated)]
struct InputEvent {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

fn ioctl_int(fd: i32, req: libc::c_ulong, val: libc::c_int) -> Result<()> {
    let rc = unsafe { libc::ioctl(fd, req as _, val) };
    if rc < 0 {
        return Err(anyhow!(
            "uinput ioctl 0x{req:X}({val}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn mouse_btn_to_evdev(btn: MouseButton) -> u16 {
    match btn {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

struct X11Helper {
    conn: RustConnection,
    screen_num: usize,
    root: Window,
    randr_subscribed: bool,
    cursor_hider: X11CursorHider,
    focus_clearer: X11FocusClearer,
}

pub struct UinputInjector {
    file: File,
    x11: Option<X11Helper>,
    wayland_geom: Option<WaylandGeometryWatcher>,
    wayland: bool,
    geometry: DesktopGeometry,
    last_geom_refresh: Instant,
    parked_pos: (i32, i32),
    rem_x: f32,
    rem_y: f32,
    scroll_rem_x: i32,
    scroll_rem_y: i32,
    est_x: f32,
    est_y: f32,
    last_pointer_sync: Instant,
    edge_push_accum: f32,
    pressed_keys: HashSet<u16>,
    pressed_buttons: HashSet<u16>,
    unfocus_on_leave: bool,
}

impl UinputInjector {
    pub fn connect() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context(
                "cannot open /dev/uinput (add your user to the `input` group and install 99-glide-uinput.rules)",
            )?;
        let fd = file.as_raw_fd();

        ioctl_int(fd, UI_SET_EVBIT, EV_KEY as i32)?;
        ioctl_int(fd, UI_SET_EVBIT, EV_REL as i32)?;

        for rel in [
            REL_X,
            REL_Y,
            REL_WHEEL,
            REL_HWHEEL,
            REL_WHEEL_HI_RES,
            REL_HWHEEL_HI_RES,
        ] {
            ioctl_int(fd, UI_SET_RELBIT, rel as i32)?;
        }

        // Enable standard keyboard evdev range (1..=248) + mouse buttons
        for key in 1u16..=248 {
            let _ = ioctl_int(fd, UI_SET_KEYBIT, key as i32);
        }
        for btn in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
            ioctl_int(fd, UI_SET_KEYBIT, btn as i32)?;
        }

        let mut name = [0u8; 80];
        let label = b"Glide Virtual HID";
        name[..label.len()].copy_from_slice(label);

        let setup = UinputSetup {
            id: InputId {
                bustype: BUS_USB,
                vendor: 0x1D6B,
                product: 0x474C,
                version: 2,
            },
            name,
            ff_effects_max: 0,
        };
        let rc = unsafe { libc::ioctl(fd, UI_DEV_SETUP as _, &setup) };
        if rc < 0 {
            return Err(anyhow!(
                "UI_DEV_SETUP failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let rc = unsafe { libc::ioctl(fd, UI_DEV_CREATE as _) };
        if rc < 0 {
            return Err(anyhow!(
                "UI_DEV_CREATE failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let wayland = crate::linux_wayland::is_wayland_session();
        let (x11, wayland_geom, geometry) = if wayland {
            match WaylandGeometryWatcher::start() {
                Ok(watcher) => {
                    let geom = watcher.current();
                    (None, Some(watcher), geom)
                }
                Err(_) => (None, None, DesktopGeometry::fallback()),
            }
        } else {
            let (conn, screen_num) = RustConnection::connect(None)
                .context("failed to connect to X11 for geometry/warp")?;
            let root = conn.setup().roots[screen_num].root;
            let randr_subscribed = subscribe_randr_events(&conn, root);
            let cursor_hider = X11CursorHider::init(&conn);
            let geometry = query_desktop_geometry(&conn, screen_num)?;
            (
                Some(X11Helper {
                    conn,
                    screen_num,
                    root,
                    randr_subscribed,
                    cursor_hider,
                    focus_clearer: X11FocusClearer::new(),
                }),
                None,
                geometry,
            )
        };
        let (cx, cy) = geometry.primary_center();
        let now = Instant::now();

        debug!(
            wayland,
            "Initialized Linux /dev/uinput virtual HID device (hardware-native acceleration & hi-res scroll)"
        );

        Ok(Self {
            file,
            x11,
            wayland_geom,
            wayland,
            geometry,
            last_geom_refresh: now,
            parked_pos: (cx, cy),
            rem_x: 0.0,
            rem_y: 0.0,
            scroll_rem_x: 0,
            scroll_rem_y: 0,
            est_x: cx as f32,
            est_y: cy as f32,
            last_pointer_sync: now,
            edge_push_accum: 0.0,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            unfocus_on_leave: false,
        })
    }

    fn emit_events(&mut self, events: &[(u16, u16, i32)]) -> Result<()> {
        let total = events.len() + 1;
        let to_ev = |type_: u16, code: u16, value: i32| InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_,
            code,
            value,
        };
        if total <= 8 {
            let mut stack = [to_ev(0, 0, 0); 8];
            for (dst, &(type_, code, value)) in stack.iter_mut().zip(events.iter()) {
                *dst = to_ev(type_, code, value);
            }
            stack[events.len()] = to_ev(EV_SYN, SYN_REPORT, 0);
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    stack.as_ptr() as *const u8,
                    total * std::mem::size_of::<InputEvent>(),
                )
            };
            self.file.write_all(bytes).context("uinput write failed")?;
        } else {
            let mut vec = Vec::with_capacity(total);
            for &(type_, code, value) in events {
                vec.push(to_ev(type_, code, value));
            }
            vec.push(to_ev(EV_SYN, SYN_REPORT, 0));
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    vec.as_ptr() as *const u8,
                    vec.len() * std::mem::size_of::<InputEvent>(),
                )
            };
            self.file.write_all(bytes).context("uinput write failed")?;
        }
        Ok(())
    }

    fn refresh_geometry(&mut self) -> Result<()> {
        if let Some(ref x11) = self.x11 {
            let _ = poll_randr_event(&x11.conn);
            self.geometry = query_desktop_geometry(&x11.conn, x11.screen_num)?;
        } else if let Some(ref watcher) = self.wayland_geom {
            let _ = watcher.poll_changed();
            self.geometry = watcher.current();
        }
        self.last_geom_refresh = Instant::now();
        Ok(())
    }

    fn maybe_refresh_geometry(&mut self) {
        let _ = self.refresh_geometry_if_needed();
    }

    fn refresh_geometry_if_needed(&mut self) -> bool {
        let next = if let Some(ref x11) = self.x11 {
            if x11.randr_subscribed {
                if !poll_randr_event(&x11.conn) {
                    return false;
                }
            } else if self.last_geom_refresh.elapsed() < Duration::from_secs(15) {
                return false;
            }
            self.last_geom_refresh = Instant::now();
            query_desktop_geometry(&x11.conn, x11.screen_num).ok()
        } else if let Some(ref watcher) = self.wayland_geom {
            watcher.poll_changed()
        } else {
            None
        };
        if let Some(new_geom) = next {
            if new_geom != self.geometry {
                self.geometry = new_geom;
                self.edge_push_accum = 0.0;
                let (min_x, min_y, max_x, max_y) = self.geometry.bounds();
                self.est_x = self.est_x.clamp(min_x as f32, (max_x - 1) as f32);
                self.est_y = self.est_y.clamp(min_y as f32, (max_y - 1) as f32);
                return true;
            }
        }
        false
    }

    fn warp_to(&mut self, x: i32, y: i32) -> Result<()> {
        if let Some(ref mut x11) = self.x11 {
            x11.conn
                .warp_pointer(x11rb::NONE, x11.root, 0, 0, 0, 0, x as i16, y as i16)?;
            x11.cursor_hider.show(&x11.conn, x11.root);
            x11.conn.flush()?;
        } else {
            let (min_x, min_y, max_x, max_y) = self.geometry.bounds();
            let span_x = (max_x - min_x).max(1);
            let span_y = (max_y - min_y).max(1);
            self.emit_events(&[(EV_REL, REL_X, -(span_x * 2)), (EV_REL, REL_Y, -(span_y * 2))])?;
            let ox = x - min_x;
            let oy = y - min_y;
            if ox != 0 || oy != 0 {
                self.emit_events(&[(EV_REL, REL_X, ox), (EV_REL, REL_Y, oy)])?;
            }
        }
        self.rem_x = 0.0;
        self.rem_y = 0.0;
        self.est_x = x as f32;
        self.est_y = y as f32;
        self.edge_push_accum = 0.0;
        self.last_pointer_sync = Instant::now();
        Ok(())
    }

    fn sync_pointer(&mut self) -> Result<(i32, i32)> {
        if let Some(ref x11) = self.x11 {
            let reply = x11.conn.query_pointer(x11.root)?.reply()?;
            self.est_x = reply.root_x as f32;
            self.est_y = reply.root_y as f32;
            self.last_pointer_sync = Instant::now();
            Ok((reply.root_x as i32, reply.root_y as i32))
        } else {
            Ok((self.est_x.round() as i32, self.est_y.round() as i32))
        }
    }

    fn query_x11_lock_state(&self) -> Option<LockState> {
        let x11 = self.x11.as_ref()?;
        let reply = x11.conn.get_keyboard_control().ok()?.reply().ok()?;
        Some(LockState {
            caps_lock: (reply.led_mask & 1) != 0,
            num_lock: (reply.led_mask & 2) != 0,
        })
    }
}

impl InputInjector for UinputInjector {
    fn os_kind(&self) -> OsKind {
        if self.wayland {
            OsKind::LinuxWayland
        } else {
            OsKind::LinuxX11
        }
    }

    fn desktop_geometry(&mut self) -> Result<DesktopGeometry> {
        self.refresh_geometry()?;
        Ok(self.geometry.clone())
    }

    fn poll_geometry_changed(&mut self) -> Option<DesktopGeometry> {
        if self.refresh_geometry_if_needed() {
            Some(self.geometry.clone())
        } else {
            None
        }
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
        let (x, y) = self
            .sync_pointer()
            .unwrap_or((self.est_x.round() as i32, self.est_y.round() as i32));
        self.geometry.normalize_point(x, y)
    }

    fn sync_lock_state(&mut self, desired: LockState) -> Result<()> {
        let Some(current) = self.query_x11_lock_state() else {
            return Ok(());
        };
        if current.caps_lock != desired.caps_lock {
            let caps = HidUsage::keyboard(0x39).to_evdev().unwrap_or(58);
            let _ = self.emit_events(&[(EV_KEY, caps, 1)]);
            let _ = self.emit_events(&[(EV_KEY, caps, 0)]);
        }
        if current.num_lock != desired.num_lock {
            let num = HidUsage::keyboard(0x53).to_evdev().unwrap_or(69);
            let _ = self.emit_events(&[(EV_KEY, num, 1)]);
            let _ = self.emit_events(&[(EV_KEY, num, 0)]);
        }
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
        self.rem_x += dx;
        self.rem_y += dy;
        let ix = self.rem_x.trunc() as i32;
        let iy = self.rem_y.trunc() as i32;
        self.rem_x -= ix as f32;
        self.rem_y -= iy as f32;

        if ix != 0 || iy != 0 {
            let mut evs = [(0u16, 0u16, 0i32); 2];
            let mut n = 0;
            if ix != 0 {
                evs[n] = (EV_REL, REL_X, ix);
                n += 1;
            }
            if iy != 0 {
                evs[n] = (EV_REL, REL_Y, iy);
                n += 1;
            }
            self.emit_events(&evs[..n])?;
        }

        let (min_x, min_y, max_x, max_y) = self.geometry.bounds();
        self.est_x = (self.est_x + dx).clamp(min_x as f32, (max_x - 1) as f32);
        self.est_y = (self.est_y + dy).clamp(min_y as f32, (max_y - 1) as f32);

        let Some(edge) = return_edge else {
            return Ok(None);
        };
        let outward = match edge {
            Edge::Left => -dx,
            Edge::Right => dx,
            Edge::Top => -dy,
            Edge::Bottom => dy,
        };
        if outward <= 0.0 {
            self.edge_push_accum = 0.0;
            return Ok(None);
        }

        // Keep a low-frequency outward calibration sync (> 48px: 150ms) so nonlinear Xorg/libinput
        // acceleration or multi-monitor gaps can never leave est_x/est_y permanently drifted away
        // from the real cursor at the return edge, while keeping near-edge checks responsive.
        let dist = self.geometry.distance_to_edge(
            edge,
            self.est_x.round() as i32,
            self.est_y.round() as i32,
        );

        let mut synced_this_call = false;
        if should_sync_x11_pointer(dist, self.edge_push_accum, self.last_pointer_sync.elapsed()) {
            let _ = self.sync_pointer()?;
            synced_this_call = true;
        }

        let dist = if synced_this_call {
            self.geometry.distance_to_edge(
                edge,
                self.est_x.round() as i32,
                self.est_y.round() as i32,
            )
        } else {
            dist
        };
        if dist > 48 {
            self.edge_push_accum = 0.0;
            return Ok(None);
        }

        let px = self.est_x.round() as i32;
        let py = self.est_y.round() as i32;
        if let Some(norm) = self
            .geometry
            .hit_outer_edge_guarded(edge, px, py, 2, dead_corner_px)
        {
            self.edge_push_accum += outward;
            if self.edge_push_accum >= edge_threshold_px.max(1) as f32 {
                let confirmed_norm = if synced_this_call {
                    norm
                } else {
                    let (rx, ry) = self.sync_pointer()?;
                    let Some(confirmed) = self
                        .geometry
                        .hit_outer_edge_guarded(edge, rx, ry, 2, dead_corner_px)
                    else {
                        self.edge_push_accum = 0.0;
                        return Ok(None);
                    };
                    confirmed
                };
                self.edge_push_accum = 0.0;
                return Ok(Some((edge, confirmed_norm)));
            }
        } else {
            self.edge_push_accum = 0.0;
        }
        Ok(None)
    }

    fn scroll(&mut self, dx_120: i16, dy_120: i16) -> Result<()> {
        let mut evs = [(0u16, 0u16, 0i32); 4];
        let mut n = 0;
        if dy_120 != 0 {
            evs[n] = (EV_REL, REL_WHEEL_HI_RES, dy_120 as i32);
            n += 1;
            self.scroll_rem_y += dy_120 as i32;
            let ticks = self.scroll_rem_y / 120;
            if ticks != 0 {
                self.scroll_rem_y -= ticks * 120;
                evs[n] = (EV_REL, REL_WHEEL, ticks);
                n += 1;
            }
        }
        if dx_120 != 0 {
            evs[n] = (EV_REL, REL_HWHEEL_HI_RES, dx_120 as i32);
            n += 1;
            self.scroll_rem_x += dx_120 as i32;
            let ticks = self.scroll_rem_x / 120;
            if ticks != 0 {
                self.scroll_rem_x -= ticks * 120;
                evs[n] = (EV_REL, REL_HWHEEL, ticks);
                n += 1;
            }
        }
        if n > 0 {
            self.emit_events(&evs[..n])?;
        }
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        let code = mouse_btn_to_evdev(button);
        self.emit_events(&[(EV_KEY, code, i32::from(pressed))])?;
        if pressed {
            self.pressed_buttons.insert(code);
        } else {
            self.pressed_buttons.remove(&code);
        }
        Ok(())
    }

    fn key(&mut self, usage: HidUsage, pressed: bool) -> Result<()> {
        let Some(code) = usage.to_evdev() else {
            trace!(?usage, "Unmapped HID usage for uinput");
            return Ok(());
        };
        self.emit_events(&[(EV_KEY, code, i32::from(pressed))])?;
        if pressed {
            self.pressed_keys.insert(code);
        } else {
            self.pressed_keys.remove(&code);
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<u16> = self.pressed_keys.drain().collect();
        let btns: Vec<u16> = self.pressed_buttons.drain().collect();
        let mut evs = Vec::with_capacity(keys.len() + btns.len());
        for k in keys {
            evs.push((EV_KEY, k, 0));
        }
        for b in btns {
            evs.push((EV_KEY, b, 0));
        }
        if !evs.is_empty() {
            if let Err(e) = self.emit_events(&evs) {
                warn!(error = %e, "Failed releasing uinput keys/buttons");
            }
        }
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        self.refresh_geometry_if_needed();
        let (px, py) = self.geometry.primary_park_corner();
        if let Some(ref mut x11) = self.x11 {
            let _ = x11
                .conn
                .warp_pointer(x11rb::NONE, x11.root, 0, 0, 0, 0, px as i16, py as i16);
            x11.cursor_hider.hide(&x11.conn, x11.root);
            if self.unfocus_on_leave {
                x11.focus_clearer.clear_focus(&x11.conn, x11.root);
            }
            let _ = x11.conn.flush();
        } else {
            let _ = self.warp_to(px, py);
        }
        self.parked_pos = (px, py);
        self.est_x = px as f32;
        self.est_y = py as f32;
        self.edge_push_accum = 0.0;
        debug!(px, py, "UinputInjector left screen; cursor parked");
        Ok(())
    }

    fn set_unfocus_on_leave(&mut self, enabled: bool) {
        self.unfocus_on_leave = enabled;
    }

    fn show_cursor(&mut self) -> Result<()> {
        if let Some(ref mut x11) = self.x11 {
            x11.cursor_hider.show(&x11.conn, x11.root);
            let _ = x11.conn.flush();
        }
        Ok(())
    }

    fn is_cursor_hidden(&self) -> bool {
        self.x11
            .as_ref()
            .is_some_and(|x11| x11.cursor_hider.is_hidden())
    }

    fn reveal_if_moved_locally(&mut self) {
        if !self.is_cursor_hidden() {
            return;
        }
        if let Ok((x, y)) = self.sync_pointer() {
            let (px, py) = self.parked_pos;
            if (x - px).abs() > 4 || (y - py).abs() > 4 {
                if let Some(ref mut x11) = self.x11 {
                    x11.cursor_hider.show(&x11.conn, x11.root);
                    let _ = x11.conn.flush();
                }
                debug!(x, y, "Local pointer moved while hidden; cursor revealed");
            }
        }
    }
}

impl Drop for UinputInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
        let _ = self.show_cursor();
        if let Some(ref mut x11) = self.x11 {
            x11.focus_clearer.destroy(&x11.conn);
            if x11.randr_subscribed {
                unsubscribe_randr_events(&x11.conn, x11.root);
                let _ = x11.conn.flush();
            }
        }
        unsafe {
            let _ = libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uinput_setup_abi_size_matches_linux_kernel() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
    }
}
