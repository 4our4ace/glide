//! X11 Client input injector using XTest (`x11rb`) with zero-roundtrip hot path.

use crate::geometry::DesktopGeometry;
use crate::linux_x11::query_desktop_geometry;
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

        let geometry = query_desktop_geometry(&conn, screen_num)?;
        let (cx, cy) = geometry.primary_center();

        Ok(Self {
            conn,
            screen_num,
            root,
            geometry,
            rem_x: 0.0,
            rem_y: 0.0,
            scroll_rem_x: 0,
            scroll_rem_y: 0,
            est_x: cx as f32,
            est_y: cy as f32,
            last_pointer_sync: Instant::now(),
            edge_push_accum: 0.0,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
        })
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
        self.geometry = query_desktop_geometry(&self.conn, self.screen_num)?;
        Ok(self.geometry.clone())
    }

    fn enter(&mut self, edge: Edge, normalized_pos: f32) -> Result<()> {
        // Refresh RandR geometry on every screen entry so monitor hotplug works seamlessly.
        if let Ok(geom) = query_desktop_geometry(&self.conn, self.screen_num) {
            self.geometry = geom;
        }
        let (x, y) = self.geometry.point_on_edge(edge, normalized_pos, 4);
        self.conn
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x as i16, y as i16)
            .context("X11 warp_pointer on Enter failed")?;
        self.conn.flush().context("X11 flush failed")?;

        self.rem_x = 0.0;
        self.rem_y = 0.0;
        self.scroll_rem_x = 0;
        self.scroll_rem_y = 0;
        self.est_x = x as f32;
        self.est_y = y as f32;
        self.edge_push_accum = 0.0;
        self.last_pointer_sync = Instant::now();
        debug!(?edge, normalized_pos, x, y, "X11Injector entered screen");
        Ok(())
    }

    fn move_relative(
        &mut self,
        dx: f32,
        dy: f32,
        return_edge: Option<Edge>,
        edge_threshold_px: i32,
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

        // Only query the actual X11 pointer when close to the return edge or periodically
        // while moving toward it, keeping normal mouse movement completely roundtrip-free!
        let dist = self.geometry.distance_to_edge(
            edge,
            self.est_x.round() as i32,
            self.est_y.round() as i32,
        );

        if outward <= 0.0 {
            self.edge_push_accum = 0.0;
            return Ok(None);
        }

        let should_sync =
            dist <= 96 || self.last_pointer_sync.elapsed() >= Duration::from_millis(35);
        if !should_sync {
            return Ok(None);
        }

        let (rx, ry) = self.sync_pointer()?;
        if let Some(norm) = self.geometry.hit_outer_edge(edge, rx, ry, 2) {
            self.edge_push_accum += outward;
            if self.edge_push_accum >= edge_threshold_px.max(1) as f32 {
                self.edge_push_accum = 0.0;
                return Ok(Some((edge, norm)));
            }
        } else {
            self.edge_push_accum = 0.0;
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
}

impl Drop for X11Injector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}
