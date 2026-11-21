//! In-memory mock capture, injector, and clipboard backends for deterministic
//! end-to-end integration tests without requiring a physical display server.

use crate::geometry::DesktopGeometry;
use crate::{
    CaptureControl, CaptureEvent, CaptureHandle, ClipboardBackend, ClipboardHandle, InputInjector,
};
use anyhow::Result;
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{ClipboardPayload, Edge, LockState, OsKind};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Default)]
pub struct MockCaptureState {
    pub active_edges: Mutex<Vec<Edge>>,
    pub geometry: Mutex<DesktopGeometry>,
    pub remote_mode: AtomicBool,
    pub locked: AtomicBool,
    pub last_leave: Mutex<Option<(Edge, f32)>>,
    pub last_leave_at: Mutex<Option<(f32, f32)>>,
    pub lock_state: Mutex<LockState>,
}

pub struct MockCaptureControl {
    pub state: Arc<MockCaptureState>,
}

impl CaptureControl for MockCaptureControl {
    fn set_active_edges(&self, edges: &[Edge]) {
        *self.state.active_edges.lock() = edges.to_vec();
    }

    fn update_tuning(
        &self,
        _edge_threshold_px: i32,
        _dead_corner_px: i32,
        _edge_dwell_ms: u64,
        _auto_lock_fullscreen: bool,
        locked: bool,
    ) {
        self.state.locked.store(locked, Ordering::SeqCst);
    }

    fn enter_remote(&self) -> Result<()> {
        self.state.remote_mode.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()> {
        self.state.remote_mode.store(false, Ordering::SeqCst);
        *self.state.last_leave.lock() = Some((return_edge, normalized_pos));
        Ok(())
    }

    fn leave_remote_at(&self, norm_x: f32, norm_y: f32) -> Result<()> {
        self.state.remote_mode.store(false, Ordering::SeqCst);
        *self.state.last_leave_at.lock() = Some((norm_x, norm_y));
        Ok(())
    }

    fn desktop_geometry(&self) -> DesktopGeometry {
        self.state.geometry.lock().clone()
    }

    fn lock_state(&self) -> LockState {
        *self.state.lock_state.lock()
    }

    fn last_local_cursor_norm(&self) -> (f32, f32) {
        (0.5, 0.5)
    }
}

pub fn create_mock_capture(
    geometry: DesktopGeometry,
) -> (
    CaptureHandle,
    mpsc::UnboundedSender<CaptureEvent>,
    Arc<MockCaptureState>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let state = Arc::new(MockCaptureState {
        geometry: Mutex::new(geometry),
        ..Default::default()
    });
    let control = MockCaptureControl {
        state: Arc::clone(&state),
    };
    (
        CaptureHandle {
            control: Box::new(control),
            events: rx,
        },
        tx,
        state,
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum InjectedAction {
    Enter(Edge, f32),
    EnterAt(f32, f32),
    LockSync(LockState),
    Move(f32, f32),
    Scroll(i16, i16),
    Button(MouseButton, bool),
    Key(HidUsage, bool),
    ReleaseAll,
}

#[derive(Debug)]
pub struct MockInjector {
    pub geometry: DesktopGeometry,
    pub pending_geometry: Arc<Mutex<Option<DesktopGeometry>>>,
    pub suspended_flag: Arc<AtomicBool>,
    pub resumed_flag: Arc<AtomicBool>,
    pub cursor_x: f32,
    pub cursor_y: f32,
    pub pressed_keys: HashSet<HidUsage>,
    pub pressed_buttons: HashSet<MouseButton>,
    pub log: Arc<Mutex<Vec<InjectedAction>>>,
    edge_push_accum: f32,
}

impl MockInjector {
    pub fn new(geometry: DesktopGeometry) -> (Self, Arc<Mutex<Vec<InjectedAction>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (cx, cy) = geometry.primary_center();
        let me = Self {
            geometry,
            pending_geometry: Arc::new(Mutex::new(None)),
            suspended_flag: Arc::new(AtomicBool::new(false)),
            resumed_flag: Arc::new(AtomicBool::new(false)),
            cursor_x: cx as f32,
            cursor_y: cy as f32,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            log: Arc::clone(&log),
            edge_push_accum: 0.0,
        };
        (me, log)
    }
}

impl InputInjector for MockInjector {
    fn os_kind(&self) -> OsKind {
        OsKind::LinuxX11
    }

    fn desktop_geometry(&mut self) -> Result<DesktopGeometry> {
        if let Some(new_geom) = self.pending_geometry.lock().take() {
            self.geometry = new_geom;
        }
        Ok(self.geometry.clone())
    }

    fn poll_geometry_changed(&mut self) -> Option<DesktopGeometry> {
        let new_geom = self.pending_geometry.lock().take()?;
        if new_geom != self.geometry {
            self.geometry = new_geom.clone();
            self.edge_push_accum = 0.0;
            return Some(new_geom);
        }
        None
    }

    fn poll_system_suspended(&mut self) -> bool {
        if self.suspended_flag.swap(false, Ordering::SeqCst) {
            let _ = self.release_all();
            true
        } else {
            false
        }
    }

    fn poll_system_resumed(&mut self) -> bool {
        self.resumed_flag.swap(false, Ordering::SeqCst)
    }

    fn enter(&mut self, edge: Edge, normalized_pos: f32) -> Result<()> {
        let (x, y) = self.geometry.point_on_edge(edge, normalized_pos, 4);
        self.cursor_x = x as f32;
        self.cursor_y = y as f32;
        self.edge_push_accum = 0.0;
        self.log
            .lock()
            .push(InjectedAction::Enter(edge, normalized_pos));
        Ok(())
    }

    fn enter_at(&mut self, norm_x: f32, norm_y: f32) -> Result<()> {
        let (x, y) = self.geometry.denormalize_point(norm_x, norm_y);
        self.cursor_x = x as f32;
        self.cursor_y = y as f32;
        self.edge_push_accum = 0.0;
        self.log
            .lock()
            .push(InjectedAction::EnterAt(norm_x, norm_y));
        Ok(())
    }

    fn cursor_norm(&mut self) -> (f32, f32) {
        self.geometry
            .normalize_point(self.cursor_x.round() as i32, self.cursor_y.round() as i32)
    }

    fn sync_lock_state(&mut self, desired: LockState) -> Result<()> {
        self.log.lock().push(InjectedAction::LockSync(desired));
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
        self.log.lock().push(InjectedAction::Move(dx, dy));
        let (min_x, min_y, max_x, max_y) = self.geometry.bounds();
        let raw_x = self.cursor_x + dx;
        let raw_y = self.cursor_y + dy;
        self.cursor_x = raw_x.clamp(min_x as f32, (max_x - 1) as f32);
        self.cursor_y = raw_y.clamp(min_y as f32, (max_y - 1) as f32);

        if let Some(edge) = return_edge {
            let overshoot = match edge {
                Edge::Left => (min_x as f32 - raw_x).max(0.0),
                Edge::Right => (raw_x - (max_x - 1) as f32).max(0.0),
                Edge::Top => (min_y as f32 - raw_y).max(0.0),
                Edge::Bottom => (raw_y - (max_y - 1) as f32).max(0.0),
            };
            if overshoot > 0.0 {
                self.edge_push_accum += overshoot;
                if self.edge_push_accum >= edge_threshold_px.max(1) as f32 {
                    self.edge_push_accum = 0.0;
                    if let Some(norm) = self.geometry.hit_outer_edge_guarded(
                        edge,
                        self.cursor_x.round() as i32,
                        self.cursor_y.round() as i32,
                        2,
                        dead_corner_px,
                    ) {
                        return Ok(Some((edge, norm)));
                    }
                }
            } else {
                self.edge_push_accum = 0.0;
            }
        }
        Ok(None)
    }

    fn scroll(&mut self, dx_120: i16, dy_120: i16) -> Result<()> {
        self.log.lock().push(InjectedAction::Scroll(dx_120, dy_120));
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        if pressed {
            self.pressed_buttons.insert(button);
        } else {
            self.pressed_buttons.remove(&button);
        }
        self.log
            .lock()
            .push(InjectedAction::Button(button, pressed));
        Ok(())
    }

    fn key(&mut self, usage: HidUsage, pressed: bool) -> Result<()> {
        if pressed {
            self.pressed_keys.insert(usage);
        } else {
            self.pressed_keys.remove(&usage);
        }
        self.log.lock().push(InjectedAction::Key(usage, pressed));
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<HidUsage> = self.pressed_keys.drain().collect();
        let btns: Vec<MouseButton> = self.pressed_buttons.drain().collect();
        let mut log = self.log.lock();
        for k in keys {
            log.push(InjectedAction::Key(k, false));
        }
        for b in btns {
            log.push(InjectedAction::Button(b, false));
        }
        log.push(InjectedAction::ReleaseAll);
        Ok(())
    }
}

pub struct MockClipboard {
    pub received: Arc<Mutex<Vec<ClipboardPayload>>>,
}

impl ClipboardBackend for MockClipboard {
    fn set(&self, payload: ClipboardPayload) -> Result<()> {
        self.received.lock().push(payload);
        Ok(())
    }
}

pub fn create_mock_clipboard() -> (
    ClipboardHandle,
    mpsc::UnboundedSender<ClipboardPayload>,
    Arc<Mutex<Vec<ClipboardPayload>>>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let received = Arc::new(Mutex::new(Vec::new()));
    let backend = MockClipboard {
        received: Arc::clone(&received),
    };
    (
        ClipboardHandle {
            backend: Box::new(backend),
            local_changes: rx,
        },
        tx,
        received,
    )
}
