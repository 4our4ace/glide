//! Glide desktop GUI + native system tray.
//!
//! The window is a native, GPU-rendered Rust UI (Rune: wgpu + winit) that looks identical on
//! Windows 11 and Ubuntu X11, with no browser or WebView. A loopback-only HTTP control server
//! remains for single-instance handoff (a second launch focuses the running window) and as a
//! last-resort browser fallback if the machine cannot create a GPU window at all.

use anyhow::{Context, Result};
use bytes::Bytes;
use glide_backend::icon::{render_png, GLIDE_ICON_SVG};
use glide_backend::os_integration::{
    configure_elevated_task, configure_firewall, is_autostart_enabled, set_autostart,
};
use glide_core::client::run_native_client_with_runtime;
use glide_core::config::{default_config_dir, GlideConfigFile};
use glide_core::runtime::{RuntimeCommand, RuntimeState};
use glide_core::server::run_native_server_with_runtime;
use glide_net::discovery::{discover_servers, local_ipv4_addresses, machine_hostname};
use glide_net::tls::ServerIdentity;
use glide_proto::{Edge, MAX_FILE_BYTES};
use parking_lot::Mutex;
use serde_json::json;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

mod native_ui;
pub(crate) mod updater;

const UI_HTML: &str = include_str!("ui.html");
const FONT_INTER: &[u8] = include_bytes!("../assets/fonts/inter.woff2");
const FONT_MONO: &[u8] = include_bytes!("../assets/fonts/jetbrains-mono.woff2");
pub const DEFAULT_GUI_PORT: u16 = 24802;
pub const GUI_WIDTH: u32 = 900;
pub const GUI_HEIGHT: u32 = 640;

type Hook = Arc<dyn Fn() + Send + Sync>;
type TrayHook = Arc<dyn Fn(bool) + Send + Sync>;

struct ActiveEngine {
    task: JoinHandle<()>,
    cmd_tx: mpsc::UnboundedSender<RuntimeCommand>,
}

struct AppController {
    config_path: PathBuf,
    config: Mutex<GlideConfigFile>,
    runtime: RuntimeState,
    engine: Mutex<Option<ActiveEngine>>,
    fingerprint: Mutex<Option<String>>,
    url: String,
    favicon_png: OnceLock<Vec<u8>>,
    tray_active: AtomicBool,
    quit_tx: mpsc::UnboundedSender<()>,
    /// Shows the native window (installed by `native_ui` while its event loop runs).
    ui_show: Mutex<Option<Hook>>,
    /// Quits the native window's event loop.
    ui_quit: Mutex<Option<Hook>>,
    /// Notifies the native UI when system-tray availability changes asynchronously.
    ui_tray: Mutex<Option<TrayHook>>,
}

impl AppController {
    fn new(config_path: PathBuf, url: String, quit_tx: mpsc::UnboundedSender<()>) -> Self {
        let cfg = if config_path.exists() {
            GlideConfigFile::load(&config_path).unwrap_or_default()
        } else {
            GlideConfigFile::default()
        };

        let runtime = RuntimeState::new();
        runtime.set_record_history(cfg.clipboard_history);

        Self {
            config_path,
            config: Mutex::new(cfg),
            runtime,
            engine: Mutex::new(None),
            fingerprint: Mutex::new(None),
            url,
            favicon_png: OnceLock::new(),
            tray_active: AtomicBool::new(false),
            quit_tx,
            ui_show: Mutex::new(None),
            ui_quit: Mutex::new(None),
            ui_tray: Mutex::new(None),
        }
    }

    /// Loads or generates the persistent Ed25519 TLS identity on demand (safe to call off the UI thread).
    fn load_identity(&self) -> Option<String> {
        if let Some(existing) = self.fingerprint.lock().clone() {
            return Some(existing);
        }
        let id_dir = self.config.lock().server.resolved_identity_dir();
        let fp = ServerIdentity::load_or_generate(&id_dir)
            .ok()
            .map(|id| id.fingerprint);
        if let Some(ref f) = fp {
            *self.fingerprint.lock() = Some(f.clone());
            self.runtime.update(|s| {
                s.fingerprint = Some(f.clone());
            });
        }
        fp
    }

    #[cfg(target_os = "linux")]
    fn notify_tray_changed(&self, active: bool) {
        let hook = self.ui_tray.lock().clone();
        if let Some(cb) = hook {
            cb(active);
        }
    }

    /// Brings the Glide window to the front: the native window when it runs, else the
    /// browser fallback.
    fn show_window(&self) {
        let hook = self.ui_show.lock().clone();
        match hook {
            Some(show) => show(),
            None => {
                let _ = launch_browser_window(&self.url);
            }
        }
    }

    fn quit_ui(&self) {
        let hook = self.ui_quit.lock().clone();
        if let Some(quit) = hook {
            quit();
        }
    }

    async fn stop_engine(&self) {
        let active = self.engine.lock().take();
        if let Some(active) = active {
            let _ = active.cmd_tx.send(RuntimeCommand::Shutdown);
            // Wait up to 400ms for sockets (UDP 24800/24801) and hooks to release cleanly
            let mut task = active.task;
            if tokio::time::timeout(Duration::from_millis(400), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        self.runtime.update(|s| {
            s.mode = "stopped".into();
            s.status = "Idle".into();
            s.active_target = "Local".into();
            s.locked = false;
            s.rtt_ms = None;
            s.rtt_history.clear();
            s.peers.clear();
        });
    }

    async fn start_engine(&self, role: &str) -> Result<()> {
        if role != "server" && role != "client" {
            anyhow::bail!("unknown role '{role}'");
        }
        self.stop_engine().await;
        let cfg = {
            let mut guard = self.config.lock();
            guard.last_role = Some(role.to_owned());
            guard.auto_start_on_launch = true;
            guard.clone()
        };
        let _ = cfg.save(&self.config_path);

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let rt = self.runtime.clone();

        let task = match role {
            "server" => {
                let scfg = cfg.server;
                let rt_err = rt.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        run_native_server_with_runtime(scfg, Some(rt), Some(cmd_rx)).await
                    {
                        warn!(error = %e, "Glide server exited with error");
                        rt_err.update(|s| {
                            s.mode = "stopped".into();
                            s.status = "Error".into();
                            s.last_error = Some(e.to_string());
                        });
                    }
                })
            }
            "client" => {
                let ccfg = cfg.client;
                let rt_err = rt.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        run_native_client_with_runtime(ccfg, Some(rt), Some(cmd_rx)).await
                    {
                        warn!(error = %e, "Glide client exited with error");
                        rt_err.update(|s| {
                            s.mode = "stopped".into();
                            s.status = "Error".into();
                            s.last_error = Some(e.to_string());
                        });
                    }
                })
            }
            other => anyhow::bail!("unknown role '{other}'"),
        };

        *self.engine.lock() = Some(ActiveEngine { task, cmd_tx });
        Ok(())
    }

    fn send_cmd(&self, cmd: RuntimeCommand) -> bool {
        if let Some(ref eng) = *self.engine.lock() {
            eng.cmd_tx.send(cmd).is_ok()
        } else {
            false
        }
    }

    fn apply_config(&self, mut new_cfg: GlideConfigFile) -> Result<()> {
        {
            let prev = self.config.lock();
            if new_cfg.last_role.is_none() {
                new_cfg.last_role = prev.last_role.clone();
            }
            if !new_cfg.auto_start_on_launch && prev.auto_start_on_launch {
                new_cfg.auto_start_on_launch = true;
            }
        }
        new_cfg.save(&self.config_path)?;
        self.runtime.set_record_history(new_cfg.clipboard_history);
        let s = new_cfg.server.clone();
        let c = new_cfg.client.clone();
        *self.config.lock() = new_cfg;

        let locked = self.runtime.read(|s| s.locked);
        self.send_cmd(RuntimeCommand::UpdateServerTuning {
            edge_threshold_px: s.edge_threshold_px,
            dead_corner_px: s.dead_corner_px,
            edge_dwell_ms: s.edge_dwell_ms,
            auto_lock_fullscreen: s.auto_lock_fullscreen,
            locked,
            sync_lock_keys: s.sync_lock_keys,
            clipboard: s.clipboard,
        });
        self.send_cmd(RuntimeCommand::UpdateClientTuning {
            sensitivity: c.sensitivity,
            scroll_speed: c.scroll_speed,
            invert_scroll: c.invert_scroll,
            auto_dpi_scale: c.auto_dpi_scale,
            edge_threshold_px: c.edge_threshold_px,
            dead_corner_px: c.dead_corner_px,
            sync_lock_keys: c.sync_lock_keys,
            clipboard: c.clipboard,
            key_remap: c.key_remap,
        });
        Ok(())
    }
}

/// Options for launching the Glide desktop GUI.
#[derive(Debug, Clone)]
pub struct GuiOptions {
    pub config_path: Option<PathBuf>,
    pub port: u16,
    pub minimized: bool,
    pub auto_start_role: Option<String>,
}

impl Default for GuiOptions {
    fn default() -> Self {
        Self {
            config_path: None,
            port: DEFAULT_GUI_PORT,
            minimized: false,
            auto_start_role: None,
        }
    }
}

/// If Glide is already running on `port`, ask the running instance to show/focus its window.
fn try_focus_existing_instance(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = StdTcpStream::connect_timeout(&addr, Duration::from_millis(200)) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let req = format!(
        "POST /api/show HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut resp = [0u8; 64];
    match stream.read(&mut resp) {
        Ok(n) if n > 0 => String::from_utf8_lossy(&resp[..n]).contains("200 OK"),
        _ => false,
    }
}

/// Binds the loopback control port synchronously before Tokio or GPU initialization.
/// Returns `Ok(None)` if an existing Glide instance took over and was focused.
fn bind_or_focus_existing(port: u16, minimized: bool) -> Result<Option<std::net::TcpListener>> {
    // Bind first: in the common case (no other instance) this succeeds in ~50µs and we
    // skip the single-instance probe entirely.
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(_) => {
            if port != 0 && !minimized && try_focus_existing_instance(port) {
                info!(port, "Focused existing Glide GUI instance");
                return Ok(None);
            }
            std::net::TcpListener::bind(("127.0.0.1", 0))
                .context("failed to bind local GUI port")?
        }
    };
    listener
        .set_nonblocking(true)
        .context("failed to configure local GUI listener")?;
    Ok(Some(listener))
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn spawn_tray_listener(
    ctrl: Arc<AppController>,
    mut tray_rx: mpsc::UnboundedReceiver<glide_backend::TrayAction>,
) {
    tokio::spawn(async move {
        use glide_backend::TrayAction;
        while let Some(act) = tray_rx.recv().await {
            match act {
                TrayAction::OpenUi => ctrl.show_window(),
                TrayAction::ToggleLock => {
                    ctrl.send_cmd(RuntimeCommand::ToggleLock);
                }
                TrayAction::Quit => {
                    ctrl.stop_engine().await;
                    let _ = ctrl.quit_tx.send(());
                    break;
                }
            }
        }
    });
}

/// Serves the loopback control API until a quit request (tray, `/api/quit`, the window, Ctrl+C),
/// then asks the native window to close.
async fn serve(
    listener: TcpListener,
    ctrl: Arc<AppController>,
    mut quit_rx: mpsc::UnboundedReceiver<()>,
) {
    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _peer)) => {
                        let c = Arc::clone(&ctrl);
                        tokio::spawn(async move {
                            if let Err(e) = handle_http_conn(stream, c).await {
                                debug!(error = %e, "GUI HTTP connection ended");
                            }
                        });
                    }
                    Err(e) => warn!(error = %e, "GUI accept error"),
                }
            }
            _ = quit_rx.recv() => {
                info!("Exiting Glide GUI");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down Glide GUI (Ctrl+C)");
                break;
            }
        }
    }
    ctrl.quit_ui();
}

/// Runs the Glide desktop GUI: native window on the calling (main) thread, engine, control
/// server and tray on a Tokio runtime. Returns when the user quits.
pub fn run_gui_blocking(opts: GuiOptions) -> Result<()> {
    let launch_t0 = std::time::Instant::now();
    glide_backend::init_platform();

    let Some(std_listener) = bind_or_focus_existing(opts.port, opts.minimized)? else {
        return Ok(());
    };

    let headless_snap = std::env::var_os("GLIDE_RUNE_ARGS").is_some();

    // Start creating the GPU device and compiling Rune's render pipeline on a worker thread
    // immediately — BEFORE Tokio runtime, config load, and winit window creation — so they
    // overlap instead of running back-to-back.
    // On Windows, Rune opens immediately on its CPU boot renderer and hands off to DX12 in
    // the background, so warming GPU up front is only needed on Linux and macOS.
    if !opts.minimized && !headless_snap {
        #[cfg(target_os = "linux")]
        rune::warm_gpu(Default::default());
        #[cfg(target_os = "macos")]
        rune::prewarm_gpu();
    }

    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .thread_name("glide-rt")
        .build()
        .context("failed to start async runtime")?;

    let listener = {
        let _guard = rt.enter();
        TcpListener::from_std(std_listener).context("failed to register local GUI listener")?
    };
    let actual_addr = listener.local_addr()?;
    let url = format!("http://127.0.0.1:{}", actual_addr.port());

    let (quit_tx, quit_rx) = mpsc::unbounded_channel::<()>();
    let cfg_path = opts
        .config_path
        .clone()
        .unwrap_or_else(|| default_config_dir().join("glide.toml"));
    let ctrl = Arc::new(AppController::new(cfg_path, url.clone(), quit_tx));

    let effective_auto_start = if headless_snap {
        None
    } else if let Some(role) = opts.auto_start_role {
        if role == "server" || role == "client" {
            ctrl.config.lock().last_role = Some(role.clone());
        }
        Some(role)
    } else {
        let cfg = ctrl.config.lock();
        if cfg.auto_start_on_launch {
            cfg.last_role.clone()
        } else {
            None
        }
    };

    if let Some(role) = effective_auto_start {
        let ctrl_auto = Arc::clone(&ctrl);
        rt.spawn(async move {
            if let Err(e) = ctrl_auto.start_engine(&role).await {
                warn!(error = %e, "Failed auto-starting Glide engine");
            }
        });
    }

    // Native OS System Tray integration runs asynchronously so D-Bus / icon setup never
    // blocks the window from appearing.
    #[cfg(target_os = "windows")]
    if !headless_snap {
        ctrl.tray_active.store(true, Ordering::SeqCst);
        let ctrl_tray = Arc::clone(&ctrl);
        rt.spawn(async move {
            let tray_rx = glide_backend::windows::tray::spawn_windows_tray();
            spawn_tray_listener(ctrl_tray, tray_rx);
        });
    }

    #[cfg(target_os = "linux")]
    let linux_tray_slot: Arc<
        tokio::sync::Mutex<Option<glide_backend::linux_x11::tray::LinuxTrayHandle>>,
    > = Arc::new(tokio::sync::Mutex::new(None));
    #[cfg(target_os = "linux")]
    let linux_tray_task = if !headless_snap {
        let ctrl_tray = Arc::clone(&ctrl);
        let slot = Arc::clone(&linux_tray_slot);
        Some(rt.spawn(async move {
            let (handle, tray_rx) = glide_backend::linux_x11::tray::spawn_linux_tray().await;
            let active = handle.is_active();
            ctrl_tray.tray_active.store(active, Ordering::SeqCst);
            ctrl_tray.notify_tray_changed(active);
            spawn_tray_listener(ctrl_tray, tray_rx);
            *slot.lock().await = Some(handle);
        }))
    } else {
        None
    };

    info!(%url, "Glide GUI ready");

    let mut server = Some(rt.spawn(serve(listener, Arc::clone(&ctrl), quit_rx)));

    if let Err(e) = native_ui::run(
        Arc::clone(&ctrl),
        rt.handle().clone(),
        opts.minimized,
        launch_t0,
    ) {
        // No display or no usable GPU: keep Glide usable through the browser fallback.
        warn!(error = %e, "Native window unavailable; using the browser fallback");
        *ctrl.ui_show.lock() = None;
        *ctrl.ui_quit.lock() = None;
        *ctrl.ui_tray.lock() = None;
        if !opts.minimized {
            let child = launch_browser_window(&ctrl.url);
            if !ctrl.tray_active.load(Ordering::SeqCst) {
                if let Some(mut child) = child {
                    let quit_tx = ctrl.quit_tx.clone();
                    std::thread::spawn(move || {
                        let _ = child.wait();
                        let _ = quit_tx.send(());
                    });
                }
            }
        }
        if let Some(task) = server.take() {
            let _ = rt.block_on(task);
        }
    }

    let _ = ctrl.quit_tx.send(());
    rt.block_on(async {
        if let Some(task) = server.take() {
            let _ = tokio::time::timeout(Duration::from_millis(500), task).await;
        }
        ctrl.stop_engine().await;
        #[cfg(target_os = "linux")]
        {
            if let Some(task) = linux_tray_task {
                let _ = tokio::time::timeout(Duration::from_millis(300), task).await;
            }
            if let Some(handle) = linux_tray_slot.lock().await.take() {
                handle.shutdown().await;
            }
        }
    });
    rt.shutdown_timeout(Duration::from_millis(300));
    Ok(())
}

/// Launches the UI in a Chromium-family browser `--app` window with an isolated profile.
pub(crate) fn launch_browser_window(url: &str) -> Option<std::process::Child> {
    #[cfg(target_os = "linux")]
    if !glide_backend::linux_wayland::is_wayland_session()
        && glide_backend::linux_x11::window_brand::focus_existing_glide_window()
    {
        return None;
    }

    let profile_dir = default_config_dir().join("ui-profile");
    let _ = std::fs::create_dir_all(&profile_dir);
    let profile_arg = format!("--user-data-dir={}", profile_dir.display());
    let app_arg = format!("--app={url}");
    let size_arg = format!("--window-size={GUI_WIDTH},{GUI_HEIGHT}");

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let candidates = [
            "msedge",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            "chrome",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        ];
        for bin in candidates {
            if let Ok(child) = Command::new(bin)
                .creation_flags(CREATE_NO_WINDOW)
                .args([
                    &app_arg,
                    &size_arg,
                    &profile_arg,
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--disable-extensions",
                    "--disable-background-networking",
                    "--disable-sync",
                    "--disable-component-update",
                ])
                .spawn()
            {
                return Some(child);
            }
        }
        if glide_backend::os_integration::open_loopback_url(url).is_err() {
            let _ = glide_backend::os_integration::open_url(url);
        }
        None
    }

    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        for bin in [
            "google-chrome",
            "google-chrome-stable",
            "chromium-browser",
            "chromium",
            "brave-browser",
            "microsoft-edge",
            "vivaldi",
        ] {
            if let Ok(child) = Command::new(bin)
                .args([
                    &app_arg,
                    &size_arg,
                    &profile_arg,
                    "--class=Glide",
                    "--name=Glide",
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--disable-extensions",
                    "--disable-infobars",
                    "--disable-features=TranslateUI",
                ])
                .spawn()
            {
                if !glide_backend::linux_wayland::is_wayland_session() {
                    glide_backend::linux_x11::window_brand::spawn_x11_window_brander(
                        GUI_WIDTH, GUI_HEIGHT,
                    );
                }
                return Some(child);
            }
        }
        if glide_backend::os_integration::open_loopback_url(url).is_err() {
            let _ = glide_backend::os_integration::open_url(url);
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        if glide_backend::os_integration::open_loopback_url(url).is_err() {
            let _ = glide_backend::os_integration::open_url(url);
        }
        None
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(s) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(s, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const MAX_CONTROL_BODY_BYTES: usize = 1024 * 1024;
const HTTP_IO_TIMEOUT: Duration = Duration::from_secs(15);

fn is_allowed_host(host: &str, port: u16) -> bool {
    let host = host.trim();
    let Some((name, p)) = host.rsplit_once(':') else {
        return false;
    };
    if p != port.to_string() {
        return false;
    }
    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost") || name == "[::1]"
}

fn is_allowed_origin(origin: &str, port: u16) -> bool {
    let origin = origin.trim();
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("HTTP://"))
    else {
        return false;
    };
    is_allowed_host(rest, port)
}

fn is_allowed_fetch_site(val: &str) -> bool {
    let v = val.trim();
    v.eq_ignore_ascii_case("same-origin") || v.eq_ignore_ascii_case("none")
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let hdr = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Referrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    tokio::time::timeout(HTTP_IO_TIMEOUT, async {
        stream.write_all(hdr.as_bytes()).await?;
        stream.write_all(body).await?;
        Ok::<(), std::io::Error>(())
    })
    .await
    .context("HTTP response write timed out")??;
    Ok(())
}

async fn handle_http_conn(mut stream: TcpStream, ctrl: Arc<AppController>) -> Result<()> {
    let local_port = stream
        .local_addr()
        .context("failed to read local HTTP socket address")?
        .port();

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = match tokio::time::timeout(HTTP_IO_TIMEOUT, async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok::<Option<usize>, std::io::Error>(None);
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(Some(pos));
            }
            if buf.len() > 64 * 1024 {
                return Ok(Some(usize::MAX));
            }
        }
    })
    .await
    {
        Ok(Ok(Some(usize::MAX))) => {
            write_response(
                &mut stream,
                "431 Request Header Fields Too Large",
                "text/plain",
                b"headers too large",
            )
            .await?;
            return Ok(());
        }
        Ok(Ok(Some(pos))) => pos,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            let _ = write_response(
                &mut stream,
                "408 Request Timeout",
                "text/plain",
                b"request timeout",
            )
            .await;
            return Ok(());
        }
    };

    let header_str = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_str.lines();
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    let mut content_len: usize = 0;
    let mut content_len_count: usize = 0;
    let mut content_len_invalid = false;
    let mut has_transfer_encoding = false;
    let mut x_filename: Option<String> = None;
    let mut host_ok = false;
    let mut host_count: usize = 0;
    let mut origin_ok = true;
    let mut origin_count: usize = 0;
    let mut fetch_site_ok = true;
    let mut fetch_site_count: usize = 0;

    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "content-length" => {
                    content_len_count += 1;
                    match val.parse::<usize>() {
                        Ok(n) => content_len = n,
                        Err(_) => content_len_invalid = true,
                    }
                }
                "transfer-encoding" => has_transfer_encoding = true,
                "x-filename" => x_filename = Some(percent_decode(val)),
                "host" => {
                    host_count += 1;
                    host_ok = is_allowed_host(val, local_port);
                }
                "origin" => {
                    origin_count += 1;
                    origin_ok = origin_ok && is_allowed_origin(val, local_port);
                }
                "sec-fetch-site" => {
                    fetch_site_count += 1;
                    fetch_site_ok = fetch_site_ok && is_allowed_fetch_site(val);
                }
                _ => {}
            }
        }
    }

    if host_count != 1 || !host_ok {
        write_response(&mut stream, "403 Forbidden", "text/plain", b"loopback only").await?;
        return Ok(());
    }

    if method != "GET"
        && method != "HEAD"
        && (origin_count > 1 || !origin_ok || fetch_site_count > 1 || !fetch_site_ok)
    {
        write_response(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            b"cross-origin request blocked",
        )
        .await?;
        return Ok(());
    }

    if has_transfer_encoding || content_len_count > 1 || content_len_invalid {
        write_response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"invalid framing",
        )
        .await?;
        return Ok(());
    }

    let max_body_bytes = if method == "POST" && path == "/api/send-file" {
        MAX_FILE_BYTES
    } else {
        MAX_CONTROL_BODY_BYTES
    };

    if content_len > max_body_bytes {
        write_response(
            &mut stream,
            "413 Payload Too Large",
            "text/plain",
            b"payload too large",
        )
        .await?;
        return Ok(());
    }

    let body_start = header_end + 4;
    let mut body = Vec::with_capacity(content_len.min(64 * 1024));
    if buf.len() > body_start {
        let take = (buf.len() - body_start).min(content_len);
        body.extend_from_slice(&buf[body_start..body_start + take]);
    }
    if body.len() < content_len {
        let mut tmp = vec![0u8; 64 * 1024];
        let read_res = tokio::time::timeout(HTTP_IO_TIMEOUT, async {
            while body.len() < content_len {
                let to_read = (content_len - body.len()).min(tmp.len());
                let n = stream.read(&mut tmp[..to_read]).await?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            Ok::<(), std::io::Error>(())
        })
        .await;
        match read_res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                let _ = write_response(
                    &mut stream,
                    "408 Request Timeout",
                    "text/plain",
                    b"body read timeout",
                )
                .await;
                return Ok(());
            }
        }
    }
    if body.len() < content_len {
        write_response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"truncated request body",
        )
        .await?;
        return Ok(());
    }
    body.truncate(content_len);

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            let html = UI_HTML.replace("__GLIDE_VERSION__", env!("CARGO_PKG_VERSION"));
            write_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                html.as_bytes(),
            )
            .await?;
        }

        ("GET", "/favicon.svg") => {
            write_response(
                &mut stream,
                "200 OK",
                "image/svg+xml",
                GLIDE_ICON_SVG.as_bytes(),
            )
            .await?;
        }

        ("GET", "/favicon.png") | ("GET", "/favicon.ico") => {
            let png = ctrl.favicon_png.get_or_init(|| render_png(64));
            write_response(&mut stream, "200 OK", "image/png", png).await?;
        }

        // Bundled UI fonts (SIL OFL 1.1) so the GUI looks identical on every OS, offline.
        ("GET", "/fonts/inter.woff2") => {
            write_response(&mut stream, "200 OK", "font/woff2", FONT_INTER).await?;
        }
        ("GET", "/fonts/mono.woff2") => {
            write_response(&mut stream, "200 OK", "font/woff2", FONT_MONO).await?;
        }

        ("GET", "/api/events") => {
            let hdr = "HTTP/1.1 200 OK\r\n\
                       Content-Type: text/event-stream\r\n\
                       Cache-Control: no-cache\r\n\
                       X-Content-Type-Options: nosniff\r\n\
                       X-Frame-Options: DENY\r\n\
                       Referrer-Policy: no-referrer\r\n\
                       Connection: keep-alive\r\n\r\n";
            tokio::time::timeout(HTTP_IO_TIMEOUT, stream.write_all(hdr.as_bytes()))
                .await
                .context("SSE header write timed out")??;
            let mut rx = ctrl.runtime.subscribe();
            let initial = serde_json::to_string(&ctrl.runtime.snapshot())?;
            tokio::time::timeout(
                HTTP_IO_TIMEOUT,
                stream.write_all(format!("data: {initial}\n\n").as_bytes()),
            )
            .await
            .context("SSE initial write timed out")??;
            loop {
                tokio::select! {
                    res = rx.recv() => {
                        match res {
                            Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let snap = serde_json::to_string(&ctrl.runtime.snapshot())?;
                                let wrote = tokio::time::timeout(
                                    HTTP_IO_TIMEOUT,
                                    stream.write_all(format!("data: {snap}\n\n").as_bytes()),
                                )
                                .await;
                                if !matches!(wrote, Ok(Ok(()))) {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(15)) => {
                        let wrote = tokio::time::timeout(
                            HTTP_IO_TIMEOUT,
                            stream.write_all(b": keepalive\n\n"),
                        )
                        .await;
                        if !matches!(wrote, Ok(Ok(()))) {
                            break;
                        }
                    }
                }
            }
        }

        ("GET", "/api/state") => {
            let cfg = ctrl.config.lock().clone();
            let fp = ctrl.load_identity();
            let doctor = glide_backend::run_doctor().ok();
            let payload = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "config": cfg,
                "status": ctrl.runtime.snapshot(),
                "fingerprint": fp,
                "autostart": is_autostart_enabled(),
                "hostname": machine_hostname(),
                "local_ips": local_ipv4_addresses(),
                "os": format!("{:?}", glide_backend::current_os_kind()),
                "tray_active": ctrl.tray_active.load(Ordering::SeqCst),
                "doctor": doctor,
            });
            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                payload.to_string().as_bytes(),
            )
            .await?;
        }

        ("GET", "/api/discover") => {
            let port = ctrl.config.lock().client.discovery_port;
            let my_fp = ctrl.fingerprint.lock().clone();
            let found = discover_servers(port, Duration::from_millis(2200), None, false)
                .await
                .unwrap_or_default();
            let list: Vec<_> = found
                .into_iter()
                .filter(|s| my_fp.as_deref() != Some(s.fingerprint.as_str()))
                .map(|s| {
                    json!({
                        "name": s.name,
                        "os": s.os,
                        "addr": s.addr.to_string(),
                        "fingerprint": s.fingerprint,
                        "ips": s.all_ips,
                    })
                })
                .collect();
            let body = json!({ "servers": list, "local_ips": local_ipv4_addresses() }).to_string();
            write_response(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
        }

        ("POST", "/api/config") => match serde_json::from_slice::<GlideConfigFile>(&body) {
            Ok(mut new_cfg) => {
                // Never allow the HTTP control API to repoint TLS identity or TOFU store paths.
                {
                    let prev = ctrl.config.lock();
                    new_cfg.server.identity_dir = prev.server.identity_dir.clone();
                    new_cfg.client.tofu_store_path = prev.client.tofu_store_path.clone();
                }
                match ctrl.apply_config(new_cfg) {
                    Ok(()) => {
                        write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#)
                            .await?
                    }
                    Err(e) => {
                        let msg = json!({ "error": e.to_string() }).to_string();
                        write_response(
                            &mut stream,
                            "500 Internal Server Error",
                            "application/json",
                            msg.as_bytes(),
                        )
                        .await?;
                    }
                }
            }
            Err(e) => {
                let msg = json!({ "error": e.to_string() }).to_string();
                write_response(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    msg.as_bytes(),
                )
                .await?;
            }
        },

        ("POST", "/api/start") => {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let role = req["role"].as_str().unwrap_or("server");
            match ctrl.start_engine(role).await {
                Ok(()) => {
                    write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#)
                        .await?
                }
                Err(e) => {
                    let msg = json!({ "error": e.to_string() }).to_string();
                    write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "application/json",
                        msg.as_bytes(),
                    )
                    .await?;
                }
            }
        }

        ("POST", "/api/stop") => {
            ctrl.stop_engine().await;
            write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#).await?;
        }

        ("POST", "/api/lock") => {
            ctrl.send_cmd(RuntimeCommand::ToggleLock);
            write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#).await?;
        }

        ("POST", "/api/move-edge") => {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let edge = match req["edge"].as_str() {
                Some("left") => Some(Edge::Left),
                Some("right") => Some(Edge::Right),
                Some("top") => Some(Edge::Top),
                Some("bottom") => Some(Edge::Bottom),
                _ => None,
            };
            let peer_name = req["peer_name"].as_str().map(str::to_string);
            if let Some(edge) = edge {
                {
                    let mut cfg = ctrl.config.lock();
                    cfg.client.edge = edge;
                    let _ = cfg.save(&ctrl.config_path);
                }
                ctrl.send_cmd(RuntimeCommand::MovePeerEdge { peer_name, edge });
                write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#)
                    .await?;
            } else {
                write_response(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    br#"{"error":"invalid edge"}"#,
                )
                .await?;
            }
        }

        ("POST", "/api/switch") => {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let edge = match req["edge"].as_str() {
                Some("left") => Some(Edge::Left),
                Some("right") => Some(Edge::Right),
                Some("top") => Some(Edge::Top),
                Some("bottom") => Some(Edge::Bottom),
                _ => None,
            };
            if let Some(e) = edge {
                ctrl.send_cmd(RuntimeCommand::SwitchToEdge(e));
            } else {
                ctrl.send_cmd(RuntimeCommand::ReturnLocal);
            }
            write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#).await?;
        }

        ("POST", "/api/clipboard/resend") => {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            if let Some(id) = req["id"].as_u64() {
                ctrl.send_cmd(RuntimeCommand::ResendClipboard(id as usize));
            }
            write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#).await?;
        }

        ("POST", "/api/send-file") => {
            let raw_name = x_filename.unwrap_or_else(|| "glide-transfer.bin".into());
            let filename = glide_proto::sanitize_filename(&raw_name);
            if ctrl.send_cmd(RuntimeCommand::SendFile {
                filename,
                data: Bytes::from(body),
            }) {
                write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#)
                    .await?;
            } else {
                write_response(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    br#"{"error":"Glide is not currently running"}"#,
                )
                .await?;
            }
        }

        ("POST", "/api/os/autostart") => {
            let req: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    let msg = json!({ "error": e.to_string() }).to_string();
                    write_response(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        msg.as_bytes(),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let enabled = req["enabled"].as_bool().unwrap_or(false);
            let role = match req.get("role") {
                None => "client",
                Some(v) => match v.as_str() {
                    Some("server") => "server",
                    Some("client") => "client",
                    _ => {
                        write_response(
                            &mut stream,
                            "400 Bad Request",
                            "application/json",
                            br#"{"error":"role must be 'server' or 'client'"}"#,
                        )
                        .await?;
                        return Ok(());
                    }
                },
            };
            let args = format!("gui --minimized --auto-start {role}");
            match set_autostart(enabled, &args) {
                Ok(()) => {
                    let msg = json!({
                        "ok": true,
                        "message": if enabled { "Autostart enabled on login" } else { "Autostart disabled" }
                    })
                    .to_string();
                    write_response(&mut stream, "200 OK", "application/json", msg.as_bytes())
                        .await?;
                }
                Err(e) => {
                    let msg =
                        json!({ "error": e.to_string(), "message": e.to_string() }).to_string();
                    write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "application/json",
                        msg.as_bytes(),
                    )
                    .await?;
                }
            }
        }

        ("POST", "/api/os/firewall") => match configure_firewall() {
            Ok(m) => {
                let msg = json!({ "ok": true, "message": m }).to_string();
                write_response(&mut stream, "200 OK", "application/json", msg.as_bytes()).await?;
            }
            Err(e) => {
                let msg = json!({ "error": e.to_string(), "message": e.to_string() }).to_string();
                write_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "application/json",
                    msg.as_bytes(),
                )
                .await?;
            }
        },

        ("POST", "/api/os/elevated") => match configure_elevated_task() {
            Ok(m) => {
                let msg = json!({ "ok": true, "message": m }).to_string();
                write_response(&mut stream, "200 OK", "application/json", msg.as_bytes()).await?;
            }
            Err(e) => {
                let msg = json!({ "error": e.to_string(), "message": e.to_string() }).to_string();
                write_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "application/json",
                    msg.as_bytes(),
                )
                .await?;
            }
        },

        ("POST", "/api/trust/reset") => {
            let tofu_path = ctrl.config.lock().client.resolved_tofu_path();
            let _ = std::fs::remove_file(&tofu_path);
            ctrl.config.lock().client.trusted_fingerprint = None;
            let _ = ctrl.config.lock().save(&ctrl.config_path);
            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                br#"{"ok":true,"message":"Cleared saved server fingerprint (will re-trust on next connect)"}"#,
            )
            .await?;
        }

        ("POST", "/api/show") => {
            ctrl.show_window();
            write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#).await?;
        }

        ("POST", "/api/quit") => {
            write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#).await?;
            let _ = ctrl.quit_tx.send(());
        }

        _ => {
            write_response(&mut stream, "404 Not Found", "text/plain", b"not found").await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn send_raw_http(addr: SocketAddr, req: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        resp
    }

    #[test]
    fn host_origin_and_fetch_site_validators() {
        assert!(is_allowed_host("127.0.0.1:24802", 24802));
        assert!(is_allowed_host("localhost:24802", 24802));
        assert!(is_allowed_host("LOCALHOST:24802", 24802));
        assert!(is_allowed_host("[::1]:24802", 24802));
        assert!(!is_allowed_host("127.0.0.1", 24802));
        assert!(!is_allowed_host("127.0.0.1:9999", 24802));
        assert!(!is_allowed_host("evil.example:24802", 24802));
        assert!(!is_allowed_host("127.0.0.1.evil.example:24802", 24802));

        assert!(is_allowed_origin("http://127.0.0.1:24802", 24802));
        assert!(is_allowed_origin("http://localhost:24802", 24802));
        assert!(is_allowed_origin("http://[::1]:24802", 24802));
        assert!(!is_allowed_origin("http://evil.example", 24802));
        assert!(!is_allowed_origin("https://127.0.0.1:24802", 24802));
        assert!(!is_allowed_origin("null", 24802));

        assert!(is_allowed_fetch_site("same-origin"));
        assert!(is_allowed_fetch_site("none"));
        assert!(!is_allowed_fetch_site("cross-site"));
        assert!(!is_allowed_fetch_site("same-site"));
    }

    #[tokio::test]
    async fn gui_http_serves_ui_state_and_blocks_dns_rebinding() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        let url = format!("http://127.0.0.1:{port}");
        let tmp_cfg = std::env::temp_dir().join(format!(
            "glide-gui-test-{}-{port}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp_cfg);
        let (quit_tx, _quit_rx) = mpsc::unbounded_channel();
        let ctrl = Arc::new(AppController::new(tmp_cfg.clone(), url, quit_tx));
        {
            let mut cfg = ctrl.config.lock();
            cfg.server.identity_dir = Some(PathBuf::from("/original/identity"));
            cfg.client.tofu_store_path = Some(PathBuf::from("/original/tofu"));
        }

        let srv_ctrl = Arc::clone(&ctrl);
        let srv = tokio::spawn(async move {
            for _ in 0..10 {
                let (stream, _) = listener.accept().await.unwrap();
                let _ = handle_http_conn(stream, Arc::clone(&srv_ctrl)).await;
            }
        });

        // 1. GET / returns HTML with security headers and injected version (not v0.2.1)
        let r1 = send_raw_http(addr, &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"))
            .await;
        assert!(r1.starts_with("HTTP/1.1 200 OK"));
        assert!(r1.contains("X-Content-Type-Options: nosniff"));
        assert!(r1.contains("X-Frame-Options: DENY"));
        assert!(r1.contains("Referrer-Policy: no-referrer"));
        assert!(r1.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
        assert!(!r1.contains("v0.2.1"));
        assert!(!r1.contains("onclick="));

        // 2. GET /api/state returns valid JSON including version
        let r2 = send_raw_http(
            addr,
            &format!("GET /api/state HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n"),
        )
        .await;
        assert!(r2.starts_with("HTTP/1.1 200 OK"));
        assert!(r2.contains("\"status\""));
        assert!(r2.contains(&format!("\"version\":\"{}\"", env!("CARGO_PKG_VERSION"))));

        // 3. Non-loopback Host header is blocked (DNS rebinding guard)
        let r3 = send_raw_http(addr, "GET /api/state HTTP/1.1\r\nHost: evil.example\r\n\r\n").await;
        assert!(r3.starts_with("HTTP/1.1 403 Forbidden"));

        // 4. Missing Host header is blocked
        let r4 = send_raw_http(addr, "GET /api/state HTTP/1.1\r\n\r\n").await;
        assert!(r4.starts_with("HTTP/1.1 403 Forbidden"));

        // 5. Duplicate Host headers are blocked
        let r5 = send_raw_http(
            addr,
            &format!("GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nHost: evil.example:{port}\r\n\r\n"),
        )
        .await;
        assert!(r5.starts_with("HTTP/1.1 403 Forbidden"));

        // 6. Cross-origin POST is blocked by Origin check
        let r6 = send_raw_http(
            addr,
            &format!("POST /api/lock HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nContent-Length: 0\r\n\r\n"),
        )
        .await;
        assert!(r6.starts_with("HTTP/1.1 403 Forbidden"));

        // 7. Cross-site POST is blocked by Sec-Fetch-Site check
        let r7 = send_raw_http(
            addr,
            &format!("POST /api/lock HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nSec-Fetch-Site: cross-site\r\nContent-Length: 0\r\n\r\n"),
        )
        .await;
        assert!(r7.starts_with("HTTP/1.1 403 Forbidden"));

        // 8. JSON/control route rejects oversized payload (> 1 MiB)
        let r8 = send_raw_http(
            addr,
            &format!(
                "POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n",
                MAX_CONTROL_BODY_BYTES + 1
            ),
        )
        .await;
        assert!(r8.starts_with("HTTP/1.1 413 Payload Too Large"));

        // 9. /api/os/autostart rejects invalid role
        let bad_autostart = r#"{"enabled":true,"role":"client; rm -rf /"}"#;
        let r9 = send_raw_http(
            addr,
            &format!(
                "POST /api/os/autostart HTTP/1.1\r\nHost: [::1]:{port}\r\nOrigin: http://[::1]:{port}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n{bad_autostart}",
                bad_autostart.len()
            ),
        )
        .await;
        assert!(r9.starts_with("HTTP/1.1 400 Bad Request"));

        // 10. POST /api/config preserves existing identity_dir and tofu_store_path
        let mut tampered = ctrl.config.lock().clone();
        tampered.server.identity_dir = Some(PathBuf::from("/tmp/attacker-identity"));
        tampered.client.tofu_store_path = Some(PathBuf::from("/tmp/attacker-tofu"));
        let cfg_json = serde_json::to_string(&tampered).unwrap();
        let r10 = send_raw_http(
            addr,
            &format!(
                "POST /api/config HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n{cfg_json}",
                cfg_json.len()
            ),
        )
        .await;
        assert!(r10.starts_with("HTTP/1.1 200 OK"));
        {
            let saved = ctrl.config.lock();
            assert_eq!(
                saved.server.identity_dir,
                Some(PathBuf::from("/original/identity"))
            );
            assert_eq!(
                saved.client.tofu_store_path,
                Some(PathBuf::from("/original/tofu"))
            );
        }

        srv.await.unwrap();
        let _ = std::fs::remove_file(&tmp_cfg);
    }
}
