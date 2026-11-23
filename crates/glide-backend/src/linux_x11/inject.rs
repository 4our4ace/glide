//! X11 Client input injector using XTest (`x11rb`) with zero-roundtrip hot path.

use crate::geometry::DesktopGeometry;
use crate::linux_x11::{
    poll_randr_event, query_desktop_geometry, subscribe_randr_events, unsubscribe_randr_events,
    X11CursorHider, X11FocusClearer,
};
use crate::InputInjector;
use anyhow::{Context, Result};
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{Edge, OsKind};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt as XProtoExt, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::rust_connection::RustConnection;

pub struct X11Injector {
    conn: RustConnection,
    screen_num: usize,
    root: Window,
    geometry: DesktopGeometry,
    randr_subscribed: bool,
    last_geom_refresh: Instant,
    cursor_hider: X11CursorHider,
    focus_clearer: X11FocusClearer,
    unfocus_on_leave: bool,
    parked_pos: (i32, i32),
    // Sub-pixel motion remainder accumulator so fractional deltas never get dropped.
    rem_x: f32,
    rem_y: f32,
    // Fractional scroll accumulator (120 units = 1 notch).
    scroll_rem_x: i32,
    scroll_rem_y: i32,
    // Estimated cursor position to avoid X11 query_pointer roundtrips when away from edge.
    est_x: f32,
    est_y: f32,
    last_pointer_sync: Instant,
    edge_push_accum: f32,
    edge_push_start: Option<Instant>,
    // Track currently pressed keys & buttons so release_all() prevents stuck modifiers.
    pressed_keys: HashSet<u8>,
    pressed_buttons: HashSet<u8>,
}

impl X11Injector {
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).context("failed to connect to X11 display")?;
        let root = conn.setup().roots[screen_num].root;

        let xtest_ver = conn
            .xtest_get_version(2, 2)
            .context("failed to query XTest")?
            .reply()
            .context("XTest extension unavailable on this X server")?;
        debug!(
            major = xtest_ver.major_version,
            minor = xtest_ver.minor_version,
            "Connected X11Injector with XTest"
        );

        let randr_subscribed = subscribe_randr_events(&conn, root);
        let cursor_hider = X11CursorHider::init(&conn);
        let geometry = query_desktop_geometry(&conn, screen_num)?;
        let (cx, cy) = geometry.primary_center();
        let now = Instant::now();

        Ok(Self {
            conn,
            screen_num,
            root,
            geometry,
            randr_subscribed,
            last_geom_refresh: now,
            cursor_hider,
            focus_clearer: X11FocusClearer::new(),
            unfocus_on_leave: false,
            parked_pos: (cx, cy),
            rem_x: 0.0,
            rem_y: 0.0,
            scroll_rem_x: 0,
            scroll_rem_y: 0,
            est_x: cx as f32,
            est_y: cy as f32,
            last_pointer_sync: now,
            edge_push_accum: 0.0,
            edge_push_start: None,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
        })
    }

    fn refresh_geometry_if_needed(&mut self) -> bool {
        let randr_dirty = self.randr_subscribed && poll_randr_event(&self.conn);
        if self.randr_subscribed {
            if !randr_dirty {
                return false;
            }
        } else if self.last_geom_refresh.elapsed() < Duration::from_secs(15) {
            return false;
        }
        self.last_geom_refresh = Instant::now();
        if let Ok(new_geom) = query_desktop_geometry(&self.conn, self.screen_num) {
            if new_geom != self.geometry {
                self.geometry = new_geom;
                self.edge_push_accum = 0.0;
                let (min_x, min_y, max_x, max_y) = self.geometry.bounds();
                self.est_x = self.est_x.clamp(min_x as f32, (max_x - 1) as f32);
                self.est_y = self.est_y.clamp(min_y as f32, (max_y - 1) as f32);
                debug!(
                    monitors = self.geometry.monitors.len(),
                    randr_dirty, "X11Injector refreshed desktop geometry"
                );
                return true;
            }
        }
        false
    }

    fn sync_pointer(&mut self) -> Result<(i32, i32)> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .context("X11 query_pointer failed")?
            .reply()
            .context("X11 query_pointer reply failed")?;
        self.est_x = reply.root_x as f32;
        self.est_y = reply.root_y as f32;
        self.last_pointer_sync = Instant::now();
        Ok((reply.root_x as i32, reply.root_y as i32))
    }

    fn emit_x11_button(&self, button: u8, pressed: bool) -> Result<()> {
        let event_type = if pressed {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.conn
            .xtest_fake_input(
                event_type,
                button,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            )
            .context("XTest button fake_input failed")?;
        Ok(())
    }
}

impl InputInjector for X11Injector {
    fn os_kind(&self) -> OsKind {
        OsKind::LinuxX11
    }

    fn desktop_geometry(&mut self) -> Result<DesktopGeometry> {
        let _ = poll_randr_event(&self.conn);
        self.last_geom_refresh = Instant::now();
        self.geometry = query_desktop_geometry(&self.conn, self.screen_num)?;
        self.last_geom_refresh = Instant::now();
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
        let _ = self.refresh_geometry_if_needed();
        let (x, y) = self.geometry.point_on_edge(edge, normalized_pos, 4);
        self.conn
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x as i16, y as i16)
            .context("X11 warp_pointer on Enter failed")?;
        self.cursor_hider.show(&self.conn, self.root);
        if self.unfocus_on_leave {
            self.focus_clearer.restore_focus(&self.conn, self.root);
        } else {
            self.focus_clearer.discard(&self.conn);
        }
        self.conn.flush().context("X11 flush failed")?;

        self.rem_x = 0.0;
        self.rem_y = 0.0;
        self.scroll_rem_x = 0;
        self.scroll_rem_y = 0;
        self.est_x = x as f32;
        self.est_y = y as f32;
        self.edge_push_accum = 0.0;
        self.edge_push_start = None;
        self.last_pointer_sync = Instant::now();
        debug!(?edge, normalized_pos, x, y, "X11Injector entered screen");
        Ok(())
    }

    fn enter_at(&mut self, norm_x: f32, norm_y: f32) -> Result<()> {
        let _ = self.refresh_geometry_if_needed();
        let (x, y) = self.geometry.denormalize_point(norm_x, norm_y);
        self.conn
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x as i16, y as i16)
            .context("X11 warp_pointer on EnterAt failed")?;
        self.cursor_hider.show(&self.conn, self.root);
        if self.unfocus_on_leave {
            self.focus_clearer.restore_focus(&self.conn, self.root);
        } else {
            self.focus_clearer.discard(&self.conn);
        }
        self.conn.flush().context("X11 flush failed")?;

        self.rem_x = 0.0;
        self.rem_y = 0.0;
        self.est_x = x as f32;
        self.est_y = y as f32;
        self.edge_push_accum = 0.0;
        self.edge_push_start = None;
        self.last_pointer_sync = Instant::now();
        Ok(())
    }

    fn cursor_norm(&mut self) -> (f32, f32) {
        let (x, y) = self
            .sync_pointer()
            .unwrap_or((self.est_x.round() as i32, self.est_y.round() as i32));
        self.geometry.normalize_point(x, y)
    }

    fn sync_lock_state(&mut self, desired: glide_proto::LockState) -> Result<()> {
        let Ok(cookie) = self.conn.get_keyboard_control() else {
            return Ok(());
        };
        let Ok(reply) = cookie.reply() else {
            return Ok(());
        };
        let current_caps = (reply.led_mask & 1) != 0;
        let current_num = (reply.led_mask & 2) != 0;

        if current_caps != desired.caps_lock {
            let caps = HidUsage::keyboard(0x39);
            let _ = self.key(caps, true);
            let _ = self.key(caps, false);
        }
        if current_num != desired.num_lock {
            let num = HidUsage::keyboard(0x53);
            let _ = self.key(num, true);
            let _ = self.key(num, false);
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
        let ix = self.rem_x.trunc() as i16;
        let iy = self.rem_y.trunc() as i16;
        self.rem_x -= ix as f32;
        self.rem_y -= iy as f32;

        if ix != 0 || iy != 0 {
            // detail = 1 means RELATIVE motion in XTestFakeInput(MotionNotify)!
            self.conn
                .xtest_fake_input(
                    MOTION_NOTIFY_EVENT,
                    1,
                    x11rb::CURRENT_TIME,
                    x11rb::NONE,
                    ix,
                    iy,
                    0,
                )
                .context("XTest relative motion failed")?;
            self.conn.flush().context("X11 flush failed")?;
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
            self.edge_push_start = None;
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
            self.edge_push_start = None;
            return Ok(None);
        }

        let px = self.est_x.round() as i32;
        let py = self.est_y.round() as i32;
        if let Some(norm) = self
            .geometry
            .hit_outer_edge_guarded(edge, px, py, 2, dead_corner_px)
        {
            let lmb_held = self.pressed_buttons.contains(&MouseButton::Left.to_x11_button());
            let start = *self.edge_push_start.get_or_insert_with(Instant::now);
            self.edge_push_accum += outward;
            let drag_dwell_ok = !lmb_held || start.elapsed() >= Duration::from_millis(25);
            if self.edge_push_accum >= edge_threshold_px.max(1) as f32 && drag_dwell_ok {
                let confirmed_norm = if synced_this_call {
                    norm
                } else {
                    let (rx, ry) = self.sync_pointer()?;
                    let Some(confirmed) = self
                        .geometry
                        .hit_outer_edge_guarded(edge, rx, ry, 2, dead_corner_px)
                    else {
                        self.edge_push_accum = 0.0;
                        self.edge_push_start = None;
                        return Ok(None);
                    };
                    confirmed
                };
                self.edge_push_accum = 0.0;
                self.edge_push_start = None;
                return Ok(Some((edge, confirmed_norm)));
            }
        } else {
            self.edge_push_accum = 0.0;
            self.edge_push_start = None;
        }

        Ok(None)
    }

    fn scroll(&mut self, dx_120: i16, dy_120: i16) -> Result<()> {
        self.scroll_rem_x += dx_120 as i32;
        self.scroll_rem_y += dy_120 as i32;

        let mut emitted = false;
        while self.scroll_rem_y >= 120 {
            self.emit_x11_button(4, true)?;
            self.emit_x11_button(4, false)?;
            self.scroll_rem_y -= 120;
            emitted = true;
        }
        while self.scroll_rem_y <= -120 {
            self.emit_x11_button(5, true)?;
            self.emit_x11_button(5, false)?;
            self.scroll_rem_y += 120;
            emitted = true;
        }
        while self.scroll_rem_x <= -120 {
            self.emit_x11_button(6, true)?;
            self.emit_x11_button(6, false)?;
            self.scroll_rem_x += 120;
            emitted = true;
        }
        while self.scroll_rem_x >= 120 {
            self.emit_x11_button(7, true)?;
            self.emit_x11_button(7, false)?;
            self.scroll_rem_x -= 120;
            emitted = true;
        }

        if emitted {
            self.conn.flush().context("X11 scroll flush failed")?;
        }
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        let b = button.to_x11_button();
        self.emit_x11_button(b, pressed)?;
        self.conn.flush().context("X11 button flush failed")?;
        if pressed {
            self.pressed_buttons.insert(b);
        } else {
            self.pressed_buttons.remove(&b);
        }
        Ok(())
    }

    fn key(&mut self, usage: HidUsage, pressed: bool) -> Result<()> {
        let Some(kc) = usage.to_x11_keycode() else {
            trace!(?usage, "Unmapped HID usage for X11");
            return Ok(());
        };
        let event_type = if pressed {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.conn
            .xtest_fake_input(event_type, kc, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)
            .context("XTest key fake_input failed")?;
        self.conn.flush().context("X11 key flush failed")?;

        if pressed {
            self.pressed_keys.insert(kc);
        } else {
            self.pressed_keys.remove(&kc);
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<u8> = self.pressed_keys.drain().collect();
        let buttons: Vec<u8> = self.pressed_buttons.drain().collect();
        if keys.is_empty() && buttons.is_empty() {
            return Ok(());
        }
        for kc in keys {
            if let Err(e) = self.conn.xtest_fake_input(
                KEY_RELEASE_EVENT,
                kc,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            ) {
                warn!(kc, error = %e, "Failed releasing X11 key during release_all");
            }
        }
        for b in buttons {
            if let Err(e) = self.emit_x11_button(b, false) {
                warn!(b, error = %e, "Failed releasing X11 button during release_all");
            }
        }
        let _ = self.conn.flush();
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        self.refresh_geometry_if_needed();
        let (px, py) = self.geometry.primary_park_corner();
        let _ = self
            .conn
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, px as i16, py as i16);
        self.cursor_hider.hide(&self.conn, self.root);
        if self.unfocus_on_leave {
            self.focus_clearer.clear_focus(&self.conn, self.root);
        } else {
            self.focus_clearer.discard(&self.conn);
        }
        let _ = self.conn.flush();
        self.parked_pos = (px, py);
        self.est_x = px as f32;
        self.est_y = py as f32;
        self.edge_push_accum = 0.0;
        self.edge_push_start = None;
        debug!(px, py, "X11Injector left screen; cursor hidden & parked");
        Ok(())
    }

    fn set_unfocus_on_leave(&mut self, enabled: bool) {
        self.unfocus_on_leave = enabled;
        if !enabled {
            self.focus_clearer.discard(&self.conn);
        }
    }

    fn discard_saved_focus(&mut self) {
        self.focus_clearer.discard(&self.conn);
    }

    fn show_cursor(&mut self) -> Result<()> {
        self.focus_clearer.discard(&self.conn);
        self.cursor_hider.show(&self.conn, self.root);
        let _ = self.conn.flush();
        Ok(())
    }

    fn is_cursor_hidden(&self) -> bool {
        self.cursor_hider.is_hidden()
    }

    fn reveal_if_moved_locally(&mut self) {
        if !self.cursor_hider.is_hidden() {
            return;
        }
        if let Ok((x, y)) = self.sync_pointer() {
            let (px, py) = self.parked_pos;
            if (x - px).abs() > 4 || (y - py).abs() > 4 {
                self.focus_clearer.discard(&self.conn);
                self.cursor_hider.show(&self.conn, self.root);
                let _ = self.conn.flush();
                debug!(x, y, "Local pointer moved while hidden; cursor revealed");
            }
        }
    }
}

impl Drop for X11Injector {
    fn drop(&mut self) {
        let _ = self.release_all();
        let _ = self.show_cursor();
        self.focus_clearer.destroy(&self.conn);
        if self.randr_subscribed {
            unsubscribe_randr_events(&self.conn, self.root);
            let _ = self.conn.flush();
        }
    }
}

/// Returns `true` when `move_relative` should issue a rate-limited X11 `query_pointer`
/// calibration roundtrip during outward motion toward the return edge:
/// - Mid-screen (`> 48px`): low-frequency `150ms` calibration sync so nonlinear Xorg/libinput
///   pointer acceleration or multi-monitor gaps can never leave `est_x`/`est_y` permanently
///   drifted away from the real cursor at the return edge.
/// - Approaching return edge (`<= 48px`): `25ms`.
/// - Actively pushing into return edge (`edge_push_accum > 0`): `12ms`.
#[inline]
pub(crate) fn should_sync_x11_pointer(
    dist_px: i32,
    edge_push_accum: f32,
    elapsed: Duration,
) -> bool {
    if dist_px > 48 {
        return elapsed >= Duration::from_millis(150);
    }
    if edge_push_accum > 0.0 {
        elapsed >= Duration::from_millis(12)
    } else {
        elapsed >= Duration::from_millis(25)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_pointer_sync_never_fires_mid_screen_and_rate_limits_near_edge() {
        // Mid-screen (> 48px): low-frequency outward calibration sync at >= 150ms
        assert!(!should_sync_x11_pointer(49, 0.0, Duration::from_millis(149)));
        assert!(should_sync_x11_pointer(49, 0.0, Duration::from_millis(150)));
        assert!(!should_sync_x11_pointer(500, 0.0, Duration::from_millis(100)));
        assert!(should_sync_x11_pointer(500, 0.0, Duration::from_millis(150)));
        assert!(!should_sync_x11_pointer(60, 5.0, Duration::from_millis(149)));
        assert!(should_sync_x11_pointer(60, 5.0, Duration::from_millis(150)));

        // Approaching return edge (<= 48px): rate-limited to >= 25ms
        assert!(!should_sync_x11_pointer(48, 0.0, Duration::from_millis(24)));
        assert!(should_sync_x11_pointer(48, 0.0, Duration::from_millis(25)));
        assert!(!should_sync_x11_pointer(0, 0.0, Duration::from_millis(10)));
        assert!(should_sync_x11_pointer(0, 0.0, Duration::from_millis(25)));

        // Actively pushing into edge (edge_push_accum > 0): rate-limited to >= 12ms
        assert!(!should_sync_x11_pointer(0, 2.0, Duration::from_millis(11)));
        assert!(should_sync_x11_pointer(0, 2.0, Duration::from_millis(12)));
    }
}
