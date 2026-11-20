//! Native GPU-rendered Glide GUI powered by Rune (wgpu + winit).

mod icons;
mod pages;
mod theme;
mod widgets;

use crate::{launch_browser_window, AppController, GUI_HEIGHT, GUI_WIDTH};
use bytes::Bytes;
use glide_backend::os_integration::{
    configure_elevated_task, configure_firewall, is_autostart_enabled, set_autostart,
};
use glide_core::config::{GlideConfigFile, KeyRemapRule};
use glide_core::runtime::{
    ClipboardHistoryEntry, FileTransferEntry, PeerSummary, RuntimeCommand, StatusSnapshot,
};
use glide_net::discovery::{discover_servers, local_ipv4_addresses, machine_hostname};
use glide_proto::{ClipboardFormat, Edge, OsKind, MAX_FILE_BYTES};
use rune::{App, AppHandle, AppSpec, Cx, El, Key, KeyEvent, TextEdit, Tx, WindowOptions};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use theme::{Palette, ThemePref};
use tracing::warn;

const FONT_INTER_400: &[u8] = include_bytes!("../../assets/fonts/static/Inter-Regular.ttf");
const FONT_INTER_500: &[u8] = include_bytes!("../../assets/fonts/static/Inter-Medium.ttf");
const FONT_INTER_600: &[u8] = include_bytes!("../../assets/fonts/static/Inter-SemiBold.ttf");
const FONT_MONO_400: &[u8] = include_bytes!("../../assets/fonts/static/JetBrainsMono-Regular.ttf");

const FONTS: &[&[u8]] = &[
    FONT_INTER_400,
    FONT_INTER_500,
    FONT_INTER_600,
    FONT_MONO_400,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Devices,
    Input,
    Clipboard,
    System,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Server,
    Client,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }
}

#[derive(Clone, Debug)]
pub enum DragKind {
    Peer(String),
    SelfClient,
}

#[derive(Clone, Debug)]
pub struct DeskDrag {
    pub kind: DragKind,
    pub origin: Edge,
    pub dx: f32,
    pub dy: f32,
    pub moved: bool,
    pub hover: Option<Edge>,
}

#[derive(Clone, Debug)]
pub struct DiscoveredPeer {
    pub name: String,
    pub os: OsKind,
    pub addr: String,
    pub fingerprint: String,
}

pub struct Ui {
    ctrl: Arc<AppController>,
    rt: tokio::runtime::Handle,
    handle: Option<AppHandle<Ui>>,

    // Machine & environment metadata
    pub hostname: String,
    pub os_kind: OsKind,
    pub os_label: &'static str,
    pub is_windows: bool,
    pub fingerprint: Option<String>,
    pub doctor_display: String,
    pub doctor_text: String,
    pub tray_active: bool,

    // Live runtime & editable config
    pub status: StatusSnapshot,
    pub config: GlideConfigFile,
    pub local_ips: Vec<String>,
    pub autostart: bool,

    // UI navigation & view state
    pub tab: Tab,
    pub role: Role,
    pub theme: ThemePref,
    pub selected_peer_name: Option<String>,
    pub discovered: Vec<DiscoveredPeer>,
    pub scanning: bool,

    // Editable text fields + last committed snapshots
    pub port_edit: TextEdit,
    pub server_edit: TextEdit,
    pub remap_from: TextEdit,
    pub remap_to: TextEdit,
    last_committed_port: String,
    last_committed_server: String,

    // Transient UI state
    toast: Option<(String, Option<Instant>)>,
    pub dismissed_error: Option<String>,
    pub desk_drag: Option<DeskDrag>,
    pub drop_hover: bool,
}

impl Ui {
    fn new(ctrl: Arc<AppController>, rt: tokio::runtime::Handle) -> Self {
        let os_kind = glide_backend::current_os_kind();
        let os_label = os_label(os_kind);
        let is_windows = matches!(os_kind, OsKind::Windows);
        let hostname = machine_hostname();
        let cfg = ctrl.config.lock().clone();
        let status = ctrl.runtime.snapshot();
        let fingerprint = ctrl.fingerprint.lock().clone();
        let tray_active = ctrl.tray_active.load(Ordering::SeqCst);
        let autostart = is_autostart_enabled();
        let local_ips = local_ipv4_addresses();

        let (doctor_display, doctor_text) = match glide_backend::run_doctor() {
            Ok(doc) => {
                let mut lines = vec![
                    format!("OS       {os_label} ({os_kind:?})"),
                    format!("Display  {}", doc.display_info),
                    String::new(),
                ];
                for note in &doc.notes {
                    lines.push(format!("• {note}"));
                }
                (doc.display_info, lines.join("\n"))
            }
            Err(_) => (
                "Display detected".into(),
                format!("OS       {os_label} ({os_kind:?})\n"),
            ),
        };

        let port_str = cfg.server.bind.port().to_string();
        let server_str = cfg.client.server.clone().unwrap_or_default();
        let role = if status.mode == "client" {
            Role::Client
        } else {
            Role::Server
        };

        Self {
            ctrl,
            rt,
            handle: None,
            hostname,
            os_kind,
            os_label,
            is_windows,
            fingerprint,
            doctor_display,
            doctor_text,
            tray_active,
            status,
            config: cfg,
            local_ips,
            autostart,
            tab: Tab::Devices,
            role,
            theme: ThemePref::load(),
            selected_peer_name: None,
            discovered: Vec::new(),
            scanning: false,
            port_edit: TextEdit::new(&port_str),
            server_edit: TextEdit::new(&server_str),
            remap_from: TextEdit::default(),
            remap_to: TextEdit::default(),
            last_committed_port: port_str,
            last_committed_server: server_str,
            toast: None,
            dismissed_error: None,
            desk_drag: None,
            drop_hover: false,
        }
    }

    // -----------------------------------------------------------------------
    // Derived state helpers
    // -----------------------------------------------------------------------

    pub fn is_running(&self) -> bool {
        self.status.mode == "server" || self.status.mode == "client"
    }

    pub fn client_connected(&self) -> bool {
        self.role == Role::Client
            && self.is_running()
            && self.status.status.starts_with("Connected")
    }

    pub fn client_server_label(&self) -> String {
        if let Some(rest) = self.status.status.strip_prefix("Connected to ") {
            return rest.to_owned();
        }
        let target = self.status.active_target.as_str();
        if !target.is_empty() && target != "Local" && target != "Local (Controlling)" {
            return target.to_owned();
        }
        "Server".into()
    }

    pub fn friendly_target(&self) -> String {
        let t = self.status.active_target.as_str();
        if t.is_empty() || t == "Local" {
            if self.role == Role::Server || !self.is_running() {
                "This PC".into()
            } else {
                self.client_server_label()
            }
        } else if t == "Local (Controlling)" {
            "This PC".into()
        } else {
            t.to_owned()
        }
    }

    pub fn sidebar_status_text(&self) -> String {
        let raw = if self.status.status.is_empty() {
            "Idle"
        } else {
            self.status.status.as_str()
        };
        if let Some(rest) = raw.strip_prefix("Connected (") {
            if let Some(num) = rest.strip_suffix(" peer(s))") {
                let word = if num == "1" { "client" } else { "clients" };
                return format!("Connected · {num} {word}");
            }
        }
        raw.to_owned()
    }

    pub fn layout_hint(&self) -> &'static str {
        if self.role == Role::Server {
            if !self.is_running() {
                "Start the server, then connect from your other PC"
            } else if self.status.peers.is_empty() {
                "Waiting for a client to connect…"
            } else {
                "Drag a screen to rearrange · double-click to jump there"
            }
        } else if self.is_running() {
            "Drag This PC to another side — it moves live, no reconnect"
        } else {
            "Drag This PC to where it sits next to the server"
        }
    }

    pub fn active_peer(&self) -> Option<&PeerSummary> {
        if self.status.peers.is_empty() {
            return None;
        }
        if let Some(ref sel) = self.selected_peer_name {
            if let Some(p) = self.status.peers.iter().find(|p| &p.name == sel) {
                return Some(p);
            }
        }
        self.status.peers.first()
    }

    pub fn current_port(&self) -> u16 {
        if let Ok(p) = self.port_edit.text.trim().parse::<u16>() {
            if p >= 1024 {
                return p;
            }
        }
        self.config.server.bind.port()
    }

    pub fn visible_error(&self) -> Option<&str> {
        let err = self.status.last_error.as_deref()?;
        if self.dismissed_error.as_deref() == Some(err) {
            None
        } else {
            Some(err)
        }
    }

    pub fn active_toast(&self) -> Option<&str> {
        self.toast.as_ref().map(|(m, _)| m.as_str())
    }

    pub fn toast(&mut self, msg: impl Into<String>, tx: &mut Tx) {
        let until = Instant::now() + Duration::from_millis(2400);
        self.toast = Some((msg.into(), Some(until)));
        tx.wake_after(Duration::from_millis(2450));
    }

    fn apply_status(&mut self, snap: StatusSnapshot) {
        if snap.mode == "server" {
            self.role = Role::Server;
        } else if snap.mode == "client" {
            self.role = Role::Client;
        }
        if snap.last_error.is_none() {
            self.dismissed_error = None;
        }
        self.status = snap;
    }

    // -----------------------------------------------------------------------
    // User actions
    // -----------------------------------------------------------------------

    pub fn guard_role(&mut self, role: Role, tx: &mut Tx) {
        if self.is_running() {
            if role != self.role {
                self.toast("Stop the current session to switch roles", tx);
            }
            return;
        }
        self.role = role;
    }

    pub fn commit_text_fields(&mut self) -> bool {
        let mut changed = false;
        let port_raw = self.port_edit.text.trim().to_owned();
        if port_raw != self.last_committed_port {
            if let Ok(p) = port_raw.parse::<u16>() {
                if p >= 1024 {
                    self.config.server.bind.set_port(p);
                    self.last_committed_port = port_raw;
                    changed = true;
                }
            }
        }
        let srv_raw = self.server_edit.text.trim().to_owned();
        if srv_raw != self.last_committed_server {
            self.config.client.server = if srv_raw.is_empty() {
                None
            } else {
                Some(srv_raw.clone())
            };
            self.last_committed_server = srv_raw;
            changed = true;
        }
        changed
    }

    pub fn commit_text_fields_and_push(&mut self) {
        if self.commit_text_fields() {
            self.push_config();
        }
    }

    pub fn push_config(&self) {
        let cfg = self.config.clone();
        if let Err(e) = self.ctrl.apply_config(cfg) {
            warn!(error = %e, "Failed saving Glide config");
        }
    }

    pub fn toggle_start_stop(&mut self, tx: &mut Tx) {
        let ctrl = Arc::clone(&self.ctrl);
        let handle = self.handle.clone();
        if self.is_running() {
            self.toast("Stopping…", tx);
            self.rt.spawn(async move {
                ctrl.stop_engine().await;
                if let Some(h) = handle {
                    h.update(|s, tx| s.toast("Stopped", tx));
                }
            });
        } else {
            self.commit_text_fields();
            self.push_config();
            let role = self.role;
            self.toast(
                if role == Role::Server {
                    "Server started"
                } else {
                    "Connecting…"
                },
                tx,
            );
            self.rt.spawn(async move {
                if let Err(e) = ctrl.start_engine(role.as_str()).await {
                    let msg = format!("Error: {e}");
                    if let Some(h) = handle {
                        h.update(move |s, tx| s.toast(msg, tx));
                    }
                }
            });
        }
    }

    pub fn toggle_lock(&mut self) {
        self.ctrl.send_cmd(RuntimeCommand::ToggleLock);
    }

    pub fn switch_to_edge(&mut self, edge: Option<Edge>) {
        if !self.is_running() || self.role != Role::Server {
            return;
        }
        match edge {
            Some(e) => {
                self.ctrl.send_cmd(RuntimeCommand::SwitchToEdge(e));
            }
            None => {
                self.ctrl.send_cmd(RuntimeCommand::ReturnLocal);
            }
        }
    }

    pub fn move_peer_live(&mut self, edge: Edge, peer_name: Option<String>, tx: &mut Tx) {
        if self.role == Role::Client {
            if self.config.client.edge == edge && !self.is_running() {
                return;
            }
            self.config.client.edge = edge;
            self.push_config();
            if self.is_running() {
                self.ctrl.send_cmd(RuntimeCommand::MovePeerEdge {
                    peer_name: None,
                    edge,
                });
                self.toast(
                    format!("This PC moved to the {}", pages::edge_word(edge)),
                    tx,
                );
            } else {
                self.toast(
                    format!("This PC is now on the {}", pages::edge_word(edge)),
                    tx,
                );
            }
            return;
        }

        if !self.is_running() || self.status.peers.is_empty() {
            self.toast("Connect a client first, then drag it into place", tx);
            return;
        }
        let target_name = peer_name.or_else(|| self.active_peer().map(|p| p.name.clone()));
        let Some(name) = target_name else {
            return;
        };
        if self
            .status
            .peers
            .iter()
            .any(|p| p.name == name && p.edge == edge)
        {
            return;
        }
        self.selected_peer_name = Some(name.clone());
        self.ctrl.send_cmd(RuntimeCommand::MovePeerEdge {
            peer_name: Some(name.clone()),
            edge,
        });
        self.toast(
            format!("{name} moved to the {}", pages::edge_word(edge)),
            tx,
        );
    }

    pub fn copy_local_ip(&mut self, tx: &mut Tx) {
        let Some(first) = self.local_ips.first() else {
            self.toast("No local network address found", tx);
            return;
        };
        let addr = format!("{first}:{}", self.current_port());
        tx.copy(addr.clone());
        self.toast(format!("Copied {addr}"), tx);
    }

    pub fn copy_fingerprint(&mut self, tx: &mut Tx) {
        let Some(fp) = self
            .status
            .fingerprint
            .as_deref()
            .or(self.fingerprint.as_deref())
            .map(str::to_owned)
        else {
            self.toast("No fingerprint yet", tx);
            return;
        };
        tx.copy(fp);
        self.toast("Fingerprint copied", tx);
    }

    pub fn trigger_scan(&mut self, silent: bool) {
        if self.scanning {
            return;
        }
        if !silent {
            self.scanning = true;
        }
        let ctrl = Arc::clone(&self.ctrl);
        let handle = self.handle.clone();
        self.rt.spawn(async move {
            let (servers, ips) = run_discovery(&ctrl).await;
            if let Some(h) = handle {
                h.update(move |s, _| {
                    s.scanning = false;
                    s.local_ips = ips;
                    s.discovered = servers;
                });
            }
        });
    }

    pub fn connect_discovered(&mut self, addr: String, fingerprint: String, tx: &mut Tx) {
        self.role = Role::Client;
        self.config.client.server = Some(addr.clone());
        self.config.client.trusted_fingerprint = Some(fingerprint);
        self.server_edit = TextEdit::new(&addr);
        self.last_committed_server = addr.clone();
        self.push_config();
        self.toast(format!("Connecting to {addr}"), tx);

        let ctrl = Arc::clone(&self.ctrl);
        let handle = self.handle.clone();
        self.rt.spawn(async move {
            if let Err(e) = ctrl.start_engine("client").await {
                let msg = format!("Error: {e}");
                if let Some(h) = handle {
                    h.update(move |s, tx| s.toast(msg, tx));
                }
            }
        });
    }

    pub fn add_remap_rule(&mut self, from: &str, to: &str, tx: &mut Tx) {
        let from = from.trim();
        let to = to.trim();
        if from.is_empty() || to.is_empty() {
            self.toast("Pick a key on both sides", tx);
            return;
        }
        if self
            .config
            .client
            .key_remap
            .iter()
            .any(|r| r.from.eq_ignore_ascii_case(from))
        {
            self.toast(format!("{from} is already remapped"), tx);
            return;
        }
        self.config.client.key_remap.push(KeyRemapRule {
            from: from.to_owned(),
            to: to.to_owned(),
        });
        self.push_config();
        self.toast(format!("Remapped {from} to {to}"), tx);
    }

    pub fn submit_remap_inputs(&mut self, tx: &mut Tx) {
        let from = self.remap_from.text.trim().to_owned();
        let to = self.remap_to.text.trim().to_owned();
        if from.is_empty() || to.is_empty() {
            self.toast("Pick a key on both sides", tx);
            return;
        }
        self.remap_from.clear();
        self.remap_to.clear();
        self.add_remap_rule(&from, &to, tx);
    }

    pub fn remove_remap(&mut self, idx: usize) {
        if idx < self.config.client.key_remap.len() {
            self.config.client.key_remap.remove(idx);
            self.push_config();
        }
    }

    pub fn resend_clipboard(&mut self, id: usize, tx: &mut Tx) {
        if self.ctrl.send_cmd(RuntimeCommand::ResendClipboard(id)) {
            self.toast("Sent to the other PC", tx);
        } else {
            self.toast("Connect first to re-send", tx);
        }
    }

    pub fn send_file_paths(&mut self, paths: Vec<PathBuf>, tx: &mut Tx) {
        if !self.is_running() {
            self.toast("Connect first, then send files", tx);
            return;
        }
        if paths.is_empty() {
            return;
        }
        let ctrl = Arc::clone(&self.ctrl);
        let handle = self.handle.clone();
        self.rt.spawn_blocking(move || {
            for path in paths {
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("glide-transfer.bin")
                    .to_owned();
                if let Some(ref h) = handle {
                    let msg = format!("Sending {filename}…");
                    h.update(move |s, tx| s.toast(msg, tx));
                }
                match std::fs::metadata(&path) {
                    Ok(meta) if meta.len() as usize > MAX_FILE_BYTES => {
                        if let Some(ref h) = handle {
                            let msg = format!("{filename} exceeds the 256 MB limit");
                            h.update(move |s, tx| s.toast(msg, tx));
                        }
                        continue;
                    }
                    Err(e) => {
                        if let Some(ref h) = handle {
                            let msg = format!("Could not read {filename}: {e}");
                            h.update(move |s, tx| s.toast(msg, tx));
                        }
                        continue;
                    }
                    Ok(_) => {}
                }
                match std::fs::read(&path) {
                    Ok(data) => {
                        let ok = ctrl.send_cmd(RuntimeCommand::SendFile {
                            filename: filename.clone(),
                            data: Bytes::from(data),
                        });
                        if let Some(ref h) = handle {
                            let msg = if ok {
                                format!("Sent {filename}")
                            } else {
                                "Glide is not currently running".into()
                            };
                            h.update(move |s, tx| s.toast(msg, tx));
                        }
                    }
                    Err(e) => {
                        if let Some(ref h) = handle {
                            let msg = format!("Failed reading {filename}: {e}");
                            h.update(move |s, tx| s.toast(msg, tx));
                        }
                    }
                }
            }
        });
        let _ = tx;
    }

    pub fn toggle_autostart(&mut self, tx: &mut Tx) {
        let next = !self.autostart;
        self.autostart = next;
        let args = format!("gui --minimized --auto-start {}", self.role.as_str());
        let handle = self.handle.clone();
        let _ = tx;
        self.rt.spawn_blocking(move || {
            let res = set_autostart(next, &args);
            if let Some(h) = handle {
                h.update(move |s, tx| match res {
                    Ok(()) => {
                        s.autostart = next;
                        s.toast(
                            if next {
                                "Autostart enabled on login"
                            } else {
                                "Autostart disabled"
                            },
                            tx,
                        );
                    }
                    Err(e) => {
                        s.autostart = is_autostart_enabled();
                        s.toast(format!("Autostart failed: {e}"), tx);
                    }
                });
            }
        });
    }

    pub fn run_firewall(&mut self, tx: &mut Tx) {
        self.toast("Configuring firewall…", tx);
        let handle = self.handle.clone();
        self.rt.spawn_blocking(move || {
            let msg = match configure_firewall() {
                Ok(m) => m,
                Err(e) => e.to_string(),
            };
            if let Some(h) = handle {
                h.update(move |s, tx| s.toast(msg, tx));
            }
        });
    }

    pub fn run_elevated(&mut self, tx: &mut Tx) {
        self.toast("Setting up elevated task…", tx);
        let handle = self.handle.clone();
        self.rt.spawn_blocking(move || {
            let msg = match configure_elevated_task() {
                Ok(m) => m,
                Err(e) => e.to_string(),
            };
            if let Some(h) = handle {
                h.update(move |s, tx| s.toast(msg, tx));
            }
        });
    }

    pub fn reset_trust(&mut self, tx: &mut Tx) {
        let tofu_path = self.config.client.resolved_tofu_path();
        let _ = std::fs::remove_file(&tofu_path);
        self.config.client.trusted_fingerprint = None;
        self.push_config();
        self.toast("Cleared saved server fingerprint", tx);
    }

    pub fn quit_app(&mut self, tx: &mut Tx) {
        let _ = self.ctrl.quit_tx.send(());
        tx.quit();
    }
}

// ---------------------------------------------------------------------------
// Entry point & event-loop wiring
// ---------------------------------------------------------------------------

fn view(s: &Ui, cx: &mut Cx<Ui>) -> El {
    let dark = s.theme.is_dark(cx.system_dark());
    let p = Palette::new(dark, cx.env().scale);
    pages::root_view(s, p)
}

fn poll(s: &mut Ui, _now: f64, _tx: &mut Tx) -> bool {
    let mut changed = false;

    if let Some((_, Some(exp))) = s.toast {
        if Instant::now() >= exp {
            s.toast = None;
            changed = true;
        }
    }

    // Commit edited Port or Server fields once they lose focus.
    if !s.port_edit.focused && !s.server_edit.focused && s.commit_text_fields() {
        s.push_config();
        changed = true;
    }

    changed
}

fn on_key(s: &mut Ui, ev: &KeyEvent, tx: &mut Tx) -> bool {
    match ev.key {
        Key::Command('q') => {
            s.quit_app(tx);
            true
        }
        _ => false,
    }
}

fn on_close(s: &mut Ui, tx: &mut Tx) {
    // If no system tray is available, closing the window quits Glide cleanly instead of
    // leaving an invisible background process.
    if !s.tray_active {
        s.quit_app(tx);
    }
}

/// Runs the native Rune window on the calling (main) thread.
pub(crate) fn run(
    ctrl: Arc<AppController>,
    rt: tokio::runtime::Handle,
    minimized: bool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    rune::set_app_id("Glide");

    #[cfg(target_os = "windows")]
    {
        let _ = rune::set_app_icon(glide_backend::icon::render_png(256));
    }

    let rune_args: Vec<String> = std::env::var("GLIDE_RUNE_ARGS")
        .ok()
        .map(|raw| raw.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let headless_snap = !rune_args.is_empty();

    let spec = AppSpec::<Ui> {
        scenes: SCENES,
        fonts: FONTS,
        families: ("Inter", "JetBrains Mono"),
        poll: Some(poll),
        on_key: Some(on_key),
        min_size: Some((GUI_WIDTH as f32, GUI_HEIGHT as f32)),
        ..AppSpec::new(
            "Glide",
            (GUI_WIDTH as f32, GUI_HEIGHT as f32),
            dummy_init,
            view,
        )
    };

    let mut app = App::new(spec)
        .args(rune_args)
        .window_options(WindowOptions {
            resizable: false,
            min_size: Some((GUI_WIDTH as f32, GUI_HEIGHT as f32)),
            max_size: Some((GUI_WIDTH as f32, GUI_HEIGHT as f32)),
            ..WindowOptions::default()
        })
        .keep_running()
        .on_close(on_close);

    if minimized {
        app = app.windowless();
    }

    #[cfg(target_os = "linux")]
    {
        app = app.on_window(|_| {
            glide_backend::linux_x11::window_brand::spawn_x11_window_brander(GUI_WIDTH, GUI_HEIGHT);
        });
    }

    let handle = app.handle();

    // Wire AppController show/quit hooks so the tray, `/api/show`, and Ctrl+C control this window.
    {
        let show_handle = handle.clone();
        let fallback_url = ctrl.url.clone();
        *ctrl.ui_show.lock() = Some(Arc::new(move || {
            show_handle.open();
            let h = show_handle.clone();
            let url = fallback_url.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1500));
                if let Some(err) = h.window_error() {
                    warn!(error = %err, "Native window could not open; launching browser fallback");
                    let _ = launch_browser_window(&url);
                }
            });
        }));

        let quit_handle = handle.clone();
        *ctrl.ui_quit.lock() = Some(Arc::new(move || {
            quit_handle.quit();
        }));
    }

    let init_ctrl = Arc::clone(&ctrl);
    let init_rt = rt.clone();
    let init_handle = handle.clone();
    app = app.init(move || {
        let mut ui = Ui::new(init_ctrl, init_rt);
        ui.handle = Some(init_handle);
        ui
    });

    if !headless_snap {
        spawn_runtime_bridge(Arc::clone(&ctrl), &rt, handle.clone());
        spawn_discovery_loop(Arc::clone(&ctrl), &rt, handle);
    }

    app.try_run()
}

fn dummy_init() -> Ui {
    unreachable!("App::init closure always constructs Ui")
}

fn spawn_runtime_bridge(
    ctrl: Arc<AppController>,
    rt: &tokio::runtime::Handle,
    handle: AppHandle<Ui>,
) {
    let mut rx = ctrl.runtime.subscribe();
    rt.spawn(async move {
        while let Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) =
            rx.recv().await
        {
            let snap = ctrl.runtime.snapshot();
            handle.update(move |ui, _| ui.apply_status(snap));
        }
    });
}

fn spawn_discovery_loop(
    ctrl: Arc<AppController>,
    rt: &tokio::runtime::Handle,
    handle: AppHandle<Ui>,
) {
    rt.spawn(async move {
        let mut first = true;
        loop {
            let snap = ctrl.runtime.snapshot();
            let running = snap.mode == "server" || snap.mode == "client";
            let connected = snap.status.starts_with("Connected");
            if first || !running || (snap.mode == "client" && !connected) {
                first = false;
                let (servers, ips) = run_discovery(&ctrl).await;
                handle.update(move |ui, _| {
                    ui.local_ips = ips;
                    ui.discovered = servers;
                });
            }
            tokio::time::sleep(Duration::from_millis(4500)).await;
        }
    });
}

async fn run_discovery(ctrl: &AppController) -> (Vec<DiscoveredPeer>, Vec<String>) {
    let port = ctrl.config.lock().client.discovery_port;
    let my_fp = ctrl.fingerprint.lock().clone();
    let found = discover_servers(port, Duration::from_millis(2000), None, false)
        .await
        .unwrap_or_default();
    let list = found
        .into_iter()
        .filter(|s| my_fp.as_deref() != Some(s.fingerprint.as_str()))
        .map(|s| DiscoveredPeer {
            name: s.name,
            os: parse_os_str(&s.os),
            addr: s.addr.to_string(),
            fingerprint: s.fingerprint,
        })
        .collect();
    (list, local_ipv4_addresses())
}

fn os_label(os: OsKind) -> &'static str {
    match os {
        OsKind::Windows => "Windows",
        OsKind::LinuxWayland => "Linux (Wayland)",
        OsKind::LinuxX11 => "Linux",
        OsKind::MacOs => "macOS",
        OsKind::Unknown => "Unknown",
    }
}

fn parse_os_str(raw: &str) -> OsKind {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("win") {
        OsKind::Windows
    } else if lower.contains("wayland") {
        OsKind::LinuxWayland
    } else if lower.contains("linux") {
        OsKind::LinuxX11
    } else if lower.contains("mac") {
        OsKind::MacOs
    } else {
        OsKind::Unknown
    }
}

// ---------------------------------------------------------------------------
// Headless visual verification scenes (`GLIDE_RUNE_ARGS="--snap out.png --scene ..."`)
// ---------------------------------------------------------------------------

fn scene_live_server(s: &mut Ui) {
    s.tab = Tab::Devices;
    s.role = Role::Server;
    s.hostname = "workstation".into();
    s.os_kind = OsKind::Windows;
    s.os_label = "Windows";
    s.local_ips = vec!["192.168.1.42".into()];
    s.status.mode = "server".into();
    s.status.status = "Connected (1 peer(s))".into();
    s.status.active_target = "Local".into();
    s.status.rtt_ms = Some(1.4);
    s.status.rtt_history = vec![1.6, 1.5, 1.4, 1.8, 1.3, 1.4, 1.5, 1.2, 1.4, 1.3, 1.5, 1.4];
    s.status.peers = vec![PeerSummary {
        name: "ubuntu-dev".into(),
        os: OsKind::LinuxX11,
        edge: Edge::Right,
        addr: "192.168.1.88:51234".into(),
        rtt_ms: Some(1.4),
    }];
}

fn scene_client_connected(s: &mut Ui) {
    s.tab = Tab::Devices;
    s.role = Role::Client;
    s.hostname = "ubuntu-dev".into();
    s.os_kind = OsKind::LinuxX11;
    s.os_label = "Linux";
    s.status.mode = "client".into();
    s.status.status = "Connected to workstation".into();
    s.status.active_target = "Local".into();
    s.status.rtt_ms = Some(1.2);
    s.status.rtt_history = vec![1.3, 1.2, 1.1, 1.4, 1.2, 1.2, 1.1, 1.2];
    s.config.client.edge = Edge::Right;
    s.config.client.server = Some("192.168.1.42:24800".into());
    s.server_edit = TextEdit::new("192.168.1.42:24800");
}

fn scene_clipboard_filled(s: &mut Ui) {
    s.tab = Tab::Clipboard;
    s.status.clipboard_history = vec![
        ClipboardHistoryEntry {
            id: 1,
            format: ClipboardFormat::TextUtf8,
            preview: "cargo build --release --bin glide".into(),
            bytes: 33,
            direction: "sent".into(),
            payload: None,
        },
        ClipboardHistoryEntry {
            id: 2,
            format: ClipboardFormat::ImagePng,
            preview: "Screenshot 1920x1080".into(),
            bytes: 428_500,
            direction: "received".into(),
            payload: None,
        },
    ];
    s.status.recent_files = vec![
        FileTransferEntry {
            filename: "glide-0.3.0-amd64.deb".into(),
            bytes: 5_242_880,
            direction: "sent".into(),
            saved_path: None,
        },
        FileTransferEntry {
            filename: "notes.md".into(),
            bytes: 4_820,
            direction: "received".into(),
            saved_path: None,
        },
    ];
}

type SceneFn = fn(&mut Ui);

const SCENES: &[(&str, SceneFn)] = &[
    ("server-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Devices;
        s.role = Role::Server;
    }),
    ("server-dark", |s| {
        s.theme = ThemePref::Dark;
        s.tab = Tab::Devices;
        s.role = Role::Server;
    }),
    ("server-live", |s| {
        s.theme = ThemePref::Light;
        scene_live_server(s);
    }),
    ("server-live-dark", |s| {
        s.theme = ThemePref::Dark;
        scene_live_server(s);
    }),
    ("client-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Devices;
        s.role = Role::Client;
    }),
    ("client-dark", |s| {
        s.theme = ThemePref::Dark;
        s.tab = Tab::Devices;
        s.role = Role::Client;
    }),
    ("client-connected", |s| {
        s.theme = ThemePref::Light;
        scene_client_connected(s);
    }),
    ("input-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Input;
    }),
    ("input-dark", |s| {
        s.theme = ThemePref::Dark;
        s.tab = Tab::Input;
        s.config.client.key_remap.push(KeyRemapRule {
            from: "CapsLock".into(),
            to: "Escape".into(),
        });
    }),
    ("clipboard-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Clipboard;
    }),
    ("clipboard-dark", |s| {
        s.theme = ThemePref::Dark;
        s.tab = Tab::Clipboard;
    }),
    ("clipboard-filled", |s| {
        s.theme = ThemePref::Light;
        scene_clipboard_filled(s);
    }),
    ("system-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::System;
        s.doctor_display = "DISPLAY=:0 (screen 0)".into();
        s.doctor_text = "OS       Linux (LinuxX11)\nDisplay  DISPLAY=:0 (screen 0)\n\n• Native X11 session detected.\n• XTest extension: OK (v2.2)\n• XInput2 extension: OK (v2.2)\n• XFixes extension: OK (v5.0)".into();
    }),
    ("system-dark", |s| {
        s.theme = ThemePref::Dark;
        s.tab = Tab::System;
        s.doctor_display = "DISPLAY=:0 (screen 0)".into();
        s.doctor_text = "OS       Linux (LinuxX11)\nDisplay  DISPLAY=:0 (screen 0)\n\n• Native X11 session detected.\n• XTest extension: OK (v2.2)\n• XInput2 extension: OK (v2.2)\n• XFixes extension: OK (v5.0)".into();
    }),
];
