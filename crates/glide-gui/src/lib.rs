//! Embedded zero-dependency desktop GUI + native Windows System Tray for Glide.
//!
//! Serves a clean, modern, identical UI across Windows 11 and Ubuntu X11 over a
//! loopback-only HTTP + Server-Sent Events control server and launches it in a
//! frameless desktop app window (`--app` mode with browser fallback).

use anyhow::{Context, Result};
use bytes::Bytes;
use glide_backend::os_integration::{
    configure_elevated_task, configure_firewall, is_autostart_enabled, set_autostart,
};
use glide_core::client::run_native_client_with_runtime;
use glide_core::config::{default_config_dir, GlideConfigFile};
use glide_core::runtime::{RuntimeCommand, RuntimeState};
use glide_core::server::run_native_server_with_runtime;
use glide_net::discovery::{discover_servers, machine_hostname};
use glide_net::tls::ServerIdentity;
use glide_proto::{Edge, MAX_FILE_BYTES};
use parking_lot::Mutex;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const UI_HTML: &str = include_str!("ui.html");
pub const DEFAULT_GUI_PORT: u16 = 24802;

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
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    url: String,
}

impl AppController {
    fn new(config_path: PathBuf, url: String) -> Self {
        let cfg = if config_path.exists() {
            GlideConfigFile::load(&config_path).unwrap_or_default()
        } else {
            GlideConfigFile::default()
        };
        let fp = ServerIdentity::load_or_generate(&cfg.server.resolved_identity_dir())
            .ok()
            .map(|id| id.fingerprint);

        let runtime = RuntimeState::new();
        if let Ok(doc) = glide_backend::run_doctor() {
            runtime.update(|s| {
                s.geometry = doc.geometry;
                s.fingerprint = fp.clone();
            });
        }

        Self {
            config_path,
            config: Mutex::new(cfg),
            runtime,
            engine: Mutex::new(None),
            fingerprint: Mutex::new(fp),
            url,
        }
    }

    fn stop_engine(&self) {
        if let Some(active) = self.engine.lock().take() {
            active.task.abort();
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

    fn start_engine(&self, role: &str) -> Result<()> {
        self.stop_engine();
        let cfg = self.config.lock().clone();
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

    fn apply_config(&self, new_cfg: GlideConfigFile) -> Result<()> {
        new_cfg.save(&self.config_path)?;
        let s = new_cfg.server.clone();
        let c = new_cfg.client.clone();
        *self.config.lock() = new_cfg;

        let locked = self.runtime.snapshot().locked;
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

/// Launches the Glide desktop GUI (embedded loopback app server + frameless window + Windows tray).
pub async fn run_gui(opts: GuiOptions) -> Result<()> {
    glide_backend::init_platform();

    let listener = match TcpListener::bind(("127.0.0.1", opts.port)).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("failed to bind local GUI port")?,
    };
    let actual_addr = listener.local_addr()?;
    let url = format!("http://127.0.0.1:{}", actual_addr.port());

    let cfg_path = opts
        .config_path
        .unwrap_or_else(|| default_config_dir().join("glide.toml"));
    let ctrl = Arc::new(AppController::new(cfg_path, url.clone()));

    if let Some(ref role) = opts.auto_start_role {
        if let Err(e) = ctrl.start_engine(role) {
            warn!(error = %e, "Failed auto-starting Glide engine");
        }
    }

    info!(%url, "Glide GUI ready");

    if !opts.minimized {
        open_app_window(&url);
    }

    // Native Windows 11 System Tray integration (zero extra deps!)
    let (quit_tx, mut quit_rx) = mpsc::unbounded_channel::<()>();
    #[cfg(target_os = "windows")]
    {
        let mut tray_rx = glide_backend::windows::tray::spawn_windows_tray();
        let tray_ctrl = Arc::clone(&ctrl);
        let tray_quit = quit_tx.clone();
        tokio::spawn(async move {
            use glide_backend::windows::tray::TrayAction;
            while let Some(act) = tray_rx.recv().await {
                match act {
                    TrayAction::OpenUi => open_app_window(&tray_ctrl.url),
                    TrayAction::ToggleLock => {
                        tray_ctrl.send_cmd(RuntimeCommand::ToggleLock);
                    }
                    TrayAction::Quit => {
                        tray_ctrl.stop_engine();
                        let _ = tray_quit.send(());
                        break;
                    }
                }
            }
        });
    }
    let _ = quit_tx;

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
                info!("Exiting Glide GUI from system tray");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down Glide GUI (Ctrl+C)");
                break;
            }
        }
    }

    ctrl.stop_engine();
    Ok(())
}

/// Opens the GUI in a clean frameless desktop app window (`--app=...`) with browser fallback.
pub fn open_app_window(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        let app_arg = format!("--app={url}");
        let size_arg = "--window-size=1020,760";
        let candidates = [
            "msedge",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            "chrome",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        ];
        for bin in candidates {
            if Command::new(bin).args([&app_arg, size_arg]).spawn().is_ok() {
                return;
            }
        }
        let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }

    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        let app_arg = format!("--app={url}");
        let size_arg = "--window-size=1020,760";
        for bin in [
            "google-chrome",
            "chromium-browser",
            "chromium",
            "brave-browser",
            "microsoft-edge",
        ] {
            if Command::new(bin).args([&app_arg, size_arg]).spawn().is_ok() {
                return;
            }
        }
        let _ = Command::new("xdg-open").arg(url).spawn();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
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
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(hdr.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

async fn handle_http_conn(mut stream: TcpStream, ctrl: Arc<AppController>) -> Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            write_response(
                &mut stream,
                "431 Request Header Fields Too Large",
                "text/plain",
                b"headers too large",
            )
            .await?;
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
    let mut x_filename: Option<String> = None;
    let mut host_ok = true;

    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "content-length" => content_len = val.parse().unwrap_or(0),
                "x-filename" => x_filename = Some(percent_decode(val)),
                "host" => {
                    let host_only = val.split(':').next().unwrap_or("");
                    host_ok = host_only == "127.0.0.1" || host_only == "localhost";
                }
                _ => {}
            }
        }
    }

    if !host_ok {
        write_response(&mut stream, "403 Forbidden", "text/plain", b"loopback only").await?;
        return Ok(());
    }

    if content_len > MAX_FILE_BYTES {
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
    let mut body = Vec::with_capacity(content_len);
    if buf.len() > body_start {
        body.extend_from_slice(&buf[body_start..]);
    }
    while body.len() < content_len {
        let to_read = (content_len - body.len()).min(65536);
        let mut tmp = vec![0u8; to_read];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len);

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            write_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                UI_HTML.as_bytes(),
            )
            .await?;
        }

        ("GET", "/api/events") => {
            let hdr = "HTTP/1.1 200 OK\r\n\
                       Content-Type: text/event-stream\r\n\
                       Cache-Control: no-cache\r\n\
                       Connection: keep-alive\r\n\r\n";
            stream.write_all(hdr.as_bytes()).await?;
            let mut rx = ctrl.runtime.subscribe();
            let initial = serde_json::to_string(&ctrl.runtime.snapshot())?;
            stream
                .write_all(format!("data: {initial}\n\n").as_bytes())
                .await?;
            loop {
                tokio::select! {
                    res = rx.recv() => {
                        match res {
                            Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let snap = serde_json::to_string(&ctrl.runtime.snapshot())?;
                                if stream.write_all(format!("data: {snap}\n\n").as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(15)) => {
                        if stream.write_all(b": keepalive\n\n").await.is_err() {
                            break;
                        }
                    }
                }
            }
        }

        ("GET", "/api/state") => {
            let cfg = ctrl.config.lock().clone();
            let doctor = glide_backend::run_doctor().ok();
            let payload = json!({
                "config": cfg,
                "status": ctrl.runtime.snapshot(),
                "fingerprint": *ctrl.fingerprint.lock(),
                "autostart": is_autostart_enabled(),
                "hostname": machine_hostname(),
                "os": format!("{:?}", glide_backend::current_os_kind()),
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
            let found = discover_servers(port, Duration::from_millis(1400), None, false)
                .await
                .unwrap_or_default();
            let list: Vec<_> = found
                .into_iter()
                .map(|s| {
                    json!({
                        "name": s.name,
                        "os": s.os,
                        "addr": s.addr.to_string(),
                        "fingerprint": s.fingerprint,
                    })
                })
                .collect();
            let body = json!({ "servers": list }).to_string();
            write_response(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
        }

        ("POST", "/api/config") => match serde_json::from_slice::<GlideConfigFile>(&body) {
            Ok(new_cfg) => match ctrl.apply_config(new_cfg) {
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
            },
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
            match ctrl.start_engine(role) {
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
            ctrl.stop_engine();
            write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#).await?;
        }

        ("POST", "/api/lock") => {
            ctrl.send_cmd(RuntimeCommand::ToggleLock);
            write_response(&mut stream, "200 OK", "application/json", br#"{"ok":true}"#).await?;
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
            let filename = x_filename.unwrap_or_else(|| "glide-transfer.bin".into());
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
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let enabled = req["enabled"].as_bool().unwrap_or(false);
            let role = req["role"].as_str().unwrap_or("client");
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

        ("POST", "/api/os/firewall") => {
            let res = configure_firewall();
            let msg = match res {
                Ok(m) => json!({ "ok": true, "message": m }),
                Err(e) => json!({ "error": e.to_string(), "message": e.to_string() }),
            };
            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                msg.to_string().as_bytes(),
            )
            .await?;
        }

        ("POST", "/api/os/elevated") => {
            let res = configure_elevated_task();
            let msg = match res {
                Ok(m) => json!({ "ok": true, "message": m }),
                Err(e) => json!({ "error": e.to_string(), "message": e.to_string() }),
            };
            write_response(
                &mut stream,
                "200 OK",
                "application/json",
                msg.to_string().as_bytes(),
            )
            .await?;
        }

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

        _ => {
            write_response(&mut stream, "404 Not Found", "text/plain", b"not found").await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gui_http_serves_ui_state_and_blocks_dns_rebinding() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}", addr.port());
        let tmp_cfg = std::env::temp_dir().join("glide-gui-test.toml");
        let ctrl = Arc::new(AppController::new(tmp_cfg, url));

        let srv_ctrl = Arc::clone(&ctrl);
        let srv = tokio::spawn(async move {
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                let _ = handle_http_conn(stream, Arc::clone(&srv_ctrl)).await;
            }
        });

        // 1. GET / returns HTML
        let mut s1 = TcpStream::connect(addr).await.unwrap();
        s1.write_all(
            format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", addr.port()).as_bytes(),
        )
        .await
        .unwrap();
        let mut r1 = String::new();
        s1.read_to_string(&mut r1).await.unwrap();
        assert!(r1.starts_with("HTTP/1.1 200 OK"));
        assert!(r1.contains("Glide"));

        // 2. GET /api/state returns valid JSON
        let mut s2 = TcpStream::connect(addr).await.unwrap();
        s2.write_all(
            format!(
                "GET /api/state HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
                addr.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut r2 = String::new();
        s2.read_to_string(&mut r2).await.unwrap();
        assert!(r2.starts_with("HTTP/1.1 200 OK"));
        assert!(r2.contains("\"status\""));

        // 3. Non-loopback Host header is blocked (DNS rebinding guard)
        let mut s3 = TcpStream::connect(addr).await.unwrap();
        s3.write_all(b"GET /api/state HTTP/1.1\r\nHost: evil.example\r\n\r\n")
            .await
            .unwrap();
        let mut r3 = String::new();
        s3.read_to_string(&mut r3).await.unwrap();
        assert!(r3.starts_with("HTTP/1.1 403 Forbidden"));

        srv.await.unwrap();
    }
}
