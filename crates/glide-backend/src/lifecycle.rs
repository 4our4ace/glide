//! Pure event-driven OS power, shutdown, and process-signal lifecycle monitoring.
//!
//! Zero polling, zero timers, zero heartbeat probes:
//! - **Linux (`systemd-logind`)**: subscribes on the system D-Bus to
//!   `org.freedesktop.login1.Manager::{PrepareForShutdown, PrepareForSleep}` and holds an
//!   unprivileged `delay` inhibitor (`shutdown:sleep`). When the OS signals imminent shutdown
//!   or suspend, the inhibitor file descriptor is moved into [`ShutdownInhibitorGuard`] so
//!   `glide-core` can close active QUIC connections and flush `CONNECTION_CLOSE` onto the wire
//!   before dropping the guard to release the OS immediately.
//! - **Windows**: spawns a lightweight hidden top-level window listening for
//!   `WM_QUERYENDSESSION`, `WM_ENDSESSION`, and `WM_POWERBROADCAST` (`PBT_APMSUSPEND` /
//!   `PBT_APMRESUME*`). The window procedure blocks briefly (bounded well below Windows'
//!   shutdown/suspend UI timeouts) until `glide-core` drops [`ShutdownInhibitorGuard`], letting
//!   QUIC `CONNECTION_CLOSE` reach remote peers before Windows tears down the network stack.
//! - **Process signals ([`shutdown_signal`])**: catches `SIGINT` / `SIGTERM` / `SIGHUP` on Unix
//!   and `CTRL_C` / `CTRL_CLOSE` / `CTRL_LOGOFF` / `CTRL_SHUTDOWN` on Windows so CLI and GUI
//!   processes flush engine shutdown cleanly before exit.

use std::fmt;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// RAII guard delaying OS shutdown or sleep until Glide finishes flushing QUIC
/// `CONNECTION_CLOSE` frames onto the wire.
///
/// Dropping this guard releases the underlying OS delay inhibitor immediately:
/// - On Linux: closes the `systemd-logind` `Inhibit(..., "delay")` file descriptor.
/// - On Windows: signals the hidden lifecycle window's `WndProc` to return immediately.
/// - In tests / fallback paths: runs an optional release callback or acts as a no-op.
pub struct ShutdownInhibitorGuard {
    #[cfg(target_os = "linux")]
    _fd: Option<std::os::fd::OwnedFd>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl ShutdownInhibitorGuard {
    /// Creates a no-op guard (used when the OS did not grant a delay inhibitor).
    pub fn noop() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            _fd: None,
            on_drop: None,
        }
    }

    /// Creates a guard that invokes `on_drop` exactly once when dropped.
    pub fn with_on_drop<F>(on_drop: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            #[cfg(target_os = "linux")]
            _fd: None,
            on_drop: Some(Box::new(on_drop)),
        }
    }

    #[cfg(target_os = "linux")]
    fn from_opt_fd(fd: Option<std::os::fd::OwnedFd>) -> Self {
        Self {
            _fd: fd,
            on_drop: None,
        }
    }
}

impl Drop for ShutdownInhibitorGuard {
    fn drop(&mut self) {
        if let Some(cb) = self.on_drop.take() {
            cb();
        }
        #[cfg(target_os = "linux")]
        {
            self._fd = None;
        }
    }
}

impl fmt::Debug for ShutdownInhibitorGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(target_os = "linux")]
        let has_fd = self._fd.is_some();
        #[cfg(not(target_os = "linux"))]
        let has_fd = false;

        f.debug_struct("ShutdownInhibitorGuard")
            .field("has_fd", &has_fd)
            .field("has_on_drop", &self.on_drop.is_some())
            .finish()
    }
}

/// Event-driven OS lifecycle transitions emitted by [`SystemLifecycleMonitor`].
#[derive(Debug)]
pub enum SystemLifecycleEvent {
    /// OS is preparing to power off, reboot, or end the desktop session.
    ///
    /// Callers should close active QUIC connections and flush `CONNECTION_CLOSE` frames
    /// before dropping the attached [`ShutdownInhibitorGuard`].
    PreparingToShutdown(ShutdownInhibitorGuard),

    /// OS is preparing to suspend or hibernate.
    ///
    /// Callers should pause reconnects/accepts, close active QUIC connections, and flush
    /// `CONNECTION_CLOSE` frames before dropping the attached [`ShutdownInhibitorGuard`].
    PreparingToSleep(ShutdownInhibitorGuard),

    /// OS resumed from sleep/hibernate or a pending shutdown was cancelled.
    ///
    /// Callers should un-pause and resume accepting/reconnecting immediately.
    Resumed,
}

/// Pure event-driven monitor for OS shutdown, sleep/suspend, and wake/resume events.
///
/// Dropping this monitor stops the background OS listener and immediately releases any held
/// OS delay inhibitor.
pub struct SystemLifecycleMonitor {
    events: Option<mpsc::UnboundedReceiver<SystemLifecycleEvent>>,
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl SystemLifecycleMonitor {
    /// Starts the native OS lifecycle monitor (best-effort; never fails engine startup if
    /// D-Bus / `systemd-logind` or Win32 window creation is unavailable).
    pub fn start() -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::start_linux_lifecycle_monitor()
        }
        #[cfg(target_os = "windows")]
        {
            windows::start_windows_lifecycle_monitor()
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Self::disabled()
        }
    }

    /// Creates a dormant monitor that emits no events and consumes zero resources.
    pub fn disabled() -> Self {
        Self {
            events: None,
            stop: None,
        }
    }

    /// Creates a mock lifecycle monitor driven by an explicit channel sender (for tests).
    pub fn from_channel() -> (Self, mpsc::UnboundedSender<SystemLifecycleEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                events: Some(rx),
                stop: None,
            },
            tx,
        )
    }

    /// Returns `true` if the underlying event channel is still open.
    pub fn is_active(&self) -> bool {
        self.events.is_some()
    }

    /// Awaits the next [`SystemLifecycleEvent`]. Returns `None` once the monitor is disabled
    /// or its OS backend stops.
    pub async fn recv(&mut self) -> Option<SystemLifecycleEvent> {
        let rx = self.events.as_mut()?;
        let ev = rx.recv().await;
        if ev.is_none() {
            self.events = None;
        }
        ev
    }

    /// Non-blockingly drains the next queued [`SystemLifecycleEvent`], if any.
    pub fn try_recv(&mut self) -> Option<SystemLifecycleEvent> {
        let rx = self.events.as_mut()?;
        match rx.try_recv() {
            Ok(ev) => Some(ev),
            Err(mpsc::error::TryRecvError::Empty) => None,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                self.events = None;
                None
            }
        }
    }
}

impl Drop for SystemLifecycleMonitor {
    fn drop(&mut self) {
        // Drop the receiver first so any buffered ShutdownInhibitorGuards release immediately.
        self.events = None;
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

/// Awaits the first OS process termination signal (`SIGINT`/`SIGTERM`/`SIGHUP` on Unix;
/// `CTRL_C`/`CTRL_CLOSE`/`CTRL_LOGOFF`/`CTRL_SHUTDOWN` on Windows) and returns its name.
///
/// All platform signal listeners are registered immediately on first poll with zero timers.
/// If any individual signal registration fails (or its stream ends without firing), that arm
/// stays pending rather than triggering a spurious shutdown.
pub async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        use tracing::warn;

        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "Failed to register SIGINT handler");
                None
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "Failed to register SIGTERM handler");
                None
            }
        };
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "Failed to register SIGHUP handler");
                None
            }
        };

        let on_int = async {
            if let Some(s) = sigint.as_mut() {
                if s.recv().await.is_some() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        };
        let on_term = async {
            if let Some(s) = sigterm.as_mut() {
                if s.recv().await.is_some() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        };
        let on_hup = async {
            if let Some(s) = sighup.as_mut() {
                if s.recv().await.is_some() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        };

        tokio::select! {
            _ = on_int => "SIGINT",
            _ = on_term => "SIGTERM",
            _ = on_hup => "SIGHUP",
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
        use tracing::warn;

        let mut sig_c = match ctrl_c() {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "Failed to register Windows CTRL_C handler");
                None
            }
        };
        let mut sig_close = match ctrl_close() {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "Failed to register Windows CTRL_CLOSE handler");
                None
            }
        };
        let mut sig_logoff = match ctrl_logoff() {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "Failed to register Windows CTRL_LOGOFF handler");
                None
            }
        };
        let mut sig_shutdown = match ctrl_shutdown() {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error = %e, "Failed to register Windows CTRL_SHUTDOWN handler");
                None
            }
        };

        let on_c = async {
            if let Some(s) = sig_c.as_mut() {
                if s.recv().await.is_some() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        };
        let on_close = async {
            if let Some(s) = sig_close.as_mut() {
                if s.recv().await.is_some() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        };
        let on_logoff = async {
            if let Some(s) = sig_logoff.as_mut() {
                if s.recv().await.is_some() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        };
        let on_shutdown = async {
            if let Some(s) = sig_shutdown.as_mut() {
                if s.recv().await.is_some() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        };

        tokio::select! {
            _ = on_c => "CTRL_C",
            _ = on_close => "CTRL_CLOSE",
            _ = on_logoff => "CTRL_LOGOFF",
            _ = on_shutdown => "CTRL_SHUTDOWN",
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "Failed to await Ctrl+C signal");
            std::future::pending::<()>().await
        }
        "Ctrl+C"
    }
}

/// Platform-independent classification for Linux `systemd-logind` `PrepareForShutdown` /
/// `PrepareForSleep` signals (unit-tested on all platforms).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogindTransition {
    PreparingToShutdown,
    PreparingToSleep,
    Resumed,
}

#[allow(dead_code)]
pub(crate) fn classify_logind_signal(member: &str, active: bool) -> Option<LogindTransition> {
    match (member, active) {
        ("PrepareForShutdown", true) => Some(LogindTransition::PreparingToShutdown),
        ("PrepareForSleep", true) => Some(LogindTransition::PreparingToSleep),
        ("PrepareForShutdown", false) | ("PrepareForSleep", false) => {
            Some(LogindTransition::Resumed)
        }
        _ => None,
    }
}

/// Platform-independent transition tracker for Windows `WM_QUERYENDSESSION`, `WM_ENDSESSION`,
/// and `WM_POWERBROADCAST` events (unit-tested on all platforms).
#[allow(dead_code)]
#[derive(Debug, Default)]
pub(crate) struct WindowsLifecycleTracker {
    paused_for_shutdown: bool,
    last_resume_emit: Option<Instant>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WinLifecycleAction {
    EmitPreparingToShutdown,
    EmitPreparingToSleep,
    EmitResumed,
    Ignore,
}

#[allow(dead_code)]
const WINDOWS_RESUME_DEBOUNCE: Duration = Duration::from_secs(1);

#[allow(dead_code)]
impl WindowsLifecycleTracker {
    pub(crate) fn on_query_end_session(&mut self) -> WinLifecycleAction {
        self.paused_for_shutdown = true;
        self.last_resume_emit = None;
        WinLifecycleAction::EmitPreparingToShutdown
    }

    pub(crate) fn on_end_session(&mut self, ending: bool, now: Instant) -> WinLifecycleAction {
        if ending {
            if !self.paused_for_shutdown {
                self.paused_for_shutdown = true;
                self.last_resume_emit = None;
                WinLifecycleAction::EmitPreparingToShutdown
            } else {
                WinLifecycleAction::Ignore
            }
        } else if self.paused_for_shutdown {
            self.paused_for_shutdown = false;
            self.last_resume_emit = Some(now);
            WinLifecycleAction::EmitResumed
        } else {
            WinLifecycleAction::Ignore
        }
    }

    pub(crate) fn on_apm_suspend(&mut self) -> WinLifecycleAction {
        self.last_resume_emit = None;
        WinLifecycleAction::EmitPreparingToSleep
    }

    pub(crate) fn on_apm_resume(&mut self, now: Instant) -> WinLifecycleAction {
        self.paused_for_shutdown = false;
        let should_emit = self
            .last_resume_emit
            .is_none_or(|t| now.saturating_duration_since(t) >= WINDOWS_RESUME_DEBOUNCE);
        if should_emit {
            self.last_resume_emit = Some(now);
            WinLifecycleAction::EmitResumed
        } else {
            WinLifecycleAction::Ignore
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use tracing::debug;

    const LOGIND_DEST: &str = "org.freedesktop.login1";
    const LOGIND_PATH: &str = "/org/freedesktop/login1";
    const LOGIND_IFACE: &str = "org.freedesktop.login1.Manager";
    const INHIBIT_WHAT: &str = "shutdown:sleep";
    const INHIBIT_WHO: &str = "Glide";
    const INHIBIT_WHY: &str = "Notify remote KVM peers before shutdown or sleep";
    const INHIBIT_MODE: &str = "delay";

    pub(super) fn start_linux_lifecycle_monitor() -> SystemLifecycleMonitor {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            debug!("No active Tokio runtime; Linux systemd-logind lifecycle monitor disabled");
            return SystemLifecycleMonitor::disabled();
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let task = handle.spawn(async move {
            if let Err(e) = run_logind_monitor(tx).await {
                debug!(
                    error = %e,
                    "Linux systemd-logind lifecycle monitor unavailable; signal shutdown remains active"
                );
            }
        });

        SystemLifecycleMonitor {
            events: Some(rx),
            stop: Some(Box::new(move || {
                task.abort();
            })),
        }
    }

    // Intentionally polls via `zbus` 5's re-exported `futures_core::Stream` trait so Layer 1
    // introduces zero new Cargo dependencies (`futures-core` / `futures-util`).
    async fn next_signal_msg(stream: &mut zbus::proxy::SignalStream<'_>) -> Option<zbus::Message> {
        std::future::poll_fn(|cx| {
            zbus::export::futures_core::Stream::poll_next(std::pin::Pin::new(&mut *stream), cx)
        })
        .await
    }

    async fn acquire_logind_inhibitor(proxy: &zbus::Proxy<'_>) -> Option<std::os::fd::OwnedFd> {
        match proxy
            .call::<_, _, zbus::zvariant::OwnedFd>(
                "Inhibit",
                &(INHIBIT_WHAT, INHIBIT_WHO, INHIBIT_WHY, INHIBIT_MODE),
            )
            .await
        {
            Ok(zfd) => {
                debug!("Acquired systemd-logind unprivileged delay inhibitor (shutdown:sleep)");
                Some(std::os::fd::OwnedFd::from(zfd))
            }
            Err(e) => {
                debug!(error = %e, "systemd-logind delay inhibitor unavailable");
                None
            }
        }
    }

    async fn run_logind_monitor(
        tx: mpsc::UnboundedSender<SystemLifecycleEvent>,
    ) -> zbus::Result<()> {
        let conn = zbus::Connection::system().await?;
        let proxy = zbus::Proxy::new(&conn, LOGIND_DEST, LOGIND_PATH, LOGIND_IFACE).await?;

        // Subscribe to both signals BEFORE acquiring the delay inhibitor so no transition
        // can race between `Inhibit` and signal registration.
        let mut shutdown_stream = proxy.receive_signal("PrepareForShutdown").await?;
        let mut sleep_stream = proxy.receive_signal("PrepareForSleep").await?;

        if tx.is_closed() {
            return Ok(());
        }

        let mut inhibitor_fd = acquire_logind_inhibitor(&proxy).await;
        debug!("Subscribed to systemd-logind PrepareForShutdown and PrepareForSleep");

        loop {
            let (member, msg) = tokio::select! {
                _ = tx.closed() => break,
                maybe_msg = next_signal_msg(&mut shutdown_stream) => {
                    match maybe_msg {
                        Some(m) => ("PrepareForShutdown", m),
                        None => break,
                    }
                }
                maybe_msg = next_signal_msg(&mut sleep_stream) => {
                    match maybe_msg {
                        Some(m) => ("PrepareForSleep", m),
                        None => break,
                    }
                }
            };

            let Ok(active) = msg.body().deserialize::<bool>() else {
                continue;
            };
            let Some(transition) = classify_logind_signal(member, active) else {
                continue;
            };

            match transition {
                LogindTransition::PreparingToShutdown => {
                    let guard = ShutdownInhibitorGuard::from_opt_fd(inhibitor_fd.take());
                    if tx
                        .send(SystemLifecycleEvent::PreparingToShutdown(guard))
                        .is_err()
                    {
                        break;
                    }
                }
                LogindTransition::PreparingToSleep => {
                    let guard = ShutdownInhibitorGuard::from_opt_fd(inhibitor_fd.take());
                    if tx
                        .send(SystemLifecycleEvent::PreparingToSleep(guard))
                        .is_err()
                    {
                        break;
                    }
                }
                LogindTransition::Resumed => {
                    // Emit Resumed first so Glide begins reconnecting immediately without waiting
                    // for the D-Bus roundtrip to re-arm the next delay inhibitor.
                    if tx.send(SystemLifecycleEvent::Resumed).is_err() {
                        break;
                    }
                    if inhibitor_fd.is_none() {
                        inhibitor_fd = acquire_logind_inhibitor(&proxy).await;
                    }
                }
            }
        }

        drop(inhibitor_fd);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use parking_lot::{Condvar, Mutex};
    use std::collections::HashMap;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::thread;
    use tracing::{debug, warn};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, MSG,
        PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMECRITICAL, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
        WM_APP, WM_DESTROY, WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION, WNDCLASSW,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };

    const WM_APP_LIFECYCLE_STOP: u32 = WM_APP + 0x40;
    /// Upper bound that `WndProc` waits for `glide-core` to close & flush QUIC `CONNECTION_CLOSE`
    /// frames before returning to Windows (well below Windows' 2 s suspend / 5 s shutdown thresholds).
    const WINDOWS_INHIBITOR_WAIT: Duration = Duration::from_millis(200);

    struct WinLifecycleState {
        tx: mpsc::UnboundedSender<SystemLifecycleEvent>,
        tracker: Mutex<WindowsLifecycleTracker>,
        live_hwnd: Arc<AtomicIsize>,
    }

    static WINDOWS_MONITORS: OnceLock<Mutex<HashMap<isize, Arc<WinLifecycleState>>>> =
        OnceLock::new();

    fn monitors_map() -> &'static Mutex<HashMap<isize, Arc<WinLifecycleState>>> {
        WINDOWS_MONITORS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn lookup_state(hwnd: HWND) -> Option<Arc<WinLifecycleState>> {
        monitors_map().lock().get(&(hwnd as isize)).cloned()
    }

    fn dispatch_action(state: &WinLifecycleState, action: WinLifecycleAction) {
        match action {
            WinLifecycleAction::Ignore => {}
            WinLifecycleAction::EmitResumed => {
                let _ = state.tx.send(SystemLifecycleEvent::Resumed);
            }
            WinLifecycleAction::EmitPreparingToShutdown
            | WinLifecycleAction::EmitPreparingToSleep => {
                let done = Arc::new((Mutex::new(false), Condvar::new()));
                let done_c = Arc::clone(&done);
                let guard = ShutdownInhibitorGuard::with_on_drop(move || {
                    let (lock, cvar) = &*done_c;
                    *lock.lock() = true;
                    cvar.notify_all();
                });
                let ev = match action {
                    WinLifecycleAction::EmitPreparingToShutdown => {
                        SystemLifecycleEvent::PreparingToShutdown(guard)
                    }
                    WinLifecycleAction::EmitPreparingToSleep => {
                        SystemLifecycleEvent::PreparingToSleep(guard)
                    }
                    _ => unreachable!(),
                };
                if state.tx.send(ev).is_err() {
                    return;
                }
                let (lock, cvar) = &*done;
                let mut finished = lock.lock();
                let _ = cvar.wait_while_for(&mut finished, |done| !*done, WINDOWS_INHIBITOR_WAIT);
            }
        }
    }

    unsafe extern "system" fn lifecycle_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => {
                if let Some(state) = lookup_state(hwnd) {
                    let action = state.tracker.lock().on_query_end_session();
                    dispatch_action(&state, action);
                }
                1 // TRUE: allow shutdown / logoff to proceed cleanly
            }
            WM_ENDSESSION => {
                if let Some(state) = lookup_state(hwnd) {
                    let action = state
                        .tracker
                        .lock()
                        .on_end_session(wparam != 0, Instant::now());
                    dispatch_action(&state, action);
                }
                0
            }
            WM_POWERBROADCAST => match wparam as u32 {
                PBT_APMSUSPEND => {
                    if let Some(state) = lookup_state(hwnd) {
                        let action = state.tracker.lock().on_apm_suspend();
                        dispatch_action(&state, action);
                    }
                    1 // TRUE per Win32 WM_POWERBROADCAST specification
                }
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL => {
                    if let Some(state) = lookup_state(hwnd) {
                        let action = state.tracker.lock().on_apm_resume(Instant::now());
                        dispatch_action(&state, action);
                    }
                    1
                }
                _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            },
            WM_DESTROY => {
                if let Some(state) = monitors_map().lock().remove(&(hwnd as isize)) {
                    state.live_hwnd.store(0, Ordering::Release);
                }
                unsafe {
                    PostQuitMessage(0);
                }
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    pub(super) fn start_windows_lifecycle_monitor() -> SystemLifecycleMonitor {
        let (tx, rx) = mpsc::unbounded_channel();
        let live_hwnd = Arc::new(AtomicIsize::new(0));
        let thread_state = Arc::new(WinLifecycleState {
            tx,
            tracker: Mutex::new(WindowsLifecycleTracker::default()),
            live_hwnd: Arc::clone(&live_hwnd),
        });
        let stopped = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<isize>(1);

        let thread_stopped = Arc::clone(&stopped);
        let thread_live_hwnd = Arc::clone(&live_hwnd);
        let spawn_res = thread::Builder::new()
            .name("glide-win-lifecycle".into())
            .spawn(move || {
                let class_name: Vec<u16> = "GlideLifecycleMonitorWindow\0".encode_utf16().collect();
                let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
                let wc = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(lifecycle_wnd_proc),
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

                // Must be a hidden top-level window (NOT HWND_MESSAGE) to receive system-wide
                // WM_QUERYENDSESSION, WM_ENDSESSION, and WM_POWERBROADCAST notifications.
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
                    warn!("CreateWindowExW failed for Glide lifecycle window");
                    let _ = ready_tx.send(0);
                    return;
                }

                thread_live_hwnd.store(hwnd as isize, Ordering::Release);
                monitors_map().lock().insert(hwnd as isize, thread_state);
                let _ = ready_tx.send(hwnd as isize);

                if !thread_stopped.load(Ordering::Acquire) {
                    debug!("Windows hidden lifecycle window active");
                    let mut msg: MSG = unsafe { std::mem::zeroed() };
                    while !thread_stopped.load(Ordering::Acquire)
                        && unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0
                    {
                        if msg.hwnd == hwnd && msg.message == WM_APP_LIFECYCLE_STOP {
                            break;
                        }
                        unsafe {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                }

                let to_destroy = thread_live_hwnd.swap(0, Ordering::AcqRel) as HWND;
                monitors_map().lock().remove(&(hwnd as isize));
                if !to_destroy.is_null() {
                    unsafe {
                        DestroyWindow(to_destroy);
                    }
                }
            });

        let hwnd_raw = if spawn_res.is_ok() {
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap_or(0)
        } else {
            0
        };

        if hwnd_raw == 0 {
            stopped.store(true, Ordering::Release);
            return SystemLifecycleMonitor::disabled();
        }

        SystemLifecycleMonitor {
            events: Some(rx),
            stop: Some(Box::new(move || {
                stopped.store(true, Ordering::Release);
                let hwnd = live_hwnd.load(Ordering::Acquire) as HWND;
                if !hwnd.is_null() {
                    unsafe {
                        PostMessageW(hwnd, WM_APP_LIFECYCLE_STOP, 0, 0);
                    }
                }
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn shutdown_inhibitor_guard_runs_on_drop_exactly_once() {
        let released = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&released);
        let guard = ShutdownInhibitorGuard::with_on_drop(move || {
            let prev = r.swap(true, Ordering::SeqCst);
            assert!(!prev, "on_drop must only run once");
        });
        assert!(!released.load(Ordering::SeqCst));
        drop(guard);
        assert!(released.load(Ordering::SeqCst));

        // No-op guard drops cleanly without panicking.
        drop(ShutdownInhibitorGuard::noop());
    }

    #[test]
    fn classify_logind_signals_maps_shutdown_sleep_and_resume() {
        assert_eq!(
            classify_logind_signal("PrepareForShutdown", true),
            Some(LogindTransition::PreparingToShutdown)
        );
        assert_eq!(
            classify_logind_signal("PrepareForShutdown", false),
            Some(LogindTransition::Resumed)
        );
        assert_eq!(
            classify_logind_signal("PrepareForSleep", true),
            Some(LogindTransition::PreparingToSleep)
        );
        assert_eq!(
            classify_logind_signal("PrepareForSleep", false),
            Some(LogindTransition::Resumed)
        );
        assert_eq!(classify_logind_signal("SessionNew", true), None);
    }

    #[test]
    fn windows_lifecycle_tracker_handles_shutdown_cancel_sleep_and_resume_debounce() {
        let mut tracker = WindowsLifecycleTracker::default();
        let t0 = Instant::now();

        // Shutdown query pauses once; confirming WM_ENDSESSION(TRUE) does not double-emit.
        assert_eq!(
            tracker.on_query_end_session(),
            WinLifecycleAction::EmitPreparingToShutdown
        );
        assert_eq!(tracker.on_end_session(true, t0), WinLifecycleAction::Ignore);

        // User cancels shutdown -> WM_ENDSESSION(FALSE) resumes immediately.
        assert_eq!(
            tracker.on_end_session(false, t0 + Duration::from_millis(50)),
            WinLifecycleAction::EmitResumed
        );
        // Duplicate WM_ENDSESSION(FALSE) is ignored.
        assert_eq!(
            tracker.on_end_session(false, t0 + Duration::from_millis(60)),
            WinLifecycleAction::Ignore
        );

        // Direct WM_ENDSESSION(TRUE) without prior WM_QUERYENDSESSION still emits shutdown.
        assert_eq!(
            tracker.on_end_session(true, t0 + Duration::from_millis(100)),
            WinLifecycleAction::EmitPreparingToShutdown
        );
        assert_eq!(
            tracker.on_end_session(false, t0 + Duration::from_millis(150)),
            WinLifecycleAction::EmitResumed
        );

        // Sleep clears resume debounce; rapid PBT_APMRESUMEAUTOMATIC + PBT_APMRESUMESUSPEND emits once.
        assert_eq!(
            tracker.on_apm_suspend(),
            WinLifecycleAction::EmitPreparingToSleep
        );
        let wake = t0 + Duration::from_millis(200);
        assert_eq!(tracker.on_apm_resume(wake), WinLifecycleAction::EmitResumed);
        assert_eq!(
            tracker.on_apm_resume(wake + Duration::from_millis(20)),
            WinLifecycleAction::Ignore
        );
        // After debounce window or after another suspend, resume emits again.
        assert_eq!(
            tracker.on_apm_suspend(),
            WinLifecycleAction::EmitPreparingToSleep
        );
        assert_eq!(
            tracker.on_apm_resume(wake + Duration::from_millis(100)),
            WinLifecycleAction::EmitResumed
        );
    }

    #[tokio::test]
    async fn monitor_drop_releases_buffered_inhibitor_guard() {
        let (monitor, tx) = SystemLifecycleMonitor::from_channel();
        let released = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&released);
        tx.send(SystemLifecycleEvent::PreparingToSleep(
            ShutdownInhibitorGuard::with_on_drop(move || {
                r.store(true, Ordering::SeqCst);
            }),
        ))
        .unwrap();

        assert!(!released.load(Ordering::SeqCst));
        drop(monitor);
        assert!(
            released.load(Ordering::SeqCst),
            "dropping SystemLifecycleMonitor must release any unread inhibitor guard"
        );
    }

    #[tokio::test]
    async fn disabled_monitor_reports_inactive_and_returns_none() {
        let mut mon = SystemLifecycleMonitor::disabled();
        assert!(!mon.is_active());
        assert!(mon.try_recv().is_none());
        assert!(mon.recv().await.is_none());
    }
}
