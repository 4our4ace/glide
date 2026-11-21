//! Windows 11 server input capture using Raw Input (`WM_INPUT`) for true unaccelerated
//! mouse deltas + high-priority `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks with:
//! - German `AltGr` fake-LCtrl (`0x21D`) filtering
//! - Invisible bottom-right corner cursor parking in remote mode (with guaranteed restore)
//! - Dead-corner guard, edge dwell, fullscreen app auto-lock, and manual cursor lock
//! - Direct keyboard screen-switching (`Ctrl+Alt+Arrow`) and lock toggle (`Ctrl+Alt+L`)
//! - CapsLock / NumLock state synchronization

use crate::dnd::DndBackend;
use crate::geometry::DesktopGeometry;
use crate::windows::{init_dpi_awareness, query_desktop_geometry, GLIDE_EXTRA_INFO};
use crate::{
    edge_bit, is_windows_consumer_or_media_vk, is_windows_workstation_lock_chord, AtomicEdgeMask,
    AtomicNormPoint, CaptureConfig, CaptureControl, CaptureEvent, CaptureHandle,
    RemoteMediaKeyTracker, WindowsPendingMeta, ALL_EDGES,
};
use anyhow::{anyhow, Context, Result};
use glide_hid::{is_windows_fake_altgr_lctrl, HidUsage, ModifierMask, MouseButton, WinScan};
use glide_proto::{Edge, LockState};
use parking_lot::{Mutex, RwLock};
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, VK_CAPITAL, VK_LBUTTON,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, GetRegisteredRawInputDevices, RegisterRawInputDevices, HRAWINPUT,
    MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT,
    RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowRect, PostMessageW,
    RegisterClassW, SetCursorPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PBT_APMRESUMEAUTOMATIC,
    PBT_APMRESUMECRITICAL, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_APP, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_POWERBROADCAST, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_WTSSESSION_CHANGE, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_OVERLAPPED, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK, XBUTTON1, XBUTTON2,
};

const WM_APP_ENTER_REMOTE: u32 = WM_APP + 1;
const WM_APP_LEAVE_REMOTE: u32 = WM_APP + 2;
const WM_APP_ENSURE_RAW_INPUT: u32 = WM_APP + 3;

const VK_PAUSE_CODE: u32 = 0x13;
const VK_SNAPSHOT_CODE: u32 = 0x2C;
const VK_NUMLOCK_CODE: u32 = 0x90;
const VK_RSHIFT_CODE: u32 = 0xA1;

enum LeaveTarget {
    Edge(Edge, f32),
    Point(f32, f32),
}

struct HookState {
    config: CaptureConfig,
    tx: mpsc::UnboundedSender<CaptureEvent>,
    capture_hwnd: AtomicIsize,
    active_edges: AtomicEdgeMask,
    geometry: RwLock<DesktopGeometry>,
    remote_mode: AtomicBool,
    cursor_parked: AtomicBool,
    locked: AtomicBool,
    auto_lock_fullscreen: AtomicBool,
    sync_lock_keys: AtomicBool,
    drag_drop_files: AtomicBool,
    dnd_probe: OnceLock<Arc<dyn DndBackend>>,
    lmb_down: AtomicBool,
    lmb_started_on_edge: AtomicBool,
    drag_strip_armed: AtomicBool,
    pin_media_keys_local: AtomicBool,
    forwarded_media_keys: Mutex<RemoteMediaKeyTracker>,
    pending_meta: WindowsPendingMeta,
    edge_threshold_px: AtomicI32,
    dead_corner_px: AtomicI32,
    edge_dwell_ms: AtomicU64,
    anchor_x: AtomicI32,
    anchor_y: AtomicI32,
    last_local_norm: AtomicNormPoint,
    pending_leave: Mutex<Option<LeaveTarget>>,
    edge_push_active: AtomicBool,
    edge_push: Mutex<(Option<Edge>, f32, Option<Instant>)>,
    mods: AtomicU8,
    start_instant: Instant,
    last_raw_mouse_ms: AtomicU64,
    last_raw_reclaim_ms: AtomicU64,
    ll_moves_without_raw: AtomicU32,
    last_geom_refresh_ms: AtomicU64,
    last_resume_emit: Mutex<Option<Instant>>,
}

impl HookState {
    fn elapsed_ms(&self) -> u64 {
        self.start_instant.elapsed().as_millis() as u64
    }

    fn flush_pending_meta(&self) {
        for usage in self.pending_meta.take_pending().into_iter().flatten() {
            let _ = self.tx.send(CaptureEvent::Key {
                usage,
                pressed: true,
            });
        }
    }

    fn refresh_geometry_now(&self) -> bool {
        self.last_geom_refresh_ms
            .store(self.elapsed_ms(), Ordering::Relaxed);
        let Ok(new_geom) = query_desktop_geometry() else {
            return false;
        };
        let changed = {
            let mut geom = self.geometry.write();
            if *geom != new_geom {
                *geom = new_geom.clone();
                true
            } else {
                false
            }
        };
        if changed {
            *self.edge_push.lock() = (None, 0.0, None);
            self.edge_push_active.store(false, Ordering::Release);
            if self.remote_mode.load(Ordering::Acquire)
                && self.cursor_parked.load(Ordering::Acquire)
            {
                let (px, py) = new_geom.primary_park_corner();
                self.park_cursor_at(px, py);
            }
            debug!(
                monitors = new_geom.monitors.len(),
                primary_scale = new_geom.primary_scale(),
                "Refreshed Windows desktop geometry"
            );
            let _ = self.tx.send(CaptureEvent::GeometryChanged(new_geom));
        }
        changed
    }

    fn maybe_refresh_geometry(&self) {
        let now = self.elapsed_ms();
        let prev = self.last_geom_refresh_ms.load(Ordering::Relaxed);
        if now.saturating_sub(prev) >= 5000
            && self
                .last_geom_refresh_ms
                .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let _ = self.refresh_geometry_now();
        }
    }

    fn restore_cursor_visibly(&self) {
        let (nx, ny) = self.last_local_norm.load();
        let (x, y) = self.geometry.read().denormalize_point(nx, ny);
        unsafe {
            SetCursorPos(x, y);
        }
    }

    /// Unconditionally exits remote mode, restores the cursor onto the visible desktop,
    /// clears stale modifier/edge-push state, and notifies `glide-core` so remote clients
    /// immediately release any held keys — guaranteeing the user is never locked out of
    /// their Windows lock screen or wake prompt.
    fn release_to_local_on_suspend(&self, reason: &str) {
        self.pending_meta.clear();
        self.cursor_parked.store(false, Ordering::Release);
        let was_remote = self.remote_mode.swap(false, Ordering::SeqCst);
        self.mods.store(0, Ordering::Relaxed);
        self.forwarded_media_keys.lock().clear();
        self.ll_moves_without_raw.store(0, Ordering::Relaxed);
        *self.edge_push.lock() = (None, 0.0, None);
        self.edge_push_active.store(false, Ordering::Release);
        *self.pending_leave.lock() = None;
        if was_remote {
            self.restore_cursor_visibly();
        }
        debug!(
            reason,
            was_remote, "Windows capture released to local mode on suspend/session lock"
        );
        let _ = self.tx.send(CaptureEvent::SystemSuspended);
    }

    /// Ensures clean local state after wake or session unlock, immediately refreshes display
    /// geometry (monitors often change while asleep), and emits a debounced `SystemResumed`.
    fn handle_resume_or_unlock(&self, reason: &str) {
        self.pending_meta.clear();
        self.cursor_parked.store(false, Ordering::Release);
        if self.remote_mode.swap(false, Ordering::SeqCst) {
            self.restore_cursor_visibly();
        }
        self.mods.store(0, Ordering::Relaxed);
        self.forwarded_media_keys.lock().clear();
        self.ll_moves_without_raw.store(0, Ordering::Relaxed);
        *self.edge_push.lock() = (None, 0.0, None);
        self.edge_push_active.store(false, Ordering::Release);
        *self.pending_leave.lock() = None;

        let _ = self.refresh_geometry_now();

        let should_emit = {
            let mut last = self.last_resume_emit.lock();
            let emit = last.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
            if emit {
                *last = Some(Instant::now());
            }
            emit
        };
        if should_emit {
            debug!(reason, "Windows capture detected system resume/unlock");
            let _ = self.tx.send(CaptureEvent::SystemResumed);
        }
    }

    fn park_cursor_at(&self, px: i32, py: i32) {
        self.anchor_x.store(px, Ordering::Relaxed);
        self.anchor_y.store(py, Ordering::Relaxed);
        unsafe {
            SetCursorPos(px, py);
        }
    }

    fn clear_edge_push(&self) {
        if self.edge_push_active.swap(false, Ordering::Relaxed) {
            *self.edge_push.lock() = (None, 0.0, None);
        }
    }
}

static GLOBAL_HOOK_STATE: OnceLock<RwLock<Option<Arc<HookState>>>> = OnceLock::new();

fn with_hook_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&HookState) -> R,
{
    let cell = GLOBAL_HOOK_STATE.get()?;
    let guard = cell.read();
    guard.as_deref().map(f)
}

struct WindowsCaptureControl {
    hwnd: AtomicIsize,
    state: Arc<HookState>,
}

impl Drop for WindowsCaptureControl {
    fn drop(&mut self) {
        self.state.pending_meta.clear();
        self.state.cursor_parked.store(false, Ordering::Release);
        // Always restore cursor & stop hook loop cleanly on shutdown
        if self.state.remote_mode.swap(false, Ordering::SeqCst) {
            let (nx, ny) = self.state.last_local_norm.load();
            let (x, y) = self.state.geometry.read().denormalize_point(nx, ny);
            unsafe {
                SetCursorPos(x, y);
            }
        }
        let hwnd = self.hwnd.swap(0, Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_QUIT, 0, 0);
            }
        }
        if let Some(cell) = GLOBAL_HOOK_STATE.get() {
            *cell.write() = None;
        }
    }
}

impl CaptureControl for WindowsCaptureControl {
    fn set_active_edges(&self, edges: &[Edge]) {
        self.state.active_edges.set(edges);
        if !edges.is_empty() {
            let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
            if !hwnd.is_null() {
                unsafe {
                    PostMessageW(hwnd, WM_APP_ENSURE_RAW_INPUT, 0, 0);
                }
            }
        }
    }

    fn update_tuning(
        &self,
        edge_threshold_px: i32,
        dead_corner_px: i32,
        edge_dwell_ms: u64,
        auto_lock_fullscreen: bool,
        locked: bool,
    ) {
        self.state
            .edge_threshold_px
            .store(edge_threshold_px.max(1), Ordering::Relaxed);
        self.state
            .dead_corner_px
            .store(dead_corner_px.max(0), Ordering::Relaxed);
        self.state
            .edge_dwell_ms
            .store(edge_dwell_ms, Ordering::Relaxed);
        self.state
            .auto_lock_fullscreen
            .store(auto_lock_fullscreen, Ordering::Relaxed);
        self.state.locked.store(locked, Ordering::Relaxed);
    }

    fn set_sync_lock_keys(&self, enabled: bool) {
        self.state.sync_lock_keys.store(enabled, Ordering::Relaxed);
    }

    fn set_drag_drop_files(&self, enabled: bool) {
        self.state.drag_drop_files.store(enabled, Ordering::Relaxed);
        if !enabled && self.state.drag_strip_armed.swap(false, Ordering::SeqCst) {
            let _ = self.state.tx.send(CaptureEvent::DragStripArmed(false));
        }
    }

    fn set_dnd_probe(&self, dnd: Arc<dyn DndBackend>) {
        let _ = self.state.dnd_probe.set(dnd);
    }

    fn set_pin_media_keys_local(&self, enabled: bool) {
        self.state
            .pin_media_keys_local
            .store(enabled, Ordering::Relaxed);
    }

    fn enter_remote(&self) -> Result<()> {
        self.state.pending_meta.clear();
        // Only post WM_APP_ENTER_REMOTE when transitioning local -> remote (e.g. hotkey or UI
        // jump). When edge crossing already entered remote mode and parked the cursor, posting
        // WM_APP_ENTER_REMOTE would overwrite last_local_norm with the parked position.
        // For file drag-out crossings, cursor parking was intentionally deferred until after
        // `cancel_local_drag()` cancelled Explorer's modal DoDragDrop at the edge strip.
        let was_remote = self.state.remote_mode.swap(true, Ordering::SeqCst);
        if !was_remote {
            self.state.cursor_parked.store(true, Ordering::Release);
            let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
            if !hwnd.is_null() {
                unsafe {
                    PostMessageW(hwnd, WM_APP_ENTER_REMOTE, 0, 0);
                }
            }
        } else if !self.state.cursor_parked.swap(true, Ordering::AcqRel) {
            let (px, py) = self.state.geometry.read().primary_park_corner();
            self.state.park_cursor_at(px, py);
        }
        Ok(())
    }

    fn leave_remote(&self, return_edge: Edge, normalized_pos: f32) -> Result<()> {
        let (x, y, nx, ny) = {
            let geom = self.state.geometry.read();
            let (x, y) = geom.point_on_edge(return_edge, normalized_pos, 6);
            let (nx, ny) = geom.normalize_point(x, y);
            (x, y, nx, ny)
        };
        self.state.last_local_norm.store(nx, ny);
        self.state.pending_meta.clear();
        self.state.cursor_parked.store(false, Ordering::Release);
        self.state.clear_edge_push();
        // Move the cursor synchronously BEFORE flipping remote_mode=false / posting
        // WM_APP_LEAVE_REMOTE so incoming drag-and-drop (`GetCursorPos()`) immediately sees
        // the real return-edge coordinates instead of the parked corner, and WM_APP_LEAVE_REMOTE
        // won't yank the cursor back if the user already moved inward.
        let moved = unsafe { SetCursorPos(x, y) } != 0;
        *self.state.pending_leave.lock() = if moved {
            None
        } else {
            Some(LeaveTarget::Edge(return_edge, normalized_pos))
        };
        self.state.remote_mode.store(false, Ordering::SeqCst);
        self.state.forwarded_media_keys.lock().clear();
        let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_LEAVE_REMOTE, 0, 0);
            }
        }
        Ok(())
    }

    fn leave_remote_at(&self, norm_x: f32, norm_y: f32) -> Result<()> {
        let (x, y) = self
            .state
            .geometry
            .read()
            .denormalize_point(norm_x, norm_y);
        self.state.last_local_norm.store(norm_x, norm_y);
        self.state.pending_meta.clear();
        self.state.cursor_parked.store(false, Ordering::Release);
        self.state.clear_edge_push();
        let moved = unsafe { SetCursorPos(x, y) } != 0;
        *self.state.pending_leave.lock() = if moved {
            None
        } else {
            Some(LeaveTarget::Point(norm_x, norm_y))
        };
        self.state.remote_mode.store(false, Ordering::SeqCst);
        self.state.forwarded_media_keys.lock().clear();
        let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_LEAVE_REMOTE, 0, 0);
            }
        }
        Ok(())
    }

    fn desktop_geometry(&self) -> DesktopGeometry {
        self.state.geometry.read().clone()
    }

    fn lock_state(&self) -> LockState {
        LockState {
            caps_lock: (unsafe { GetKeyState(VK_CAPITAL as i32) } & 1) != 0,
            num_lock: (unsafe { GetKeyState(VK_NUMLOCK_CODE as i32) } & 1) != 0,
        }
    }

    fn last_local_cursor_norm(&self) -> (f32, f32) {
        self.state.last_local_norm.load()
    }
}

fn arrow_to_edge(usage: HidUsage) -> Option<Edge> {
    match usage.raw() {
        x if x == HidUsage::keyboard(0x50).raw() => Some(Edge::Left),
        x if x == HidUsage::keyboard(0x4F).raw() => Some(Edge::Right),
        x if x == HidUsage::keyboard(0x52).raw() => Some(Edge::Top),
        x if x == HidUsage::keyboard(0x51).raw() => Some(Edge::Bottom),
        _ => None,
    }
}

fn translate_ll_key(vk_code: u32, scan_code: u32, flags: u32) -> Option<HidUsage> {
    // Handle Consumer Control / Media VKs explicitly so keyboards/headsets that send
    // scanCode == 0 for volume/playback buttons still resolve reliably.
    match vk_code {
        0xAD => return Some(HidUsage::consumer(0x00E2)), // VK_VOLUME_MUTE
        0xAE => return Some(HidUsage::consumer(0x00EA)), // VK_VOLUME_DOWN
        0xAF => return Some(HidUsage::consumer(0x00E9)), // VK_VOLUME_UP
        0xB0 => return Some(HidUsage::consumer(0x00B5)), // VK_MEDIA_NEXT_TRACK
        0xB1 => return Some(HidUsage::consumer(0x00B6)), // VK_MEDIA_PREV_TRACK
        0xB2 => return Some(HidUsage::consumer(0x00B7)), // VK_MEDIA_STOP
        0xB3 => return Some(HidUsage::consumer(0x00CD)), // VK_MEDIA_PLAY_PAUSE
        _ => {}
    }

    let win_scan = match vk_code {
        VK_PAUSE_CODE => WinScan {
            code: 0x45,
            extended: false,
        },
        VK_NUMLOCK_CODE => WinScan {
            code: 0x45,
            extended: true,
        },
        VK_RSHIFT_CODE => WinScan {
            code: 0x36,
            extended: false,
        },
        VK_SNAPSHOT_CODE => WinScan {
            code: 0x37,
            extended: true,
        },
        _ => WinScan {
            code: (scan_code & 0xFF) as u16,
            extended: (flags & LLKHF_EXTENDED) != 0,
        },
    };
    HidUsage::from_win_scan(win_scan)
}

#[inline]
fn eq_utf16_ascii(wide: &[u16], ascii: &str) -> bool {
    wide.len() == ascii.len()
        && wide
            .iter()
            .zip(ascii.bytes())
            .all(|(&w, b)| w == u16::from(b))
}

/// Checks whether the active foreground window covers an entire monitor (e.g. fullscreen game/VM),
/// excluding Windows desktop shell windows (`Progman`, `WorkerW`, `Shell_TrayWnd`).
fn is_foreground_fullscreen(geom: &DesktopGeometry) -> bool {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return false;
    }
    let mut class_buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32) };
    if len > 0 {
        let wide = &class_buf[..len as usize];
        if eq_utf16_ascii(wide, "Progman")
            || eq_utf16_ascii(wide, "WorkerW")
            || eq_utf16_ascii(wide, "Shell_TrayWnd")
            || eq_utf16_ascii(wide, "Shell_SecondaryTrayWnd")
        {
            return false;
        }
    }

    let mut rect: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return false;
    }

    geom.monitors.iter().any(|m| {
        rect.left <= m.left()
            && rect.top <= m.top()
            && rect.right >= m.right()
            && rect.bottom >= m.bottom()
    })
}

unsafe extern "system" fn keyboard_ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if kb.dwExtraInfo != GLIDE_EXTRA_INFO {
            let pressed = match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
                WM_KEYUP | WM_SYSKEYUP => Some(false),
                _ => None,
            };

            if let Some(pressed) = pressed {
                let suppress = with_hook_state(|state| {
                    let remote = state.remote_mode.load(Ordering::Acquire);

                    // CRITICAL FOR GERMAN / EU KEYBOARDS:
                    // Swallow Windows' synthetic fake VK_LCONTROL (scanCode == 0x21D) before AltGr!
                    if is_windows_fake_altgr_lctrl(kb.vkCode, kb.scanCode) {
                        return remote;
                    }

                    let Some(usage) = translate_ll_key(kb.vkCode, kb.scanCode, kb.flags) else {
                        if remote
                            && state.pin_media_keys_local.load(Ordering::Relaxed)
                            && is_windows_consumer_or_media_vk(kb.vkCode, kb.scanCode, kb.flags)
                        {
                            return false;
                        }
                        trace!(
                            vk = kb.vkCode,
                            scan = kb.scanCode,
                            flags = kb.flags,
                            "Unmapped Win key"
                        );
                        return remote;
                    };

                    // Track modifiers atomically for local hotkey chords & bare-Meta deferral
                    let prev_mask = ModifierMask(state.mods.load(Ordering::Relaxed));
                    let mut mask = prev_mask;
                    mask.update(usage, pressed);
                    state.mods.store(mask.0, Ordering::Relaxed);

                    if usage == state.config.escape_key {
                        if pressed && remote {
                            state.pending_meta.clear();
                            state.cursor_parked.store(false, Ordering::Release);
                            state.remote_mode.store(false, Ordering::SeqCst);
                            state.forwarded_media_keys.lock().clear();
                            let _ = state.tx.send(CaptureEvent::EscapeHotkey);
                            return true;
                        }
                        return remote;
                    }

                    // Direct screen-switch hotkeys (Ctrl + LeftAlt + Arrow) & Cursor Lock (Ctrl + LeftAlt + L).
                    // Specifically checks ALT_LEFT (never ALT_RIGHT / AltGr) so German typing never collides!
                    if pressed
                        && state.config.enable_hotkeys
                        && mask.ctrl()
                        && mask.contains(ModifierMask::ALT_LEFT)
                    {
                        if let Some(direction) = arrow_to_edge(usage) {
                            let _ = state.tx.send(CaptureEvent::HotkeySwitch { direction });
                            return true;
                        }
                        if usage == HidUsage::keyboard(0x0F) {
                            // KeyL
                            let _ = state.tx.send(CaptureEvent::HotkeyToggleLock);
                            return true;
                        }
                    }

                    if remote {
                        // Windows' kernel (win32k) intercepts physical Win+L below WH_KEYBOARD_LL
                        // and unconditionally locks the local Windows server regardless of hook
                        // suppression. Never forward `L` (or any deferred bare Meta) to the remote
                        // client on Win+L so we neither lock both machines nor leak a lone Super
                        // tap that opens GNOME/KDE Activities.
                        if is_windows_workstation_lock_chord(mask, kb.vkCode, pressed) {
                            state.release_to_local_on_suspend("Win+L");
                            return true;
                        }
                        if usage.is_media_key()
                            && !state.forwarded_media_keys.lock().should_forward(
                                usage,
                                pressed,
                                state.pin_media_keys_local.load(Ordering::Relaxed),
                            )
                        {
                            return false;
                        }
                        if state
                            .pending_meta
                            .try_defer(prev_mask, mask, usage, pressed)
                        {
                            return true;
                        }
                        state.flush_pending_meta();
                        let _ = state.tx.send(CaptureEvent::Key { usage, pressed });
                        // Keep the physical keyboard's CapsLock / NumLock LED and Windows'
                        // toggle state in sync with the client when lock-key sync is enabled.
                        if state.sync_lock_keys.load(Ordering::Relaxed)
                            && (kb.vkCode == VK_CAPITAL as u32 || kb.vkCode == VK_NUMLOCK_CODE)
                        {
                            return false;
                        }
                        return true;
                    }

                    false
                })
                .unwrap_or(false);

                if suppress {
                    return 1;
                }
            }
        }
    }

    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

fn maybe_request_raw_input_reclaim(state: &HookState, now: u64) {
    let prev = state.last_raw_reclaim_ms.load(Ordering::Relaxed);
    if (prev == 0 || now.saturating_sub(prev) >= 1000)
        && state
            .last_raw_reclaim_ms
            .compare_exchange(prev, now.max(1), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let hwnd = state.capture_hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_ENSURE_RAW_INPUT, 0, 0);
            }
        }
    }
}

unsafe extern "system" fn mouse_ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if ms.dwExtraInfo != GLIDE_EXTRA_INFO {
            let msg = wparam as u32;
            let suppress = with_hook_state(|state| {
                let remote = state.remote_mode.load(Ordering::Acquire);
                if !remote {
                    match msg {
                        WM_MOUSEMOVE => {
                            let now = state.elapsed_ms();
                            let last_raw = state.last_raw_mouse_ms.load(Ordering::Relaxed);
                            let missed = state
                                .ll_moves_without_raw
                                .fetch_add(1, Ordering::Relaxed)
                                .saturating_add(1);
                            // Self-heal if another window in this process (such as winit's
                            // EventLoop::new or a native file dialog) overwrote our process-wide
                            // Raw Input registration, and fall back to WH_MOUSE_LL deltas for
                            // precision touchpads / RDP sessions that do not emit relative WM_INPUT.
                            // Requiring >= 3 consecutive LL moves without Raw Input avoids spurious
                            // triggers after normal idle pauses when Raw Input is healthy.
                            if missed >= 3 && (last_raw == 0 || now.saturating_sub(last_raw) > 150) {
                                maybe_request_raw_input_reclaim(state, now);
                                if !state.locked.load(Ordering::Relaxed)
                                    && state.active_edges.load() != 0
                                {
                                    let mut cur_pt = POINT { x: 0, y: 0 };
                                    if unsafe { GetCursorPos(&mut cur_pt) } != 0 {
                                        let dx = (ms.pt.x - cur_pt.x) as f32;
                                        let dy = (ms.pt.y - cur_pt.y) as f32;
                                        if dx != 0.0 || dy != 0.0 {
                                            return process_local_edge_motion(state, dx, dy, ms.pt);
                                        }
                                    }
                                }
                            }
                        }
                        WM_LBUTTONDOWN => {
                            let edge_mask = state.active_edges.load();
                            let started_on_edge = if edge_mask != 0 {
                                let geom = state.geometry.read();
                                ALL_EDGES.iter().any(|&e| {
                                    (edge_mask & edge_bit(e)) != 0
                                        && geom.distance_to_edge(e, ms.pt.x, ms.pt.y) <= 4
                                })
                            } else {
                                false
                            };
                            state
                                .lmb_started_on_edge
                                .store(started_on_edge, Ordering::Relaxed);
                            state.lmb_down.store(true, Ordering::SeqCst);
                            // Arm pre-created 4px edge strips IMMEDIATELY on interior LMB down
                            // (matching client.rs) so fast drags to the shared border always hit
                            // an active OLE IDropTarget with zero creation latency.
                            if !started_on_edge
                                && edge_mask != 0
                                && state.drag_drop_files.load(Ordering::Relaxed)
                                && !state.locked.load(Ordering::Relaxed)
                                && !state.drag_strip_armed.swap(true, Ordering::SeqCst)
                            {
                                let _ = state.tx.send(CaptureEvent::DragStripArmed(true));
                            }
                            let _ = state
                                .tx
                                .send(CaptureEvent::LocalLeftButton { pressed: true });
                        }
                        WM_LBUTTONUP => {
                            state.lmb_down.store(false, Ordering::SeqCst);
                            state.lmb_started_on_edge.store(false, Ordering::Relaxed);
                            if state.drag_strip_armed.swap(false, Ordering::SeqCst) {
                                let _ = state.tx.send(CaptureEvent::DragStripArmed(false));
                            }
                            let _ = state
                                .tx
                                .send(CaptureEvent::LocalLeftButton { pressed: false });
                        }
                        _ => {}
                    }
                    return false;
                }

                match msg {
                    WM_MOUSEMOVE => {
                        // Primary motion path is Raw Input (WM_INPUT). Fallback for touchpads /
                        // re-claim if another window temporarily stole Raw Input.
                        // Never flush `pending_meta` on mouse motion so tiny hand tremor between
                        // `Win` and `L` never leaks `Super`.
                        let now = state.elapsed_ms();
                        let last_raw = state.last_raw_mouse_ms.load(Ordering::Relaxed);
                        let missed = state
                            .ll_moves_without_raw
                            .fetch_add(1, Ordering::Relaxed)
                            .saturating_add(1);
                        if missed >= 3 && (last_raw == 0 || now.saturating_sub(last_raw) > 100) {
                            if last_raw == 0 || now.saturating_sub(last_raw) > 250 {
                                maybe_request_raw_input_reclaim(state, now);
                            }
                            let ax = state.anchor_x.load(Ordering::Relaxed);
                            let ay = state.anchor_y.load(Ordering::Relaxed);
                            let dx = (ms.pt.x - ax) as f32;
                            let dy = (ms.pt.y - ay) as f32;
                            if dx != 0.0 || dy != 0.0 {
                                let _ = state.tx.send(CaptureEvent::MouseDelta { dx, dy });
                            }
                        }
                        true
                    }
                    WM_LBUTTONDOWN => {
                        state.lmb_down.store(true, Ordering::SeqCst);
                        state.flush_pending_meta();
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Left,
                            pressed: true,
                        });
                        true
                    }
                    WM_LBUTTONUP => {
                        state.lmb_down.store(false, Ordering::SeqCst);
                        state.flush_pending_meta();
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Left,
                            pressed: false,
                        });
                        true
                    }
                    WM_RBUTTONDOWN => {
                        state.flush_pending_meta();
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Right,
                            pressed: true,
                        });
                        true
                    }
                    WM_RBUTTONUP => {
                        state.flush_pending_meta();
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Right,
                            pressed: false,
                        });
                        true
                    }
                    WM_MBUTTONDOWN => {
                        state.flush_pending_meta();
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Middle,
                            pressed: true,
                        });
                        true
                    }
                    WM_MBUTTONUP => {
                        state.flush_pending_meta();
                        let _ = state.tx.send(CaptureEvent::MouseButton {
                            button: MouseButton::Middle,
                            pressed: false,
                        });
                        true
                    }
                    WM_XBUTTONDOWN | WM_XBUTTONUP => {
                        let pressed = msg == WM_XBUTTONDOWN;
                        let xbtn = ((ms.mouseData >> 16) & 0xFFFF) as u16;
                        let button = if xbtn == XBUTTON1 {
                            Some(MouseButton::Back)
                        } else if xbtn == XBUTTON2 {
                            Some(MouseButton::Forward)
                        } else {
                            None
                        };
                        if let Some(button) = button {
                            state.flush_pending_meta();
                            let _ = state.tx.send(CaptureEvent::MouseButton { button, pressed });
                        }
                        true
                    }
                    WM_MOUSEWHEEL => {
                        let delta = ((ms.mouseData >> 16) & 0xFFFF) as i16;
                        if delta != 0 {
                            state.flush_pending_meta();
                            let _ = state.tx.send(CaptureEvent::MouseScroll {
                                dx_120: 0,
                                dy_120: delta,
                            });
                        }
                        true
                    }
                    WM_MOUSEHWHEEL => {
                        let delta = ((ms.mouseData >> 16) & 0xFFFF) as i16;
                        if delta != 0 {
                            state.flush_pending_meta();
                            let _ = state.tx.send(CaptureEvent::MouseScroll {
                                dx_120: delta,
                                dy_120: 0,
                            });
                        }
                        true
                    }
                    _ => false,
                }
            })
            .unwrap_or(false);

            if suppress {
                return 1;
            }
        }
    }

    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

/// Evaluates local cursor motion `(dx, dy)` at desktop point `pt` for cross-edge switching and
/// drag-and-drop edge strip arming. Returns `true` if focus crossed into remote mode.
fn process_local_edge_motion(state: &HookState, dx: f32, dy: f32, pt: POINT) -> bool {
    if state.locked.load(Ordering::Relaxed) {
        return false;
    }

    state.maybe_refresh_geometry();
    let edge_mask = state.active_edges.load();
    if edge_mask == 0 {
        if state.drag_strip_armed.swap(false, Ordering::SeqCst) {
            let _ = state.tx.send(CaptureEvent::DragStripArmed(false));
        }
        return false;
    }

    let geom = state.geometry.read();
    let (nx, ny) = geom.normalize_point(pt.x, pt.y);
    state.last_local_norm.store(nx, ny);

    // Guard against stale `lmb_down` if an elevated (UIPI) window swallowed WM_LBUTTONUP.
    let mut lmb_down = state.lmb_down.load(Ordering::Acquire);
    if lmb_down && (unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } as u16 & 0x8000) == 0 {
        state.lmb_down.store(false, Ordering::SeqCst);
        state.lmb_started_on_edge.store(false, Ordering::Relaxed);
        lmb_down = false;
    }
    let dnd_enabled = state.drag_drop_files.load(Ordering::Relaxed);
    let started_on_edge = state.lmb_started_on_edge.load(Ordering::Relaxed);

    // Keep invisible 4px edge-strip drop targets armed for the full duration of an interior LMB
    // hold (and disarm immediately once LMB releases or DnD is disabled).
    if lmb_down && dnd_enabled && !started_on_edge {
        if !state.drag_strip_armed.swap(true, Ordering::SeqCst) {
            let _ = state.tx.send(CaptureEvent::DragStripArmed(true));
        }
    } else if state.drag_strip_armed.swap(false, Ordering::SeqCst) {
        let _ = state.tx.send(CaptureEvent::DragStripArmed(false));
    }

    let dead_corner = state.dead_corner_px.load(Ordering::Relaxed).max(0);

    let mut hit: Option<(Edge, f32, f32)> = None;
    for edge in ALL_EDGES {
        if (edge_mask & edge_bit(edge)) == 0 {
            continue;
        }
        let outward = match edge {
            Edge::Left => -dx,
            Edge::Right => dx,
            Edge::Top => -dy,
            Edge::Bottom => dy,
        };
        if outward > 0.0 {
            if let Some(norm) = geom.hit_outer_edge_guarded(edge, pt.x, pt.y, 2, dead_corner) {
                hit = Some((edge, norm, outward));
                break;
            }
        }
    }

    if let Some((edge, norm, outward)) = hit {
        // Only query foreground fullscreen status when the cursor actually pushes an active edge,
        // avoiding 1000Hz GetForegroundWindow/GetClassNameW/GetWindowRect syscalls mid-screen.
        if state.auto_lock_fullscreen.load(Ordering::Relaxed) && is_foreground_fullscreen(&geom) {
            state.clear_edge_push();
            if state.drag_strip_armed.swap(false, Ordering::SeqCst) {
                let _ = state.tx.send(CaptureEvent::DragStripArmed(false));
            }
            return false;
        }

        // While LMB is held, NEVER cross the edge unless cross-edge file DnD is enabled, the
        // drag started away from the edge, AND our edge-strip IDropTarget has actually staged
        // CF_HDROP files on `edge`. This lets Explorer's cross-process COM DragEnter complete
        // reliably and guarantees window titlebar drags (Aero Snap) or text selections are never
        // hijacked across the shared screen edge.
        if lmb_down {
            if !dnd_enabled || started_on_edge {
                state.clear_edge_push();
                return false;
            }
            if !state
                .dnd_probe
                .get()
                .is_some_and(|dnd| dnd.has_staged_files(edge))
            {
                return false;
            }
        }
        let dragging_staged_files = lmb_down;

        let threshold = state.edge_threshold_px.load(Ordering::Relaxed).max(1) as f32;
        let dwell_ms = state.edge_dwell_ms.load(Ordering::Relaxed);

        let mut push = state.edge_push.lock();
        if push.0 == Some(edge) {
            push.1 += outward;
        } else {
            *push = (Some(edge), outward, Some(Instant::now()));
            state.edge_push_active.store(true, Ordering::Relaxed);
        }
        let dwell_ok = dwell_ms == 0
            || push
                .2
                .is_some_and(|t| t.elapsed() >= Duration::from_millis(dwell_ms));
        if push.1 >= threshold && dwell_ok {
            *push = (None, 0.0, None);
            state.edge_push_active.store(false, Ordering::Relaxed);
            drop(push);
            let (px, py) = geom.primary_park_corner();
            drop(geom);
            state.pending_meta.clear();
            state.drag_strip_armed.store(false, Ordering::SeqCst);
            if dragging_staged_files {
                // Keep the cursor right at the edge strip while `server.rs` runs
                // `dnd.cancel_local_drag()` (Escape + VK_LBUTTON release) so Explorer cancels
                // cleanly before `enter_remote()` parks the cursor at `primary_park_corner()`.
                state.anchor_x.store(pt.x, Ordering::Relaxed);
                state.anchor_y.store(pt.y, Ordering::Relaxed);
                state.cursor_parked.store(false, Ordering::Release);
                state.remote_mode.store(true, Ordering::SeqCst);
            } else {
                state.cursor_parked.store(true, Ordering::Release);
                state.remote_mode.store(true, Ordering::SeqCst);
                state.park_cursor_at(px, py);
            }
            let _ = state.tx.send(CaptureEvent::EdgeCrossed {
                edge,
                normalized_pos: norm,
            });
            return true;
        }
    } else if state.edge_push_active.load(Ordering::Relaxed) {
        let mut push = state.edge_push.lock();
        if let Some(prev_edge) = push.0 {
            if geom
                .hit_outer_edge_guarded(prev_edge, pt.x, pt.y, 4, dead_corner)
                .is_none()
            {
                *push = (None, 0.0, None);
                state.edge_push_active.store(false, Ordering::Relaxed);
            }
        } else {
            state.edge_push_active.store(false, Ordering::Relaxed);
        }
    }

    false
}

fn handle_raw_mouse_input(lparam: LPARAM) {
    let mut size: u32 = size_of::<RAWINPUT>() as u32;
    let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
    let res = unsafe {
        GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            &mut raw as *mut RAWINPUT as *mut _,
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if res == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE {
        return;
    }

    let mouse = unsafe { raw.data.mouse };
    if (mouse.usFlags & MOUSE_MOVE_ABSOLUTE) != 0 {
        return;
    }

    let dx = mouse.lLastX as f32;
    let dy = mouse.lLastY as f32;
    if dx == 0.0 && dy == 0.0 {
        return;
    }

    with_hook_state(|state| {
        state
            .last_raw_mouse_ms
            .store(state.elapsed_ms().max(1), Ordering::Relaxed);
        state.ll_moves_without_raw.store(0, Ordering::Relaxed);

        if state.remote_mode.load(Ordering::Acquire) {
            let _ = state.tx.send(CaptureEvent::MouseDelta { dx, dy });
            return;
        }

        if state.locked.load(Ordering::Relaxed) {
            return;
        }

        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } == 0 {
            return;
        }
        let _ = process_local_edge_motion(state, dx, dy, pt);
    });
}

fn register_mouse_raw_input(hwnd: HWND) -> bool {
    let rid = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    unsafe { RegisterRawInputDevices(&rid, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0 }
}

/// Returns `true` if this process's mouse Raw Input `(0x01, 0x02)` sink is currently registered
/// to `hwnd` with `RIDEV_INPUTSINK`.
fn is_mouse_raw_input_owned_by(hwnd: HWND) -> bool {
    let mut count: u32 = 0;
    let cb_size = size_of::<RAWINPUTDEVICE>() as u32;
    unsafe {
        GetRegisteredRawInputDevices(ptr::null_mut(), &mut count, cb_size);
    }
    if count == 0 || count > 256 {
        return false;
    }
    let mut devices = vec![
        RAWINPUTDEVICE {
            usUsagePage: 0,
            usUsage: 0,
            dwFlags: 0,
            hwndTarget: ptr::null_mut(),
        };
        count as usize
    ];
    let got = unsafe { GetRegisteredRawInputDevices(devices.as_mut_ptr(), &mut count, cb_size) };
    if got == u32::MAX {
        return false;
    }
    devices.iter().take(got as usize).any(|d| {
        d.usUsagePage == 0x01
            && d.usUsage == 0x02
            && d.hwndTarget == hwnd
            && (d.dwFlags & RIDEV_INPUTSINK) != 0
    })
}

/// Re-registers our `RIDEV_INPUTSINK` mouse handler if another in-process window (such as winit's
/// `EventLoop::new` or a native file picker) replaced the process-wide Raw Input registration.
fn ensure_mouse_raw_input_registered(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    if !is_mouse_raw_input_owned_by(hwnd) {
        if register_mouse_raw_input(hwnd) {
            info!(
                "Re-claimed Windows Raw Input mouse sink after another window overwrote registration"
            );
        } else {
            warn!("Failed re-claiming Windows Raw Input mouse sink");
        }
    }
}

unsafe extern "system" fn capture_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_INPUT => {
            handle_raw_mouse_input(lparam);
            0
        }
        WM_APP_ENSURE_RAW_INPUT => {
            ensure_mouse_raw_input_registered(hwnd);
            0
        }
        WM_APP_ENTER_REMOTE => {
            with_hook_state(|state| {
                if !state.remote_mode.load(Ordering::Acquire) {
                    return;
                }
                // Save current local cursor before hiding/parking (for hotkey return memory)
                let mut pt = POINT { x: 0, y: 0 };
                let (px, py) = {
                    let geom = state.geometry.read();
                    if unsafe { GetCursorPos(&mut pt) } != 0 {
                        let (nx, ny) = geom.normalize_point(pt.x, pt.y);
                        state.last_local_norm.store(nx, ny);
                    }
                    geom.primary_park_corner()
                };
                state.park_cursor_at(px, py);
            });
            0
        }
        WM_APP_LEAVE_REMOTE => {
            with_hook_state(|state| {
                if let Some(target) = state.pending_leave.lock().take() {
                    let (x, y) = {
                        let geom = state.geometry.read();
                        match target {
                            LeaveTarget::Edge(edge, norm) => geom.point_on_edge(edge, norm, 6),
                            LeaveTarget::Point(nx, ny) => geom.denormalize_point(nx, ny),
                        }
                    };
                    unsafe {
                        SetCursorPos(x, y);
                    }
                }
                state.clear_edge_push();
            });
            0
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            with_hook_state(|state| {
                state.refresh_geometry_now();
            });
            0
        }
        WM_POWERBROADCAST => match wparam as u32 {
            PBT_APMSUSPEND => {
                with_hook_state(|state| {
                    state.release_to_local_on_suspend("PBT_APMSUSPEND");
                });
                1 // TRUE per Win32 WM_POWERBROADCAST specification
            }
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL => {
                ensure_mouse_raw_input_registered(hwnd);
                with_hook_state(|state| {
                    state.handle_resume_or_unlock("WM_POWERBROADCAST_RESUME");
                });
                1
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        },
        WM_WTSSESSION_CHANGE => match wparam as u32 {
            WTS_SESSION_LOCK => {
                with_hook_state(|state| {
                    state.release_to_local_on_suspend("WTS_SESSION_LOCK");
                });
                0
            }
            WTS_SESSION_UNLOCK => {
                ensure_mouse_raw_input_registered(hwnd);
                with_hook_state(|state| {
                    state.handle_resume_or_unlock("WTS_SESSION_UNLOCK");
                });
                0
            }
            _ => 0,
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Starts the Windows 11 server capture hook + Raw Input loop on a dedicated high-priority thread.
pub fn start_windows_capture(config: CaptureConfig) -> Result<CaptureHandle> {
    init_dpi_awareness();
    let geometry = query_desktop_geometry()?;
    let (tx, rx) = mpsc::unbounded_channel();

    let state = Arc::new(HookState {
        capture_hwnd: AtomicIsize::new(0),
        edge_threshold_px: AtomicI32::new(config.edge_threshold_px.max(1)),
        dead_corner_px: AtomicI32::new(config.dead_corner_px.max(0)),
        edge_dwell_ms: AtomicU64::new(config.edge_dwell_ms),
        auto_lock_fullscreen: AtomicBool::new(config.auto_lock_fullscreen),
        sync_lock_keys: AtomicBool::new(config.sync_lock_keys),
        drag_drop_files: AtomicBool::new(config.drag_drop_files),
        dnd_probe: OnceLock::new(),
        lmb_down: AtomicBool::new(false),
        lmb_started_on_edge: AtomicBool::new(false),
        drag_strip_armed: AtomicBool::new(false),
        pin_media_keys_local: AtomicBool::new(config.pin_media_keys_local),
        forwarded_media_keys: Mutex::new(RemoteMediaKeyTracker::new()),
        pending_meta: WindowsPendingMeta::new(),
        config,
        tx,
        active_edges: AtomicEdgeMask::empty(),
        geometry: RwLock::new(geometry),
        remote_mode: AtomicBool::new(false),
        cursor_parked: AtomicBool::new(false),
        locked: AtomicBool::new(false),
        anchor_x: AtomicI32::new(0),
        anchor_y: AtomicI32::new(0),
        last_local_norm: AtomicNormPoint::new(0.5, 0.5),
        pending_leave: Mutex::new(None),
        edge_push_active: AtomicBool::new(false),
        edge_push: Mutex::new((None, 0.0, None)),
        mods: AtomicU8::new(0),
        start_instant: Instant::now(),
        last_raw_mouse_ms: AtomicU64::new(0),
        last_raw_reclaim_ms: AtomicU64::new(0),
        ll_moves_without_raw: AtomicU32::new(0),
        last_geom_refresh_ms: AtomicU64::new(0),
        last_resume_emit: Mutex::new(None),
    });

    let cell = GLOBAL_HOOK_STATE.get_or_init(|| RwLock::new(None));
    *cell.write() = Some(Arc::clone(&state));

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<isize>>();
    let thread_state = Arc::clone(&state);

    thread::Builder::new()
        .name("glide-win-capture".into())
        .spawn(move || {
            unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            }
            match init_capture_window_and_hooks() {
                Ok((hwnd, kb_hook, ms_hook, wts_registered)) => {
                    thread_state
                        .capture_hwnd
                        .store(hwnd as isize, Ordering::Release);
                    let _ = ready_tx.send(Ok(hwnd as isize));
                    debug!(wts_registered, "Windows Raw Input + LL hooks active");

                    let mut msg: MSG = unsafe { std::mem::zeroed() };
                    while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
                        unsafe {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }

                    thread_state.capture_hwnd.store(0, Ordering::Release);
                    unsafe {
                        if !kb_hook.is_null() {
                            UnhookWindowsHookEx(kb_hook);
                        }
                        if !ms_hook.is_null() {
                            UnhookWindowsHookEx(ms_hook);
                        }
                        if wts_registered {
                            WTSUnRegisterSessionNotification(hwnd);
                        }
                        DestroyWindow(hwnd);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to initialize Windows capture hooks");
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .context("failed to spawn glide-win-capture thread")?;

    let hwnd_val = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .context("timed out waiting for Windows capture hook thread")??;

    let control = WindowsCaptureControl {
        hwnd: AtomicIsize::new(hwnd_val),
        state,
    };

    Ok(CaptureHandle {
        control: Box::new(control),
        events: rx,
    })
}

fn init_capture_window_and_hooks() -> Result<(HWND, HHOOK, HHOOK, bool)> {
    let class_name: Vec<u16> = "GlideRawInputCaptureWindow\0".encode_utf16().collect();
    let hinstance = unsafe { GetModuleHandleW(ptr::null()) };

    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(capture_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };

    unsafe {
        RegisterClassW(&wc);
    }

    // Use a hidden top-level window (NOT HWND_MESSAGE): message-only windows never receive
    // broadcast notifications like WM_DISPLAYCHANGE or WM_POWERBROADCAST. Because WS_VISIBLE
    // is omitted and ShowWindow is never called, the window remains invisible and never appears
    // in the taskbar or Alt+Tab.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(anyhow!("CreateWindowExW failed for Glide raw input window"));
    }

    if !register_mouse_raw_input(hwnd) {
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(anyhow!("RegisterRawInputDevices failed for mouse"));
    }

    let kb_hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_ll_proc), hinstance, 0) };
    if kb_hook.is_null() {
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(anyhow!("SetWindowsHookExW(WH_KEYBOARD_LL) failed"));
    }

    let ms_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), hinstance, 0) };
    if ms_hook.is_null() {
        unsafe {
            UnhookWindowsHookEx(kb_hook);
            DestroyWindow(hwnd);
        }
        return Err(anyhow!("SetWindowsHookExW(WH_MOUSE_LL) failed"));
    }

    // Best-effort session lock/unlock subscription (WM_WTSSESSION_CHANGE) so pressing Win+L
    // or idle-locking immediately restores local control and cursor visibility.
    let wts_registered =
        unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } != 0;
    if !wts_registered {
        warn!("WTSRegisterSessionNotification unavailable; session lock detection degraded (WM_POWERBROADCAST remains active)");
    }

    Ok((hwnd, kb_hook, ms_hook, wts_registered))
}
