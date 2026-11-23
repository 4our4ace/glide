//! Native Windows 11 backend powered by `windows-sys` (pure Rust, zero C wrappers).
//!
//! Uses:
//! - Per-Monitor DPI Awareness V2 for exact physical multi-monitor coordinates
//! - Raw Input (`WM_INPUT`) for true unaccelerated mouse deltas & edge push detection
//! - High-priority `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks with German `AltGr` fake-LCtrl filtering
//! - `SendInput` with `KEYEVENTF_SCANCODE` & relative `MOUSEEVENTF_MOVE` + hi-res wheel
//! - `AddClipboardFormatListener` (`WM_CLIPBOARDUPDATE`) for instant bidirectional clipboard sync

pub mod capture;
pub mod clipboard;
pub mod dnd;
pub mod inject;
pub mod tray;

use crate::geometry::{DesktopGeometry, MonitorRect};
use crate::DoctorReport;
use anyhow::Result;
use glide_proto::OsKind;
use parking_lot::{Condvar, Mutex};
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, ProcessPowerThrottling,
    SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    IsWindowEnabled, SendInput, SetActiveWindow, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE,
    MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW,
    GetForegroundWindow, GetLastActivePopup, GetMessageW, GetWindowThreadProcessId,
    IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible, PostMessageW, RegisterClassW,
    SetForegroundWindow, SetLayeredWindowAttributes, ShowWindow, SwitchToThisWindow,
    TranslateMessage, LWA_ALPHA, MSG, SW_HIDE, SW_SHOW, WM_APP, WM_CLOSE, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Unique `dwExtraInfo` tag stamped onto events synthesized by Glide so our hooks
/// immediately ignore our own injected events (zero feedback loops).
pub(crate) const GLIDE_EXTRA_INFO: usize = 0x474C_4944;

const MONITORINFOF_PRIMARY: u32 = 1;

const WM_APP_FOCUS_PROCESS: u32 = WM_APP + 1;
const WM_APP_FOCUS_DISCARD: u32 = WM_APP + 2;
const WM_APP_FOCUS_SHUTDOWN: u32 = WM_APP + 3;

#[inline]
pub(crate) fn eq_utf16_ascii(wide: &[u16], ascii: &str) -> bool {
    wide.len() == ascii.len()
        && wide
            .iter()
            .zip(ascii.bytes())
            .all(|(&w, b)| w == u16::from(b))
}

/// Returns `true` if `hwnd` belongs to the Windows desktop shell or taskbar
/// (`Progman`, `WorkerW`, `Shell_TrayWnd`, `Shell_SecondaryTrayWnd`).
pub(crate) fn is_shell_window_class(hwnd: HWND) -> bool {
    let mut class_buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32) };
    if len <= 0 {
        return false;
    }
    let wide = &class_buf[..len as usize];
    eq_utf16_ascii(wide, "Progman")
        || eq_utf16_ascii(wide, "WorkerW")
        || eq_utf16_ascii(wide, "Shell_TrayWnd")
        || eq_utf16_ascii(wide, "Shell_SecondaryTrayWnd")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusOp {
    Unfocus { g: u64, epoch: u64 },
    Restore { g: u64, epoch: u64 },
}

impl FocusOp {
    #[inline]
    fn generation(self) -> u64 {
        match self {
            FocusOp::Unfocus { g, .. } | FocusOp::Restore { g, .. } => g,
        }
    }
}

struct FocusCore {
    gen: AtomicU64,
    discard_epoch: AtomicU64,
    last_unfocus_gen: AtomicU64,
    cleared_through_gen: AtomicU64,
    sink_hwnd: AtomicIsize,
    shutdown: AtomicBool,
    worker_failed: AtomicBool,
    pending: Mutex<Option<FocusOp>>,
    /// Highest generation whose worker handling has finished, allowing Windows client
    /// `Enter`/`EnterAt` to wait briefly so immediate post-jump keystrokes do not hit the sink.
    completed_gen: (Mutex<u64>, Condvar),
}

struct FocusWorkerState {
    /// Discard epoch that currently owns the shown focus-sink window (`None` while hidden).
    sink_epoch: Option<u64>,
    /// Saved foreground window to re-focus on return, tagged with its discard epoch.
    saved: Option<(u64, HWND)>,
}

/// Lock-free frontend + dedicated lazy Win32 focus-sink coordinator for `unfocus_on_leave`
/// and automatic focus restoration when pointer/keyboard focus returns to this Windows PC.
///
/// Why an invisible focus-sink window instead of `SetForegroundWindow(GetShellWindow())`:
/// On Windows 11, background processes (including Raw Input / `WH_MOUSE_LL` listeners) are
/// blocked by `ForegroundLockTimeout` from transferring foreground directly to a third-party
/// window like `explorer.exe` (`Progman`), and would again lack foreground permission when
/// restoring the user's window on return.
///
/// Owning a hidden 1x1 off-screen `WS_EX_TOOLWINDOW | WS_EX_LAYERED` focus-sink window on a
/// lazily-spawned `"glide-win-focus"` message-loop thread solves both halves cleanly:
/// - On leave: unlocks foreground via a harmless `GLIDE_EXTRA_INFO`-tagged zero-delta mouse
///   `SendInput` (Microsoft PowerToys Command Palette pattern), activates the invisible sink
///   window (with `AttachThreadInput` + `SetActiveWindow` and `SwitchToThisWindow` fallbacks),
///   and verifies `GetForegroundWindow() == sink_hwnd`.
/// - On return: because Glide's sink window already owns foreground, `SetForegroundWindow`
///   to the saved window succeeds natively before hiding the sink window (`SW_HIDE`).
pub(crate) struct WindowsFocusState {
    core: Arc<FocusCore>,
    worker_started: AtomicBool,
    spawn_lock: Mutex<()>,
}

impl WindowsFocusState {
    pub(crate) fn new() -> Self {
        Self {
            core: Arc::new(FocusCore {
                gen: AtomicU64::new(0),
                discard_epoch: AtomicU64::new(0),
                last_unfocus_gen: AtomicU64::new(0),
                cleared_through_gen: AtomicU64::new(0),
                sink_hwnd: AtomicIsize::new(0),
                shutdown: AtomicBool::new(false),
                worker_failed: AtomicBool::new(false),
                pending: Mutex::new(None),
                completed_gen: (Mutex::new(0), Condvar::new()),
            }),
            worker_started: AtomicBool::new(false),
            spawn_lock: Mutex::new(()),
        }
    }

    fn ensure_worker_started(&self) -> bool {
        if self.core.shutdown.load(Ordering::Acquire)
            || self.core.worker_failed.load(Ordering::Acquire)
        {
            return false;
        }
        if self.worker_started.load(Ordering::Acquire) {
            return true;
        }
        let _guard = self.spawn_lock.lock();
        if self.core.shutdown.load(Ordering::Acquire)
            || self.core.worker_failed.load(Ordering::Acquire)
        {
            return false;
        }
        if self.worker_started.load(Ordering::Acquire) {
            return true;
        }
        let core = Arc::clone(&self.core);
        match std::thread::Builder::new()
            .name("glide-win-focus".into())
            .spawn(move || focus_worker_loop(core))
        {
            Ok(_) => {
                self.worker_started.store(true, Ordering::Release);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to spawn Windows focus-sink worker thread");
                false
            }
        }
    }

    /// Cancels any pending unfocus/restore, discards any saved window handle, and hides
    /// the focus-sink window if active. Lock-free and non-blocking so it is safe on hook threads.
    pub(crate) fn discard(&self) {
        self.core.discard_epoch.fetch_add(1, Ordering::SeqCst);
        let g = self.core.gen.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        self.core.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
        let hwnd = self.core.sink_hwnd.load(Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_FOCUS_DISCARD, 0, 0);
            }
        }
    }

    /// Cancels an unstarted unfocus without discarding an already-saved HWND so an
    /// immediately-following `leave_remote_at` (e.g. `EscapeHotkey`) can still restore focus.
    pub(crate) fn bump_gen_only(&self) {
        self.core.gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Dispatches foreground-window unfocusing to the lazily-spawned `"glide-win-focus"`
    /// thread so low-level hooks and QUIC `Enter`/`Leave` sends never block.
    pub(crate) fn schedule_unfocus(&self) {
        if self.core.shutdown.load(Ordering::Acquire)
            || self.core.worker_failed.load(Ordering::Acquire)
        {
            return;
        }
        let epoch = self.core.discard_epoch.load(Ordering::Acquire);
        let g = self.core.gen.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        self.core.last_unfocus_gen.fetch_max(g, Ordering::AcqRel);
        enqueue_pending(&self.core, FocusOp::Unfocus { g, epoch });
        if !self.ensure_worker_started() {
            let mut slot = self.core.pending.lock();
            if slot.is_some_and(|prev| prev.generation() == g) {
                *slot = None;
            }
            self.core.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
            return;
        }
        let hwnd = self.core.sink_hwnd.load(Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_FOCUS_PROCESS, 0, 0);
            }
        }
    }

    fn schedule_restore_inner(&self) -> Option<u64> {
        let g = self.core.gen.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let last_unfocus = self.core.last_unfocus_gen.load(Ordering::Acquire);
        if last_unfocus == 0
            || last_unfocus <= self.core.cleared_through_gen.load(Ordering::Acquire)
        {
            return None;
        }
        if self.core.shutdown.load(Ordering::Acquire)
            || self.core.worker_failed.load(Ordering::Acquire)
        {
            return None;
        }
        let epoch = self.core.discard_epoch.load(Ordering::Acquire);
        enqueue_pending(&self.core, FocusOp::Restore { g, epoch });
        let hwnd = self.core.sink_hwnd.load(Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_FOCUS_PROCESS, 0, 0);
            }
        }
        Some(g)
    }

    /// Dispatches restoration of the previously unfocused foreground window to the
    /// `"glide-win-focus"` thread. Returns immediately with zero syscalls when nothing
    /// was unfocused.
    pub(crate) fn schedule_restore(&self) {
        let _ = self.schedule_restore_inner();
    }

    /// Dispatches restoration and waits up to `max_wait` for the worker to hand foreground
    /// back to the saved window before returning (used on Windows client `Enter`/`EnterAt`
    /// so immediate post-jump keystrokes never land in the hidden sink window).
    pub(crate) fn schedule_restore_and_wait(&self, max_wait: Duration) {
        let Some(target_gen) = self.schedule_restore_inner() else {
            return;
        };
        let deadline = Instant::now() + max_wait;
        let (lock, cvar) = &self.core.completed_gen;
        let mut done = lock.lock();
        while *done < target_gen
            && !self.core.shutdown.load(Ordering::Acquire)
            && !self.core.worker_failed.load(Ordering::Acquire)
        {
            if cvar.wait_until(&mut done, deadline).timed_out() {
                break;
            }
        }
    }

    /// Permanently stops the lazy focus-sink worker thread (if started) and hides/destroys
    /// the focus-sink window. Safe to call multiple times.
    pub(crate) fn shutdown(&self) {
        self.core.shutdown.store(true, Ordering::SeqCst);
        self.core.discard_epoch.fetch_add(1, Ordering::SeqCst);
        let g = self.core.gen.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        self.core.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
        let hwnd = self.core.sink_hwnd.swap(0, Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_FOCUS_SHUTDOWN, 0, 0);
            }
        }
        let (lock, cvar) = &self.core.completed_gen;
        let _done = lock.lock();
        cvar.notify_all();
    }
}

impl Drop for WindowsFocusState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn enqueue_pending(core: &FocusCore, op: FocusOp) {
    let mut slot = core.pending.lock();
    if slot.is_none_or(|prev| op.generation() > prev.generation()) {
        *slot = Some(op);
    }
}

fn mark_completed(core: &FocusCore, g: u64) {
    let (lock, cvar) = &core.completed_gen;
    let mut done = lock.lock();
    if g > *done {
        *done = g;
        cvar.notify_all();
    }
}

unsafe extern "system" fn focus_sink_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLOSE {
        // Never allow an external WM_CLOSE (e.g. Alt+F4 while unfocused on a Windows client)
        // to destroy the focus-sink window; hide it instead so focus naturally yields.
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn create_focus_sink_window() -> Option<HWND> {
    let class_name: Vec<u16> = "GlideFocusSinkWindow\0".encode_utf16().collect();
    let hinstance = unsafe { GetModuleHandleW(ptr::null()) };

    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(focus_sink_wnd_proc),
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

    // Off-screen 1x1 borderless popup excluded from Taskbar, Alt+Tab, Win+Tab, and Virtual
    // Desktops (WS_EX_TOOLWINDOW), with layered alpha=1/255 (never cloaked/unfocusable,
    // visually invisible, and intentionally without WS_EX_NOACTIVATE or WS_EX_TOPMOST).
    // Null title avoids screen readers / UI Automation announcing an internal class name.
    // Starts hidden (no WS_VISIBLE) until unfocus_on_leave activates it.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            class_name.as_ptr(),
            ptr::null(),
            WS_POPUP,
            -16_000,
            -16_000,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        tracing::warn!("CreateWindowExW failed for Glide focus-sink window");
        return None;
    }

    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);
    }
    Some(hwnd)
}

fn focus_worker_loop(core: Arc<FocusCore>) {
    let Some(sink_hwnd) = create_focus_sink_window() else {
        core.worker_failed.store(true, Ordering::Release);
        *core.pending.lock() = None;
        let g = core.gen.load(Ordering::Acquire);
        core.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
        mark_completed(&core, u64::MAX);
        return;
    };

    core.sink_hwnd.store(sink_hwnd as isize, Ordering::SeqCst);

    let mut state = FocusWorkerState {
        sink_epoch: None,
        saved: None,
    };

    if !core.shutdown.load(Ordering::SeqCst) {
        // Drain any op queued while this thread was lazily starting up.
        if let Some(op) = core.pending.lock().take() {
            handle_focus_op(&core, sink_hwnd, &mut state, op);
            mark_completed(&core, op.generation());
        }

        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while !core.shutdown.load(Ordering::Acquire)
            && unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0
        {
            if msg.hwnd == sink_hwnd {
                match msg.message {
                    WM_APP_FOCUS_PROCESS => {
                        if let Some(op) = core.pending.lock().take() {
                            handle_focus_op(&core, sink_hwnd, &mut state, op);
                            mark_completed(&core, op.generation());
                        }
                        continue;
                    }
                    WM_APP_FOCUS_DISCARD => {
                        handle_focus_discard(&core, sink_hwnd, &mut state);
                        continue;
                    }
                    WM_APP_FOCUS_SHUTDOWN => {
                        break;
                    }
                    _ => {}
                }
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    core.sink_hwnd.store(0, Ordering::SeqCst);
    mark_completed(&core, u64::MAX);
    unsafe {
        ShowWindow(sink_hwnd, SW_HIDE);
        DestroyWindow(sink_hwnd);
    }
}

fn handle_focus_op(core: &FocusCore, sink_hwnd: HWND, state: &mut FocusWorkerState, op: FocusOp) {
    match op {
        FocusOp::Unfocus { g, epoch } => handle_unfocus(core, sink_hwnd, state, g, epoch),
        FocusOp::Restore { g, epoch } => handle_restore(core, sink_hwnd, state, g, epoch),
    }
}

fn handle_focus_discard(core: &FocusCore, sink_hwnd: HWND, state: &mut FocusWorkerState) {
    let cur_epoch = core.discard_epoch.load(Ordering::Acquire);
    if state
        .saved
        .is_some_and(|(saved_epoch, _)| saved_epoch != cur_epoch)
    {
        state.saved = None;
    }
    if state.sink_epoch.is_none_or(|e| e != cur_epoch) {
        state.sink_epoch = None;
        unsafe {
            ShowWindow(sink_hwnd, SW_HIDE);
        }
    }
}

fn send_noop_mouse_input(flags: u32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
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

/// Moves Windows foreground to Glide's invisible `sink_hwnd` using a 3-tier Windows 11
/// escalation path, verifying `GetForegroundWindow() == sink_hwnd` after each tier.
fn take_foreground_with_sink(sink_hwnd: HWND) -> bool {
    // Tier 1 (Microsoft PowerToys Command Palette Windows 11 foreground unlock):
    // A zero-flag, zero-delta mouse INPUT tagged with GLIDE_EXTRA_INFO marks this process
    // as the last input source in Win32k (lifting ForegroundLockTimeout) without moving the
    // cursor, clicking, touching the keyboard, or triggering our own WH_MOUSE_LL / WM_INPUT.
    send_noop_mouse_input(0);
    unsafe {
        ShowWindow(sink_hwnd, SW_SHOW);
        let _ = SetForegroundWindow(sink_hwnd);
    }
    if unsafe { GetForegroundWindow() } == sink_hwnd {
        return true;
    }

    // Tier 2: Pair a zero-delta MOUSEEVENTF_MOVE with AttachThreadInput + SetActiveWindow
    // from sink_hwnd's own owning thread, re-querying GetForegroundWindow() fresh in case
    // foreground changed or closed during Tier 1.
    send_noop_mouse_input(MOUSEEVENTF_MOVE);
    let cur_tid = unsafe { GetCurrentThreadId() };
    let cur_fg = unsafe { GetForegroundWindow() };
    let fg_tid = if !cur_fg.is_null() && unsafe { IsHungAppWindow(cur_fg) } == 0 {
        unsafe { GetWindowThreadProcessId(cur_fg, ptr::null_mut()) }
    } else {
        0
    };
    let attached =
        fg_tid != 0 && fg_tid != cur_tid && unsafe { AttachThreadInput(cur_tid, fg_tid, 1) } != 0;
    unsafe {
        let _ = SetForegroundWindow(sink_hwnd);
        let _ = SetActiveWindow(sink_hwnd);
    }
    if attached {
        unsafe {
            AttachThreadInput(cur_tid, fg_tid, 0);
        }
    }
    if unsafe { GetForegroundWindow() } == sink_hwnd {
        return true;
    }

    // Tier 3: Escalate via SwitchToThisWindow (fAltTab = TRUE) for Windows 11 shell/UWP
    // foreground transitions where SetForegroundWindow alone is ignored.
    unsafe {
        SwitchToThisWindow(sink_hwnd, 1);
    }
    unsafe { GetForegroundWindow() == sink_hwnd }
}

fn handle_unfocus(
    core: &FocusCore,
    sink_hwnd: HWND,
    state: &mut FocusWorkerState,
    g: u64,
    epoch: u64,
) {
    if core.gen.load(Ordering::SeqCst) != g || core.discard_epoch.load(Ordering::SeqCst) != epoch {
        return;
    }

    let fg = unsafe { GetForegroundWindow() };

    // If our sink window already owns foreground (e.g. duplicate Leave or direct
    // Client A -> Client B switch), preserve any saved HWND from the current epoch.
    if fg == sink_hwnd {
        state.sink_epoch = Some(epoch);
        if state
            .saved
            .is_some_and(|(saved_epoch, _)| saved_epoch != epoch)
        {
            state.saved = None;
        }
        return;
    }

    state.saved = None;

    if fg.is_null() || is_shell_window_class(fg) {
        state.sink_epoch = None;
        unsafe {
            ShowWindow(sink_hwnd, SW_HIDE);
        }
        core.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
        return;
    }

    if unsafe { IsHungAppWindow(fg) } != 0 {
        tracing::debug!("Skipping unfocus_on_leave for hung foreground window");
        state.sink_epoch = None;
        unsafe {
            ShowWindow(sink_hwnd, SW_HIDE);
        }
        core.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
        return;
    }

    if core.gen.load(Ordering::SeqCst) != g || core.discard_epoch.load(Ordering::SeqCst) != epoch {
        return;
    }

    let took_focus = take_foreground_with_sink(sink_hwnd);
    if took_focus && core.discard_epoch.load(Ordering::SeqCst) == epoch {
        // Save even if `gen` moved (e.g. a queued Restore arrived mid-call) so that
        // Restore immediately gives foreground back to `fg` and hides `sink_hwnd`.
        state.sink_epoch = Some(epoch);
        state.saved = Some((epoch, fg));
    } else {
        state.sink_epoch = None;
        state.saved = None;
        unsafe {
            ShowWindow(sink_hwnd, SW_HIDE);
        }
        if !took_focus {
            tracing::debug!(
                "Unable to move foreground to focus-sink window during unfocus_on_leave (e.g. UIPI elevated window)"
            );
        }
        core.cleared_through_gen.fetch_max(g, Ordering::AcqRel);
    }
}

fn resolve_restorable_target(raw_target: HWND) -> Option<HWND> {
    let mut target = raw_target;
    if target.is_null()
        || unsafe { IsWindow(target) } == 0
        || unsafe { IsWindowVisible(target) } == 0
        || unsafe { IsIconic(target) } != 0
        || unsafe { IsHungAppWindow(target) } != 0
    {
        return None;
    }

    // Modal dialog safety: if the saved owner window is disabled by a modal dialog,
    // redirect focus to its active enabled popup rather than a disabled owner.
    if unsafe { IsWindowEnabled(target) } == 0 {
        let popup = unsafe { GetLastActivePopup(target) };
        if !popup.is_null()
            && popup != target
            && unsafe { IsWindow(popup) } != 0
            && unsafe { IsWindowVisible(popup) } != 0
            && unsafe { IsIconic(popup) } == 0
            && unsafe { IsWindowEnabled(popup) } != 0
            && unsafe { IsHungAppWindow(popup) } == 0
        {
            target = popup;
        } else {
            return None;
        }
    }

    Some(target)
}

fn handle_restore(
    core: &FocusCore,
    sink_hwnd: HWND,
    state: &mut FocusWorkerState,
    g: u64,
    epoch: u64,
) {
    if core.gen.load(Ordering::Acquire) != g || core.discard_epoch.load(Ordering::Acquire) != epoch
    {
        return;
    }

    let cur_fg = unsafe { GetForegroundWindow() };
    let sink_holds_focus = cur_fg == sink_hwnd || cur_fg.is_null();

    // Validate the candidate target BEFORE mutating `state` so that if a rapid
    // `schedule_unfocus()` bumps `gen` (same `discard_epoch`) while inspecting the window,
    // we leave `state.saved`, `state.sink_epoch`, and the visible `sink_hwnd` untouched.
    let target = state
        .saved
        .filter(|&(saved_epoch, _)| sink_holds_focus && saved_epoch == epoch)
        .and_then(|(_, raw_target)| resolve_restorable_target(raw_target));

    if core.discard_epoch.load(Ordering::Acquire) != epoch {
        handle_focus_discard(core, sink_hwnd, state);
        return;
    }
    if core.gen.load(Ordering::Acquire) != g {
        return;
    }

    state.saved = None;
    state.sink_epoch = None;
    core.cleared_through_gen.fetch_max(g, Ordering::AcqRel);

    if let Some(target) = target {
        // Because sink_hwnd currently owns foreground, SetForegroundWindow(target)
        // is natively permitted by Windows 11 before we hide sink_hwnd.
        unsafe {
            let _ = SetForegroundWindow(target);
        }
        let after = unsafe { GetForegroundWindow() };
        if after == sink_hwnd || after.is_null() {
            // UWP / ApplicationFrameWindow fallback (PowerToys #50455): only escalate if
            // foreground is still stuck on our sink (don't fight an app that handed focus
            // to its own owned child/dialog window).
            unsafe {
                SwitchToThisWindow(target, 1);
            }
            let after_fallback = unsafe { GetForegroundWindow() };
            if after_fallback == sink_hwnd || after_fallback.is_null() {
                tracing::debug!("Unable to restore saved foreground window after unfocus_on_leave");
            }
        }
    }

    // Always hide the focus-sink window AFTER transferring foreground away so hiding it
    // does not cause Windows to pick a different Z-order window.
    unsafe {
        ShowWindow(sink_hwnd, SW_HIDE);
    }
}

/// Enables Per-Monitor DPI Awareness V2 so Windows reports real physical coordinates
/// across mixed-DPI multi-monitor setups (e.g. 100% + 125% + 150% + 200%).
pub fn init_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Opts this process out of Windows EcoQoS (E-core efficiency throttling) and
/// Windows 11 minimized/background timer-resolution throttling so 1000Hz input hooks
/// and QUIC timers stay jitter-free even while minimized to the system tray.
///
/// Issued as two separate `SetProcessInformation` calls because Windows 10 supports
/// `PROCESS_POWER_THROTTLING_EXECUTION_SPEED` while `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`
/// (Windows 11 22000+) causes Windows 10 to reject a combined mask with `ERROR_INVALID_PARAMETER`.
pub fn init_windows_realtime_qos() {
    let process = unsafe { GetCurrentProcess() };
    for flag in [
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    ] {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: flag,
            StateMask: 0,
        };
        unsafe {
            let _ = SetProcessInformation(
                process,
                ProcessPowerThrottling,
                &state as *const PROCESS_POWER_THROTTLING_STATE as *const std::ffi::c_void,
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

unsafe extern "system" fn enum_monitor_cb(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _lprect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(lparam as *mut Vec<MonitorRect>) };

    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;

    if unsafe {
        GetMonitorInfoW(
            hmonitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
    } != 0
    {
        let rc = info.monitorInfo.rcMonitor;
        let primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;

        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        let _ = unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
        let scale = (dpi_x as f32 / 96.0).max(0.5);

        let name_end = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let name = String::from_utf16_lossy(&info.szDevice[..name_end]);

        let width = rc.right - rc.left;
        let height = rc.bottom - rc.top;
        if width > 0 && height > 0 {
            monitors.push(MonitorRect {
                name,
                x: rc.left,
                y: rc.top,
                width,
                height,
                scale,
                primary,
            });
        }
    }

    1 // TRUE: continue enumeration
}

/// Queries all active Windows monitors in physical virtual desktop coordinates.
pub fn query_desktop_geometry() -> Result<DesktopGeometry> {
    let mut monitors: Vec<MonitorRect> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(enum_monitor_cb),
            &mut monitors as *mut Vec<MonitorRect> as LPARAM,
        );
    }
    if monitors.is_empty() {
        return Ok(DesktopGeometry::fallback());
    }
    if !monitors.iter().any(|m| m.primary) {
        monitors[0].primary = true;
    }
    Ok(DesktopGeometry::new(monitors))
}

/// Runs Windows 11 diagnostics for `glide doctor`.
pub fn run_doctor() -> Result<DoctorReport> {
    init_dpi_awareness();
    let geometry = query_desktop_geometry()?;
    let notes = vec![
        "Per-Monitor DPI Awareness V2 enabled.".into(),
        "Raw Input (WM_INPUT) + low-level hooks ready for unaccelerated mouse deltas.".into(),
        "German/EU AltGr fake-LCtrl (scanCode 0x21D) filter active.".into(),
        "Tip: Allow UDP ports 24800 (QUIC) and 24801 (LAN auto-discovery) on Private networks in Windows Firewall.".into(),
    ];
    Ok(DoctorReport {
        os_kind: OsKind::Windows,
        display_info: format!("Windows Desktop ({} monitor(s))", geometry.monitors.len()),
        geometry,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_pending_keeps_highest_generation() {
        let state = WindowsFocusState::new();
        enqueue_pending(&state.core, FocusOp::Unfocus { g: 5, epoch: 1 });
        // Out-of-order older generation must not overwrite newer op.
        enqueue_pending(&state.core, FocusOp::Restore { g: 3, epoch: 1 });
        assert_eq!(
            *state.core.pending.lock(),
            Some(FocusOp::Unfocus { g: 5, epoch: 1 })
        );
        // Newer generation replaces older op.
        enqueue_pending(&state.core, FocusOp::Restore { g: 6, epoch: 1 });
        assert_eq!(
            *state.core.pending.lock(),
            Some(FocusOp::Restore { g: 6, epoch: 1 })
        );
    }

    #[test]
    fn mark_completed_is_monotonic() {
        let state = WindowsFocusState::new();
        mark_completed(&state.core, 4);
        mark_completed(&state.core, 2);
        assert_eq!(*state.core.completed_gen.0.lock(), 4);
        mark_completed(&state.core, 9);
        assert_eq!(*state.core.completed_gen.0.lock(), 9);
    }

    #[test]
    fn focus_state_fast_paths_and_shutdown_do_not_spawn_worker() {
        let state = WindowsFocusState::new();
        state.schedule_restore();
        state.schedule_restore_and_wait(Duration::from_millis(5));
        state.bump_gen_only();
        state.discard();
        assert!(!state.worker_started.load(Ordering::Acquire));

        // After shutdown, schedule_unfocus must remain a no-op and never spawn.
        state.shutdown();
        state.shutdown();
        state.schedule_unfocus();
        state.schedule_restore_and_wait(Duration::from_millis(5));
        assert!(!state.worker_started.load(Ordering::Acquire));
        assert!(state.core.pending.lock().is_none());
    }

    #[test]
    fn worker_failed_latch_short_circuits_future_ops() {
        let state = WindowsFocusState::new();
        state.core.worker_failed.store(true, Ordering::Release);
        state.schedule_unfocus();
        state.schedule_restore_and_wait(Duration::from_millis(5));
        assert!(!state.worker_started.load(Ordering::Acquire));
        assert!(state.core.pending.lock().is_none());
        assert_eq!(state.core.last_unfocus_gen.load(Ordering::Acquire), 0);
    }
}
