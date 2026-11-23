//! In-memory mock capture, injector, and clipboard backends for deterministic
//! end-to-end integration tests without requiring a physical display server.

use crate::geometry::DesktopGeometry;
use crate::{
    CaptureControl, CaptureEvent, CaptureHandle, CaptureHotkeys, ClipboardBackend, ClipboardHandle,
    InputInjector,
};
use anyhow::Result;
use glide_hid::{HidUsage, MouseButton};
use glide_proto::{ClipboardPayload, Edge, LockState, OsKind};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct MockCaptureState {
    pub active_edges: Mutex<Vec<Edge>>,
    pub geometry: Mutex<DesktopGeometry>,
    pub remote_mode: AtomicBool,
    pub locked: AtomicBool,
    pub allow_fullscreen_videos: AtomicBool,
    pub pin_media_keys_local: AtomicBool,
    pub unfocus_on_leave: AtomicBool,
    pub unfocus_count: AtomicUsize,
    pub has_saved_focus: AtomicBool,
    pub refocus_count: AtomicUsize,
    pub discard_focus_count: AtomicUsize,
    pub hotkeys: Mutex<CaptureHotkeys>,
    pub hotkeys_suspended: AtomicBool,
    pub enter_remote_calls: AtomicUsize,
    pub reset_remote_keys_calls: AtomicUsize,
    pub leave_remote_at_calls: AtomicUsize,
    pub last_leave: Mutex<Option<(Edge, f32)>>,
    pub last_leave_at: Mutex<Option<(f32, f32)>>,
    pub lock_state: Mutex<LockState>,
}

impl Default for MockCaptureState {
    fn default() -> Self {
        Self {
            active_edges: Mutex::new(Vec::new()),
            geometry: Mutex::new(DesktopGeometry::default()),
            remote_mode: AtomicBool::new(false),
            locked: AtomicBool::new(false),
            allow_fullscreen_videos: AtomicBool::new(true),
            pin_media_keys_local: AtomicBool::new(false),
            unfocus_on_leave: AtomicBool::new(false),
            unfocus_count: AtomicUsize::new(0),
            has_saved_focus: AtomicBool::new(false),
            refocus_count: AtomicUsize::new(0),
            discard_focus_count: AtomicUsize::new(0),
            hotkeys: Mutex::new(CaptureHotkeys::default()),
            hotkeys_suspended: AtomicBool::new(false),
            enter_remote_calls: AtomicUsize::new(0),
            reset_remote_keys_calls: AtomicUsize::new(0),
            leave_remote_at_calls: AtomicUsize::new(0),
            last_leave: Mutex::new(None),
            last_leave_at: Mutex::new(None),
            lock_state: Mutex::new(LockState::default()),
        }
    }
}

impl MockCaptureState {
    pub fn allow_fullscreen_videos(&self) -> bool {
        self.allow_fullscreen_videos.load(Ordering::SeqCst)
    }

    pub fn unfocus_count(&self) -> usize {
        self.unfocus_count.load(Ordering::SeqCst)
    }

    pub fn unfocus_on_leave(&self) -> bool {
        self.unfocus_on_leave.load(Ordering::SeqCst)
    }

    pub fn has_saved_focus(&self) -> bool {
        self.has_saved_focus.load(Ordering::SeqCst)
    }

    pub fn refocus_count(&self) -> usize {
        self.refocus_count.load(Ordering::SeqCst)
    }

    pub fn discard_focus_count(&self) -> usize {
        self.discard_focus_count.load(Ordering::SeqCst)
    }
}

pub struct MockCaptureControl {
    pub state: Arc<MockCaptureState>,
}

impl MockCaptureControl {
    pub fn allow_fullscreen_videos(&self) -> bool {
        self.state.allow_fullscreen_videos()
    }

    pub fn unfocus_count(&self) -> usize {
        self.state.unfocus_count()
    }

    pub fn unfocus_on_leave(&self) -> bool {
        self.state.unfocus_on_leave()
    }

    pub fn refocus_count(&self) -> usize {
        self.state.refocus_count()
    }

    pub fn discard_focus_count(&self) -> usize {
        self.state.discard_focus_count()
    }
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

    fn set_allow_fullscreen_videos(&self, enabled: bool) {
        self.state
            .allow_fullscreen_videos
            .store(enabled, Ordering::SeqCst);
    }

    fn set_pin_media_keys_local(&self, enabled: bool) {
        self.state
            .pin_media_keys_local
            .store(enabled, Ordering::SeqCst);
    }

    fn set_unfocus_on_leave(&self, enabled: bool) {
        self.state
            .unfocus_on_leave
            .store(enabled, Ordering::SeqCst);
        if !enabled {
            self.state.has_saved_focus.store(false, Ordering::SeqCst);
        }
    }

    fn discard_saved_focus(&self) {
        self.state
            .discard_focus_count
            .fetch_add(1, Ordering::SeqCst);
        self.state.has_saved_focus.store(false, Ordering::SeqCst);
    }

    fn set_hotkeys(&self, hotkeys: CaptureHotkeys) {
        *self.state.hotkeys.lock() = hotkeys;
    }

    fn set_hotkeys_suspended(&self, suspended: bool) {
        self.state
            .hotkeys_suspended
            .store(suspended, Ordering::SeqCst);
    }

    fn enter_remote(&self) -> Result<()> {
        self.state.enter_remote_calls.fetch_add(1, Ordering::SeqCst);
        self.state.remote_mode.store(true, Ordering::SeqCst);
        if self.state.unfocus_on_leave.load(Ordering::SeqCst) {
            self.state.unfocus_count.fetch_add(1, Ordering::SeqCst);
            self.state.has_saved_focus.store(true, Ordering::SeqCst);
        } else {
            self.state.has_saved_focus.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    fn reset_remote_keys(&self) {
        self.state
            .reset_remote_keys_calls
            .fetch_add(1, Ordering::SeqCst);
    }

    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()> {
        self.state.remote_mode.store(false, Ordering::SeqCst);
        *self.state.last_leave.lock() = Some((return_edge, normalized_pos));
        if self.state.has_saved_focus.swap(false, Ordering::SeqCst)
            && self.state.unfocus_on_leave.load(Ordering::SeqCst)
        {
            self.state.refocus_count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn leave_remote_at(&self, norm_x: f32, norm_y: f32) -> Result<()> {
        self.state
            .leave_remote_at_calls
            .fetch_add(1, Ordering::SeqCst);
        self.state.remote_mode.store(false, Ordering::SeqCst);
        *self.state.last_leave_at.lock() = Some((norm_x, norm_y));
        if self.state.has_saved_focus.swap(false, Ordering::SeqCst)
            && self.state.unfocus_on_leave.load(Ordering::SeqCst)
        {
            self.state.refocus_count.fetch_add(1, Ordering::SeqCst);
        }
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
    UnfocusWindow,
    RefocusWindow,
}

#[derive(Debug)]
pub struct MockInjector {
    pub geometry: DesktopGeometry,
    pub pending_geometry: Arc<Mutex<Option<DesktopGeometry>>>,
    pub suspended_flag: Arc<AtomicBool>,
    pub resumed_flag: Arc<AtomicBool>,
    pub unfocus_on_leave_flag: Arc<AtomicBool>,
    pub discard_focus_count: Arc<AtomicUsize>,
    pub unfocus_on_leave: bool,
    pub unfocus_calls: usize,
    pub has_saved_focus: bool,
    pub refocus_calls: usize,
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
            unfocus_on_leave_flag: Arc::new(AtomicBool::new(false)),
            discard_focus_count: Arc::new(AtomicUsize::new(0)),
            unfocus_on_leave: false,
            unfocus_calls: 0,
            has_saved_focus: false,
            refocus_calls: 0,
            cursor_x: cx as f32,
            cursor_y: cy as f32,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            log: Arc::clone(&log),
            edge_push_accum: 0.0,
        };
        (me, log)
    }

    pub fn unfocus_on_leave(&self) -> bool {
        self.unfocus_on_leave
    }

    pub fn unfocus_calls(&self) -> usize {
        self.unfocus_calls
    }

    pub fn refocus_calls(&self) -> usize {
        self.refocus_calls
    }
}

impl InputInjector for MockInjector {
    fn os_kind(&self) -> OsKind {
        OsKind::LinuxX11
    }

    fn set_unfocus_on_leave(&mut self, enabled: bool) {
        self.unfocus_on_leave = enabled;
        self.unfocus_on_leave_flag.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.has_saved_focus = false;
        }
    }

    fn discard_saved_focus(&mut self) {
        self.discard_focus_count.fetch_add(1, Ordering::SeqCst);
        self.has_saved_focus = false;
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
            self.has_saved_focus = false;
            let _ = self.release_all();
            true
        } else {
            false
        }
    }

    fn poll_system_resumed(&mut self) -> bool {
        if self.resumed_flag.swap(false, Ordering::SeqCst) {
            self.has_saved_focus = false;
            true
        } else {
            false
        }
    }

    fn enter(&mut self, edge: Edge, normalized_pos: f32) -> Result<()> {
        let (x, y) = self.geometry.point_on_edge(edge, normalized_pos, 4);
        self.cursor_x = x as f32;
        self.cursor_y = y as f32;
        self.edge_push_accum = 0.0;
        let mut log = self.log.lock();
        log.push(InjectedAction::Enter(edge, normalized_pos));
        if std::mem::take(&mut self.has_saved_focus) && self.unfocus_on_leave {
            self.refocus_calls += 1;
            log.push(InjectedAction::RefocusWindow);
        }
        Ok(())
    }

    fn enter_at(&mut self, norm_x: f32, norm_y: f32) -> Result<()> {
        let (x, y) = self.geometry.denormalize_point(norm_x, norm_y);
        self.cursor_x = x as f32;
        self.cursor_y = y as f32;
        self.edge_push_accum = 0.0;
        let mut log = self.log.lock();
        log.push(InjectedAction::EnterAt(norm_x, norm_y));
        if std::mem::take(&mut self.has_saved_focus) && self.unfocus_on_leave {
            self.refocus_calls += 1;
            log.push(InjectedAction::RefocusWindow);
        }
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

    fn leave(&mut self) -> Result<()> {
        self.edge_push_accum = 0.0;
        if self.unfocus_on_leave {
            self.unfocus_calls += 1;
            self.has_saved_focus = true;
            self.log.lock().push(InjectedAction::UnfocusWindow);
        } else {
            self.has_saved_focus = false;
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        self.has_saved_focus = false;
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

#[derive(Debug, Default)]
pub struct MockDndState {
    pub enabled: AtomicBool,
    pub armed: AtomicBool,
    pub active_edges: Mutex<Vec<Edge>>,
    pub staged: Mutex<Option<(Edge, Vec<std::path::PathBuf>)>>,
    pub cancelled_local_drags: std::sync::atomic::AtomicUsize,
    pub incoming_cancels: std::sync::atomic::AtomicUsize,
    pub delivered_drops: Mutex<Vec<Vec<std::path::PathBuf>>>,
}

impl MockDndState {
    pub fn stage_files(&self, edge: Edge, paths: Vec<std::path::PathBuf>) {
        *self.staged.lock() = Some((edge, paths));
    }
}

pub struct MockDndBackend {
    pub state: Arc<MockDndState>,
}

impl crate::dnd::DndBackend for MockDndBackend {
    fn set_enabled(&self, enabled: bool) {
        self.state.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.state.armed.store(false, Ordering::SeqCst);
            *self.state.staged.lock() = None;
        }
    }

    fn set_active_edges(&self, edges: &[Edge], _dead_corner_px: i32) {
        *self.state.active_edges.lock() = edges.to_vec();
    }

    fn set_drag_armed(&self, armed: bool) {
        let effective = armed && self.state.enabled.load(Ordering::Acquire);
        self.state.armed.store(effective, Ordering::SeqCst);
    }

    fn staged_edge_mask(&self) -> u8 {
        if !self.state.enabled.load(Ordering::Acquire) {
            return 0;
        }
        self.state
            .staged
            .lock()
            .as_ref()
            .map_or(0, |(e, p)| if p.is_empty() { 0 } else { crate::edge_bit(*e) })
    }

    fn take_staged_files(&self, edge: Edge) -> Option<Vec<std::path::PathBuf>> {
        if !self.state.enabled.load(Ordering::Acquire) {
            return None;
        }
        let mut guard = self.state.staged.lock();
        if guard.as_ref().is_some_and(|(e, _)| *e == edge) {
            guard.take().map(|(_, p)| p)
        } else {
            None
        }
    }

    fn cancel_local_drag(&self) {
        self.state.armed.store(false, Ordering::SeqCst);
        *self.state.staged.lock() = None;
        self.state
            .cancelled_local_drags
            .fetch_add(1, Ordering::SeqCst);
    }

    fn on_incoming_drag_cancel(&self) {
        self.state.incoming_cancels.fetch_add(1, Ordering::SeqCst);
    }

    fn deliver_drop(
        &self,
        paths: Vec<std::path::PathBuf>,
    ) -> Result<crate::dnd::DropDeliveryOutcome> {
        self.state.delivered_drops.lock().push(paths);
        Ok(crate::dnd::DropDeliveryOutcome::DroppedIntoTarget)
    }
}

pub fn create_mock_dnd() -> (Arc<dyn crate::dnd::DndBackend>, Arc<MockDndState>) {
    let state = Arc::new(MockDndState {
        enabled: AtomicBool::new(true),
        ..Default::default()
    });
    let backend: Arc<dyn crate::dnd::DndBackend> = Arc::new(MockDndBackend {
        state: Arc::clone(&state),
    });
    (backend, state)
}
