//! Windows 11 client input injector using `SendInput` (scancode keys + relative mouse + hi-res scroll),
//! cursor memory restoration, dead-corner guards, and CapsLock/NumLock sync.

use crate::geometry::DesktopGeometry;
use crate::windows::{init_dpi_awareness, query_desktop_geometry, GLIDE_EXTRA_INFO};
use crate::InputInjector;
use anyhow::Result;
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{Edge, LockState, OsKind};
use std::collections::HashSet;
use std::mem::size_of;
use tracing::{debug, trace};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VK_CAPITAL, VK_NUMLOCK,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos, XBUTTON1, XBUTTON2};

pub struct WindowsInjector {
    geometry: DesktopGeometry,
    rem_x: f32,
    rem_y: f32,
    edge_push_accum: f32,
    pressed_keys: HashSet<HidUsage>,
    pressed_buttons: HashSet<MouseButton>,
}

impl WindowsInjector {
    pub fn new() -> Result<Self> {
        init_dpi_awareness();
        let geometry = query_desktop_geometry()?;
        Ok(Self {
            geometry,
            rem_x: 0.0,
            rem_y: 0.0,
            edge_push_accum: 0.0,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
        })
    }

    fn send_mouse(&self, dx: i32, dy: i32, mouse_data: u32, flags: u32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: GLIDE_EXTRA_INFO,
                },
            },
        };
        unsafe {
            SendInput(1, &input, size_of::<INPUT>() as i32);
        }
    }

    fn send_key_scan(&self, scan: u16, extended: bool, pressed: bool) {
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: GLIDE_EXTRA_INFO,
                },
            },
        };
        unsafe {
            SendInput(1, &input, size_of::<INPUT>() as i32);
        }
    }
}

impl InputInjector for WindowsInjector {
    fn os_kind(&self) -> OsKind {
        OsKind::Windows
    }

    fn desktop_geometry(&mut self) -> Result<DesktopGeometry> {
        self.geometry = query_desktop_geometry()?;
        Ok(self.geometry.clone())
    }

    fn enter(&mut self, edge: Edge, normalized_pos: f32) -> Result<()> {
        if let Ok(geom) = query_desktop_geometry() {
            self.geometry = geom;
        }
        let (x, y) = self.geometry.point_on_edge(edge, normalized_pos, 4);
        unsafe {
            SetCursorPos(x, y);
        }
        self.rem_x = 0.0;
        self.rem_y = 0.0;
        self.edge_push_accum = 0.0;
        debug!(
            ?edge,
            normalized_pos, x, y, "WindowsInjector entered screen"
        );
        Ok(())
    }

    fn enter_at(&mut self, norm_x: f32, norm_y: f32) -> Result<()> {
        if let Ok(geom) = query_desktop_geometry() {
            self.geometry = geom;
        }
        let (x, y) = self.geometry.denormalize_point(norm_x, norm_y);
        unsafe {
            SetCursorPos(x, y);
        }
        self.rem_x = 0.0;
        self.rem_y = 0.0;
        self.edge_push_accum = 0.0;
        Ok(())
    }

    fn cursor_norm(&mut self) -> (f32, f32) {
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } != 0 {
            self.geometry.normalize_point(pt.x, pt.y)
        } else {
            (0.5, 0.5)
        }
    }

    fn sync_lock_state(&mut self, desired: LockState) -> Result<()> {
        let cur_caps = (unsafe { GetKeyState(VK_CAPITAL as i32) } & 1) != 0;
        let cur_num = (unsafe { GetKeyState(VK_NUMLOCK as i32) } & 1) != 0;
        if cur_caps != desired.caps_lock {
            self.send_key_scan(0x3A, false, true);
            self.send_key_scan(0x3A, false, false);
        }
        if cur_num != desired.num_lock {
            self.send_key_scan(0x45, true, true);
            self.send_key_scan(0x45, true, false);
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
            self.send_mouse(ix, iy, 0, MOUSEEVENTF_MOVE);
        }

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

        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } != 0 {
            if let Some(norm) =
                self.geometry
                    .hit_outer_edge_guarded(edge, pt.x, pt.y, 2, dead_corner_px)
            {
                self.edge_push_accum += outward;
                if self.edge_push_accum >= edge_threshold_px.max(1) as f32 {
                    self.edge_push_accum = 0.0;
                    return Ok(Some((edge, norm)));
                }
            } else {
                self.edge_push_accum = 0.0;
            }
        }

        Ok(None)
    }

    fn scroll(&mut self, dx_120: i16, dy_120: i16) -> Result<()> {
        if dy_120 != 0 {
            self.send_mouse(0, 0, (dy_120 as i32) as u32, MOUSEEVENTF_WHEEL);
        }
        if dx_120 != 0 {
            self.send_mouse(0, 0, (dx_120 as i32) as u32, MOUSEEVENTF_HWHEEL);
        }
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        let (flags, data) = match (button, pressed) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
        };
        self.send_mouse(0, 0, data, flags);
        if pressed {
            self.pressed_buttons.insert(button);
        } else {
            self.pressed_buttons.remove(&button);
        }
        Ok(())
    }

    fn key(&mut self, usage: HidUsage, pressed: bool) -> Result<()> {
        let Some(scan) = usage.to_win_scan() else {
            trace!(?usage, "Unmapped HID usage for Windows");
            return Ok(());
        };
        self.send_key_scan(scan.code, scan.extended, pressed);
        if pressed {
            self.pressed_keys.insert(usage);
        } else {
            self.pressed_keys.remove(&usage);
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<HidUsage> = self.pressed_keys.drain().collect();
        let btns: Vec<MouseButton> = self.pressed_buttons.drain().collect();
        for k in keys {
            if let Some(scan) = k.to_win_scan() {
                self.send_key_scan(scan.code, scan.extended, false);
            }
        }
        for b in btns {
            let _ = self.button(b, false);
        }
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        let (px, py) = self.geometry.primary_park_corner();
        unsafe {
            SetCursorPos(px, py);
        }
        self.edge_push_accum = 0.0;
        Ok(())
    }
}

impl Drop for WindowsInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}
