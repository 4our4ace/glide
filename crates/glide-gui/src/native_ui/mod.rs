//! Native GPU-rendered Glide GUI powered by Rune (wgpu + winit).

mod icons;
mod pages;
mod theme;
mod widgets;

use crate::updater::{self, UpdateCheckState};
use crate::{launch_browser_window, AppController, GUI_HEIGHT, GUI_WIDTH};
use glide_backend::hotkeys::{
    arrow_to_edge, CaptureHotkeys, HidUsage, HotkeyChord, HotkeyMods, ModifierMask,
};
use glide_backend::os_integration::{
    configure_elevated_task, configure_firewall, ensure_receive_dir, is_autostart_enabled,
    open_path_in_os, resolve_receive_dir, reveal_in_file_manager, set_autostart,
};
use glide_core::config::{
    list_known_servers, remove_known_server, GlideConfigFile, KeyRemapRule, KnownServer,
    UpdateChannel,
};
use glide_core::runtime::{
    read_regular_file_bounded, ClipboardHistoryEntry, FileTransferEntry, PeerSummary,
    RuntimeCommand, StatusSnapshot,
};
use glide_net::discovery::{
    discover_servers, local_ipv4_addresses, local_ipv4_addresses_async, machine_hostname,
};
use glide_proto::{ClipboardFormat, Edge, OsKind};
use rune::{
    App, AppHandle, AppSpec, Cx, El, Key, KeyEvent, Mods, PhysicalInput, PhysicalKeyCode, TextEdit,
    Tx, WindowOptions,
};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    Settings,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPeer {
    pub name: String,
    pub os: OsKind,
    pub addr: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutSlot {
    Jump,
    Lock,
    Home,
}

impl ShortcutSlot {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Jump => "Jump to a side",
            Self::Lock => "Lock current screen",
            Self::Home => "Bring cursor home",
        }
    }

    pub const fn recorder_key(self) -> &'static str {
        match self {
            Self::Jump => "sc-jump-rec",
            Self::Lock => "sc-lock-rec",
            Self::Home => "sc-home-rec",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ShortcutRecorderState {
    pub active: Option<ShortcutSlot>,
    pub held_mods: HotkeyMods,
    pub altgr_held: bool,
    pub swallow_key: Option<PhysicalKeyCode>,
    held_phys: ModifierMask,
}

/// Tracks whether the native Rune window is currently open/visible so background
/// loops (`spawn_runtime_bridge` and `spawn_discovery_loop`) can park with zero CPU,
/// zero UI wakeups, and zero LAN scans while Glide is minimized to the system tray.
struct WindowVisibility {
    visible: AtomicBool,
    last_pushed_rev: AtomicU64,
    changed_tx: tokio::sync::watch::Sender<bool>,
}

impl WindowVisibility {
    fn new(initial_visible: bool) -> Self {
        let (changed_tx, _) = tokio::sync::watch::channel(initial_visible);
        Self {
            visible: AtomicBool::new(initial_visible),
            last_pushed_rev: AtomicU64::new(0),
            changed_tx,
        }
    }

    fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    /// Updates visibility and notifies parked background loops on state transitions.
    /// Returns `true` when transitioning from hidden (`false`) to visible (`true`).
    fn set_visible(&self, visible: bool) -> bool {
        let prev = self.visible.swap(visible, Ordering::AcqRel);
        if prev != visible {
            let _ = self.changed_tx.send(visible);
        }
        !prev && visible
    }

    fn record_rev(&self, rev: u64) {
        self.last_pushed_rev.fetch_max(rev, Ordering::AcqRel);
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.changed_tx.subscribe()
    }

    /// Pushes a fresh [`StatusSnapshot`] to the UI thread when `ctrl.runtime` has
    /// advanced past the last revision pushed through this tracker.
    fn push_fresh_snapshot(&self, ctrl: &AppController, handle: &AppHandle<Ui>) {
        let cur_rev = ctrl.runtime.revision();
        if self.last_pushed_rev.load(Ordering::Acquire) >= cur_rev {
            return;
        }
        let (rev, snap) = ctrl.runtime.snapshot_with_rev();
        self.last_pushed_rev.fetch_max(rev, Ordering::AcqRel);
        handle.update_if(move |ui, _| ui.apply_status_if_newer(rev, snap));
    }
}

pub struct Ui {
    ctrl: Arc<AppController>,
    rt: tokio::runtime::Handle,
    handle: Option<AppHandle<Ui>>,
    window_vis: Arc<WindowVisibility>,
    status_rev: u64,

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
    pub known_servers: Vec<KnownServer>,
    pub saved_servers_expanded: bool,
    pub scanning: bool,
    pub show_advanced: bool,
    pub show_nightly_warning: bool,
    pub update_state: UpdateCheckState,

    // Editable text fields + last committed snapshots
    pub port_edit: TextEdit,
    pub server_edit: TextEdit,
    pub max_file_mb_edit: TextEdit,
    pub receive_dir_edit: TextEdit,
    pub remap_from: TextEdit,
    pub remap_to: TextEdit,
    pub sudo_password: TextEdit,
    pub sudo_submitting: bool,
    last_committed_port: String,
    last_committed_server: String,
    last_committed_max_file_mb: String,
    last_committed_receive_dir: String,

    // Transient UI state
    toast: Option<(String, Option<Instant>)>,
    pub dismissed_error: Option<String>,
    pub desk_drag: Option<DeskDrag>,
    pub drop_hover: bool,

    // Interactive shortcut recorder + remembered non-disabled bindings
    pub shortcut_recorder: ShortcutRecorderState,
    pub last_switch_mods: Option<HotkeyMods>,
    pub last_lock_chord: Option<HotkeyChord>,
    pub last_escape_chord: Option<HotkeyChord>,
}

impl Ui {
    fn new(
        ctrl: Arc<AppController>,
        rt: tokio::runtime::Handle,
        window_vis: Arc<WindowVisibility>,
    ) -> Self {
        let os_kind = glide_backend::current_os_kind();
        let os_label = os_label(os_kind);
        let is_windows = matches!(os_kind, OsKind::Windows);
        let hostname = machine_hostname();
        let cfg = ctrl.config.lock().clone();
        let parsed_hotkeys = cfg.server.parsed_hotkeys();
        let (status_rev, status) = ctrl.runtime.snapshot_with_rev();
        window_vis.record_rev(status_rev);
        let fingerprint = ctrl.fingerprint.lock().clone();
        let tray_active = ctrl.tray_active.load(Ordering::SeqCst);
        let autostart = is_autostart_enabled();
        let local_ips = local_ipv4_addresses();
        let known_servers = list_known_servers(&ctrl.known_servers_path()).unwrap_or_default();

        let doctor_display = "Detecting display…".to_owned();
        let doctor_text =
            format!("OS       {os_label} ({os_kind:?})\nChecking display & input extensions…");

        let port_str = cfg.server.bind.port().to_string();
        let server_str = cfg.client.server.clone().unwrap_or_default();
        let max_file_mb_str = cfg.max_file_mb_clamped().to_string();
        let receive_dir_str = cfg.receive_dir.clone().unwrap_or_default();
        let role = if status.mode == "client" {
            Role::Client
        } else if status.mode == "server" {
            Role::Server
        } else if cfg.last_role.as_deref() == Some("client") {
            Role::Client
        } else {
            Role::Server
        };

        Self {
            ctrl,
            rt,
            handle: None,
            window_vis,
            status_rev,
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
            known_servers,
            saved_servers_expanded: false,
            scanning: false,
            show_advanced: false,
            show_nightly_warning: false,
            update_state: UpdateCheckState::Idle,
            port_edit: TextEdit::new(&port_str),
            server_edit: TextEdit::new(&server_str),
            max_file_mb_edit: TextEdit::new(&max_file_mb_str),
            receive_dir_edit: TextEdit::new(&receive_dir_str),
            remap_from: TextEdit::default(),
            remap_to: TextEdit::default(),
            sudo_password: TextEdit::default(),
            sudo_submitting: false,
            last_committed_port: port_str,
            last_committed_server: server_str,
            last_committed_max_file_mb: max_file_mb_str,
            last_committed_receive_dir: receive_dir_str,
            toast: None,
            dismissed_error: None,
            desk_drag: None,
            drop_hover: false,
            shortcut_recorder: ShortcutRecorderState::default(),
            last_switch_mods: parsed_hotkeys.switch_mods,
            last_lock_chord: parsed_hotkeys.lock,
            last_escape_chord: parsed_hotkeys.escape,
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

    pub fn client_server_label(&self) -> &str {
        if let Some(rest) = self.status.status.strip_prefix("Connected to ") {
            return rest;
        }
        let target = self.status.active_target.as_str();
        if !target.is_empty() && target != "Local" && target != "Local (Controlling)" {
            return target;
        }
        "Server"
    }

    pub fn sidebar_status_text(&self) -> Cow<'_, str> {
        let raw = if self.status.status.is_empty() {
            "Idle"
        } else {
            self.status.status.as_str()
        };
        if let Some(rest) = raw.strip_prefix("Connected (") {
            if let Some(num) = rest.strip_suffix(" peer(s))") {
                let word = if num == "1" { "client" } else { "clients" };
                return Cow::Owned(format!("Connected · {num} {word}"));
            }
        }
        Cow::Borrowed(raw)
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
        let newly_connected_client = snap.mode == "client"
            && snap.status.starts_with("Connected")
            && (self.status.status != snap.status || self.status.local_addr != snap.local_addr);
        if snap.mode == "server" {
            self.role = Role::Server;
        } else if snap.mode == "client" {
            self.role = Role::Client;
        }
        if !snap.local_ips.is_empty() {
            self.local_ips = snap.local_ips.clone();
        }
        if snap.last_error.is_none() {
            self.dismissed_error = None;
        }
        self.status = snap;
        if newly_connected_client {
            self.refresh_known_servers();
        }
    }

    pub fn refresh_known_servers(&mut self) {
        self.known_servers = list_known_servers(&self.ctrl.known_servers_path()).unwrap_or_default();
        if self.known_servers.is_empty() {
            self.saved_servers_expanded = false;
        }
    }

    fn apply_status_if_newer(&mut self, rev: u64, snap: StatusSnapshot) -> bool {
        if rev <= self.status_rev {
            return false;
        }
        self.status_rev = rev;
        self.apply_status(snap);
        true
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
        if self.role != role {
            self.role = role;
            self.config.last_role = Some(role.as_str().to_owned());
            self.push_config();
        }
    }

    fn commit_text_fields_inner(&mut self, include_focused: bool) -> bool {
        let mut changed = false;
        if include_focused || !self.port_edit.focused {
            let port_raw = self.port_edit.text.trim();
            if port_raw != self.last_committed_port {
                if let Ok(p) = port_raw.parse::<u16>() {
                    if p >= 1024 {
                        self.config.server.bind.set_port(p);
                        self.last_committed_port.clear();
                        self.last_committed_port.push_str(port_raw);
                        changed = true;
                    }
                }
            }
        }
        if include_focused || !self.server_edit.focused {
            let srv_raw = self.server_edit.text.trim();
            if srv_raw != self.last_committed_server {
                self.config.client.server = if srv_raw.is_empty() {
                    None
                } else {
                    Some(srv_raw.to_owned())
                };
                self.config.client.trusted_fingerprint = None;
                self.last_committed_server.clear();
                self.last_committed_server.push_str(srv_raw);
                changed = true;
            }
        }
        if include_focused || !self.max_file_mb_edit.focused {
            changed |= self.commit_max_file_mb();
        }
        if include_focused || !self.receive_dir_edit.focused {
            changed |= self.commit_receive_dir();
        }
        changed
    }

    pub fn commit_text_fields(&mut self) -> bool {
        self.commit_text_fields_inner(false)
    }

    pub fn commit_all_text_fields(&mut self) -> bool {
        self.commit_text_fields_inner(true)
    }

    pub fn commit_max_file_mb(&mut self) -> bool {
        let raw = self.max_file_mb_edit.text.trim().to_owned();
        if raw == self.last_committed_max_file_mb {
            return false;
        }
        if let Ok(parsed) = raw.parse::<u32>() {
            let clamped = GlideConfigFile::clamp_max_file_mb(parsed);
            let normalized = clamped.to_string();
            self.config.max_file_mb = clamped;
            if self.max_file_mb_edit.text != normalized {
                self.max_file_mb_edit = TextEdit::new(&normalized);
            }
            self.last_committed_max_file_mb = normalized;
            true
        } else {
            let prev = self.last_committed_max_file_mb.clone();
            self.max_file_mb_edit = TextEdit::new(&prev);
            false
        }
    }

    pub fn commit_receive_dir(&mut self) -> bool {
        let raw = self.receive_dir_edit.text.trim().to_owned();
        if raw == self.last_committed_receive_dir {
            return false;
        }
        if raw.is_empty() {
            self.config.receive_dir = None;
            if !self.receive_dir_edit.text.is_empty() {
                self.receive_dir_edit = TextEdit::new("");
            }
            self.last_committed_receive_dir.clear();
            return true;
        }
        if resolve_receive_dir(Some(&raw)).is_ok() {
            self.config.receive_dir = Some(raw.clone());
            if self.receive_dir_edit.text != raw {
                self.receive_dir_edit = TextEdit::new(&raw);
            }
            self.last_committed_receive_dir = raw;
            true
        } else {
            let prev = self.last_committed_receive_dir.clone();
            self.receive_dir_edit = TextEdit::new(&prev);
            false
        }
    }

    pub fn commit_text_fields_and_push(&mut self) {
        if self.commit_all_text_fields() {
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
            let role = self.role;
            self.commit_all_text_fields();
            self.config.last_role = Some(role.as_str().to_owned());
            self.config.auto_start_on_launch = true;
            self.push_config();
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
        let port = self.current_port();
        let addr = if port == glide_core::config::DEFAULT_QUIC_PORT {
            first.clone()
        } else {
            format!("{first}:{port}")
        };
        tx.copy(addr.clone());
        self.toast(format!("Copied {addr}"), tx);
    }

    pub fn set_update_channel(&mut self, channel: UpdateChannel, tx: &mut Tx) {
        if self.config.update_channel == channel {
            self.show_nightly_warning = false;
            return;
        }
        if matches!(
            self.update_state,
            UpdateCheckState::Downloading { .. }
                | UpdateCheckState::Installing { .. }
                | UpdateCheckState::AwaitingSudoPassword { .. }
        ) {
            return;
        }
        if channel == UpdateChannel::Nightly && self.config.update_channel == UpdateChannel::Stable
        {
            self.show_nightly_warning = true;
            return;
        }
        self.apply_update_channel_switch(channel, tx);
    }

    pub fn cancel_switch_to_nightly(&mut self) {
        self.show_nightly_warning = false;
    }

    pub fn confirm_switch_to_nightly(&mut self, tx: &mut Tx) {
        self.show_nightly_warning = false;
        if self.config.update_channel == UpdateChannel::Nightly {
            return;
        }
        if matches!(
            self.update_state,
            UpdateCheckState::Downloading { .. }
                | UpdateCheckState::Installing { .. }
                | UpdateCheckState::AwaitingSudoPassword { .. }
        ) {
            return;
        }
        self.apply_update_channel_switch(UpdateChannel::Nightly, tx);
    }

    fn apply_update_channel_switch(&mut self, channel: UpdateChannel, tx: &mut Tx) {
        self.show_nightly_warning = false;
        self.config.update_channel = channel;
        self.push_config();
        self.update_state = UpdateCheckState::Idle;
        self.check_for_updates(tx);
    }

    pub fn check_for_updates(&mut self, tx: &mut Tx) {
        if matches!(
            self.update_state,
            UpdateCheckState::Checking
                | UpdateCheckState::Downloading { .. }
                | UpdateCheckState::Installing { .. }
                | UpdateCheckState::AwaitingSudoPassword { .. }
        ) {
            return;
        }
        let channel = self.config.update_channel;
        self.update_state = UpdateCheckState::Checking;
        let _ = tx;
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let current = env!("CARGO_PKG_VERSION");
        self.rt.spawn(async move {
            let state =
                tokio::task::spawn_blocking(move || updater::check_release(channel, current))
                    .await
                    .unwrap_or_else(|_| UpdateCheckState::Error("Update check failed".into()));
            handle.update(move |s, tx| {
                if s.config.update_channel != channel {
                    return;
                }
                match &state {
                    UpdateCheckState::UpToDate { latest } => {
                        s.toast(format!("Glide {latest} is up to date"), tx);
                    }
                    UpdateCheckState::Available(avail) => {
                        if avail.is_channel_switch {
                            s.toast(format!("Ready to switch to Glide {}", avail.version), tx);
                        } else {
                            s.toast(format!("Glide {} is available", avail.version), tx);
                        }
                    }
                    UpdateCheckState::Error(msg) => {
                        s.toast(msg.clone(), tx);
                    }
                    _ => {}
                }
                s.update_state = state;
            });
        });
    }

    pub fn install_update(&mut self, tx: &mut Tx) {
        let avail = match &self.update_state {
            UpdateCheckState::Available(avail) => (**avail).clone(),
            _ => return,
        };
        if !avail.can_self_update() {
            self.open_update_download(tx);
            return;
        }

        let version = avail.version.clone();
        self.update_state = UpdateCheckState::Downloading {
            version: version.clone(),
            progress_pct: 0,
        };
        self.toast(format!("Downloading Glide {version}…"), tx);

        let handle = self.handle.clone();
        let ctrl = Arc::clone(&self.ctrl);
        let current = env!("CARGO_PKG_VERSION");
        self.rt.spawn(async move {
            let h_prog = handle.clone();
            let v_prog = version.clone();
            let h_inst = handle.clone();
            let v_inst = version.clone();

            let res = tokio::task::spawn_blocking(move || {
                updater::download_and_install_update(
                    &avail,
                    current,
                    false,
                    move |pct| {
                        if let Some(ref h) = h_prog {
                            let ver = v_prog.clone();
                            h.update_if(move |s, _| {
                                if let UpdateCheckState::Downloading { progress_pct, .. } =
                                    &mut s.update_state
                                {
                                    if *progress_pct == pct {
                                        return false;
                                    }
                                    *progress_pct = pct;
                                    true
                                } else {
                                    s.update_state = UpdateCheckState::Downloading {
                                        version: ver,
                                        progress_pct: pct,
                                    };
                                    true
                                }
                            });
                        }
                    },
                    move || {
                        if let Some(ref h) = h_inst {
                            let ver = v_inst.clone();
                            h.update(move |s, _| {
                                s.update_state = UpdateCheckState::Installing { version: ver };
                            });
                        }
                    },
                )
            })
            .await
            .unwrap_or_else(|_| Err("Update task panicked".into()));

            match res {
                Ok(updater::UpdateInstallOutcome::InstalledRelaunch(relaunch_exe)) => {
                    *ctrl.relaunch_after_exit.lock() = Some(relaunch_exe);
                    if let Some(h) = handle {
                        h.update(move |s, tx| {
                            s.quit_app(tx);
                        });
                    }
                }
                Ok(updater::UpdateInstallOutcome::ExternalInstallerLaunched) => {
                    if let Some(h) = handle {
                        h.update(move |s, tx| {
                            s.quit_app(tx);
                        });
                    }
                }
                Ok(updater::UpdateInstallOutcome::NeedsSudoPassword {
                    deb_path,
                    stage_dir,
                    username,
                }) => {
                    if let Some(h) = handle {
                        let ver = version.clone();
                        h.update(move |s, _| {
                            updater::zeroize_string(&mut s.sudo_password.text);
                            s.sudo_password.clear();
                            s.sudo_submitting = false;
                            s.update_state = UpdateCheckState::AwaitingSudoPassword {
                                version: ver,
                                deb_path,
                                stage_dir,
                                username,
                                error: None,
                            };
                        });
                    } else {
                        updater::cleanup_staged_dir(&stage_dir);
                    }
                }
                Err(msg) => {
                    if let Some(h) = handle {
                        h.update(move |s, tx| {
                            s.toast(msg.clone(), tx);
                            s.update_state = UpdateCheckState::Error(msg);
                        });
                    }
                }
            }
        });
    }

    pub fn submit_sudo_update(&mut self, tx: &mut Tx) {
        if self.sudo_submitting {
            return;
        }
        let (version, deb_path, stage_dir, username) = match &self.update_state {
            UpdateCheckState::AwaitingSudoPassword {
                version,
                deb_path,
                stage_dir,
                username,
                ..
            } => (
                version.clone(),
                deb_path.clone(),
                stage_dir.clone(),
                username.clone(),
            ),
            _ => return,
        };

        let mut password = std::mem::take(&mut self.sudo_password.text);
        self.sudo_password.clear();
        if password.is_empty() {
            self.update_state = UpdateCheckState::AwaitingSudoPassword {
                version,
                deb_path,
                stage_dir,
                username,
                error: Some("Please enter your sudo password.".into()),
            };
            return;
        }

        self.sudo_submitting = true;
        self.update_state = UpdateCheckState::AwaitingSudoPassword {
            version: version.clone(),
            deb_path: deb_path.clone(),
            stage_dir: stage_dir.clone(),
            username: username.clone(),
            error: None,
        };
        self.toast(format!("Installing Glide {version}…"), tx);

        let handle = self.handle.clone();
        let ctrl = Arc::clone(&self.ctrl);
        self.rt.spawn(async move {
            let dp = deb_path.clone();
            let sd = stage_dir.clone();
            let res = tokio::task::spawn_blocking(move || {
                updater::install_staged_deb_with_sudo(&dp, &sd, &mut password)
            })
            .await
            .unwrap_or_else(|_| {
                updater::cleanup_staged_dir(&stage_dir);
                Err(updater::SudoInstallError::Fatal(
                    "Sudo update task panicked".into(),
                ))
            });

            match res {
                Ok(relaunch_exe) => {
                    *ctrl.relaunch_after_exit.lock() = Some(relaunch_exe);
                    if let Some(h) = handle {
                        h.update(move |s, tx| {
                            s.sudo_submitting = false;
                            s.update_state = UpdateCheckState::Installing { version };
                            s.quit_app(tx);
                        });
                    }
                }
                Err(updater::SudoInstallError::WrongPassword(msg)) => {
                    if let Some(h) = handle {
                        h.update(move |s, _| {
                            s.sudo_submitting = false;
                            s.update_state = UpdateCheckState::AwaitingSudoPassword {
                                version,
                                deb_path,
                                stage_dir,
                                username,
                                error: Some(msg),
                            };
                        });
                    } else {
                        updater::cleanup_staged_dir(&stage_dir);
                    }
                }
                Err(updater::SudoInstallError::Fatal(msg)) => {
                    if let Some(h) = handle {
                        h.update(move |s, tx| {
                            s.sudo_submitting = false;
                            s.toast(msg.clone(), tx);
                            s.update_state = UpdateCheckState::Error(msg);
                        });
                    }
                }
            }
        });
    }

    pub fn cancel_sudo_update(&mut self, tx: &mut Tx) {
        if self.sudo_submitting {
            return;
        }
        updater::zeroize_string(&mut self.sudo_password.text);
        self.sudo_password.clear();
        if let UpdateCheckState::AwaitingSudoPassword { stage_dir, .. } =
            std::mem::replace(&mut self.update_state, UpdateCheckState::Idle)
        {
            updater::cleanup_staged_dir(&stage_dir);
            self.toast("Update cancelled", tx);
        }
    }

    pub fn open_update_download(&mut self, tx: &mut Tx) {
        let url = match &self.update_state {
            UpdateCheckState::Available(avail) => avail.html_url.clone(),
            _ => updater::RELEASES_PAGE_URL.to_owned(),
        };
        match updater::open_url(&url) {
            Ok(()) => self.toast("Opened releases page in browser", tx),
            Err(e) => self.toast(format!("Could not open browser: {e}"), tx),
        }
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
        self.config.last_role = Some("client".into());
        self.config.auto_start_on_launch = true;
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

    pub fn connect_known_server(&mut self, srv: KnownServer, tx: &mut Tx) {
        self.role = Role::Client;
        self.config.last_role = Some("client".into());
        self.config.auto_start_on_launch = true;
        self.config.client.server = Some(srv.address.clone());
        self.config.client.trusted_fingerprint = srv.fingerprint.clone();
        self.config.client.edge = srv.edge;
        self.server_edit = TextEdit::new(&srv.address);
        self.last_committed_server = srv.address.clone();
        self.push_config();
        self.toast(format!("Connecting to {}…", srv.name), tx);

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

    pub fn forget_known_server(&mut self, addr: String, tx: &mut Tx) {
        let path = self.ctrl.known_servers_path();
        match remove_known_server(&path, &addr) {
            Ok(true) => {
                self.refresh_known_servers();
                self.toast("Removed saved server", tx);
            }
            Ok(false) => {
                self.refresh_known_servers();
            }
            Err(e) => {
                self.toast(format!("Could not remove server: {e}"), tx);
            }
        }
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

    pub fn toggle_clipboard_history(&mut self, tx: &mut Tx) {
        let next = !self.config.clipboard_history;
        self.config.clipboard_history = next;
        if !next {
            self.status.clipboard_history.clear();
        }
        self.push_config();
        self.toast(
            if next {
                "Clipboard history on"
            } else {
                "Clipboard history off & cleared"
            },
            tx,
        );
    }

    pub fn toggle_hide_clipboard_history(&mut self) {
        self.config.hide_clipboard_history = !self.config.hide_clipboard_history;
        self.push_config();
    }

    pub fn toggle_drag_drop_files(&mut self, tx: &mut Tx) {
        let next = !self.config.drag_drop_files;
        self.config.drag_drop_files = next;
        self.config.server.drag_drop_files = next;
        self.config.client.drag_drop_files = next;
        self.push_config();
        self.toast(
            if next {
                "Edge file drag-and-drop on"
            } else {
                "Edge file drag-and-drop off"
            },
            tx,
        );
    }

    pub fn clear_clipboard_history(&mut self, tx: &mut Tx) {
        self.ctrl.runtime.clear_clipboard_history();
        self.status.clipboard_history.clear();
        self.toast("Clipboard history cleared", tx);
    }

    pub fn send_file_paths(&mut self, paths: Vec<PathBuf>, tx: &mut Tx) {
        if !self.is_running() {
            self.toast("Connect first, then send files", tx);
            return;
        }
        if paths.is_empty() {
            return;
        }
        let max_file_bytes = self.config.max_file_bytes();
        let ctrl = Arc::clone(&self.ctrl);
        let handle = self.handle.clone();
        self.rt.spawn_blocking(move || {
            for path in paths {
                let display_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("glide-transfer.bin")
                    .to_owned();
                if let Some(ref h) = handle {
                    let msg = format!("Sending {display_name}…");
                    h.update(move |s, tx| s.toast(msg, tx));
                }
                match read_regular_file_bounded(&path, max_file_bytes) {
                    Ok((filename, data)) => {
                        let ok = ctrl.send_cmd(RuntimeCommand::SendFile {
                            filename: filename.clone(),
                            data,
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
                            let msg = format!("Could not send {display_name}: {e}");
                            h.update(move |s, tx| s.toast(msg, tx));
                        }
                    }
                }
            }
        });
        let _ = tx;
    }

    pub fn reset_receive_dir(&mut self, tx: &mut Tx) {
        self.config.receive_dir = None;
        self.receive_dir_edit = TextEdit::new("");
        self.last_committed_receive_dir.clear();
        self.push_config();
        self.toast("Save folder reset to Downloads/Glide", tx);
    }

    pub fn open_receive_folder(&mut self, tx: &mut Tx) {
        self.commit_text_fields_and_push();
        let custom = self.config.receive_dir.clone();
        let handle = self.handle.clone();
        let _ = tx;
        self.rt.spawn_blocking(move || {
            let res =
                ensure_receive_dir(custom.as_deref()).and_then(|dir| open_path_in_os(&dir));
            if let Err(e) = res {
                if let Some(h) = handle {
                    let msg = format!("Could not open folder: {e}");
                    h.update(move |s, tx| s.toast(msg, tx));
                }
            }
        });
    }

    pub fn open_received_file(&mut self, path: String, tx: &mut Tx) {
        let handle = self.handle.clone();
        let _ = tx;
        self.rt.spawn_blocking(move || {
            if let Err(e) = open_path_in_os(std::path::Path::new(&path)) {
                if let Some(h) = handle {
                    let msg = format!("Could not open file: {e}");
                    h.update(move |s, tx| s.toast(msg, tx));
                }
            }
        });
    }

    pub fn reveal_received_file(&mut self, path: String, tx: &mut Tx) {
        let handle = self.handle.clone();
        let _ = tx;
        self.rt.spawn_blocking(move || {
            if let Err(e) = reveal_in_file_manager(std::path::Path::new(&path)) {
                if let Some(h) = handle {
                    let msg = format!("Could not open folder: {e}");
                    h.update(move |s, tx| s.toast(msg, tx));
                }
            }
        });
    }

    pub fn toggle_autostart(&mut self, tx: &mut Tx) {
        let next = !self.autostart;
        self.autostart = next;
        if next {
            self.config.last_role = Some(self.role.as_str().to_owned());
            self.config.auto_start_on_launch = true;
            self.push_config();
        }
        let args = "gui --minimized".to_owned();
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

    pub fn set_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.stop_recording_shortcut();
            self.shortcut_recorder.swallow_key = None;
            self.tab = tab;
        }
    }

    pub fn start_recording_shortcut(&mut self, slot: ShortcutSlot) {
        self.shortcut_recorder.active = Some(slot);
        self.shortcut_recorder.held_mods = HotkeyMods::EMPTY;
        self.shortcut_recorder.altgr_held = false;
        self.shortcut_recorder.swallow_key = None;
        self.shortcut_recorder.held_phys = ModifierMask::EMPTY;
        self.ctrl.set_hotkeys_suspended(true);
    }

    pub fn stop_recording_shortcut(&mut self) {
        if self.shortcut_recorder.active.take().is_some() {
            self.shortcut_recorder.held_mods = HotkeyMods::EMPTY;
            self.shortcut_recorder.altgr_held = false;
            self.shortcut_recorder.held_phys = ModifierMask::EMPTY;
            self.ctrl.set_hotkeys_suspended(false);
        }
    }

    fn sync_phys_mods_with_event(&mut self, mods: Mods) {
        if !mods.ctrl && !(!cfg!(target_os = "macos") && mods.cmd) {
            self.shortcut_recorder.held_phys.0 &=
                !(ModifierMask::CONTROL_LEFT | ModifierMask::CONTROL_RIGHT);
        }
        if !mods.alt {
            self.shortcut_recorder.held_phys.0 &= !ModifierMask::ALT_LEFT;
        }
        if !mods.shift {
            self.shortcut_recorder.held_phys.0 &=
                !(ModifierMask::SHIFT_LEFT | ModifierMask::SHIFT_RIGHT);
        }
        if !mods.logo && !(cfg!(target_os = "macos") && mods.cmd) {
            self.shortcut_recorder.held_phys.0 &=
                !(ModifierMask::META_LEFT | ModifierMask::META_RIGHT);
        }
    }

    pub fn set_shortcut_enabled(&mut self, slot: ShortcutSlot, enabled: bool, tx: &mut Tx) {
        let cur = self.config.server.parsed_hotkeys();
        if !enabled {
            match slot {
                ShortcutSlot::Jump => {
                    if let Some(m) = cur.switch_mods {
                        self.last_switch_mods = Some(m);
                    }
                    self.config.server.set_switch_shortcut(None);
                }
                ShortcutSlot::Lock => {
                    if let Some(c) = cur.lock {
                        self.last_lock_chord = Some(c);
                    }
                    self.config.server.set_lock_shortcut(None);
                }
                ShortcutSlot::Home => {
                    if let Some(c) = cur.escape {
                        self.last_escape_chord = Some(c);
                    }
                    self.config.server.set_escape_shortcut(None);
                }
            }
            if self.shortcut_recorder.active == Some(slot) {
                self.stop_recording_shortcut();
            }
            self.push_config();
            return;
        }

        self.stop_recording_shortcut();
        match slot {
            ShortcutSlot::Jump => {
                let candidate = self.last_switch_mods.unwrap_or(HotkeyMods::DEFAULT_SWITCH);
                if let Some(other) = jump_conflict(cur, candidate) {
                    self.start_recording_shortcut(slot);
                    tx.focus(slot.recorder_key(), false);
                    self.toast(
                        format!(
                            "Already used by {} — press a new shortcut",
                            other.title()
                        ),
                        tx,
                    );
                    return;
                }
                self.config.server.set_switch_shortcut(Some(candidate));
                self.last_switch_mods = Some(candidate);
            }
            ShortcutSlot::Lock => {
                let candidate = self.last_lock_chord.unwrap_or(HotkeyChord::DEFAULT_LOCK);
                if let Some(other) = chord_conflict(slot, cur, candidate) {
                    self.start_recording_shortcut(slot);
                    tx.focus(slot.recorder_key(), false);
                    self.toast(
                        format!(
                            "Already used by {} — press a new shortcut",
                            other.title()
                        ),
                        tx,
                    );
                    return;
                }
                self.config.server.set_lock_shortcut(Some(candidate));
                self.last_lock_chord = Some(candidate);
            }
            ShortcutSlot::Home => {
                let candidate = self.last_escape_chord.unwrap_or(HotkeyChord::DEFAULT_ESCAPE);
                if let Some(other) = chord_conflict(slot, cur, candidate) {
                    self.start_recording_shortcut(slot);
                    tx.focus(slot.recorder_key(), false);
                    self.toast(
                        format!(
                            "Already used by {} — press a new shortcut",
                            other.title()
                        ),
                        tx,
                    );
                    return;
                }
                self.config.server.set_escape_shortcut(Some(candidate));
                self.last_escape_chord = Some(candidate);
            }
        }
        self.push_config();
    }

    pub fn toggle_shortcut_slot(&mut self, slot: ShortcutSlot, tx: &mut Tx) {
        let cur = self.config.server.parsed_hotkeys();
        let enabled = match slot {
            ShortcutSlot::Jump => cur.switch_mods.is_some(),
            ShortcutSlot::Lock => cur.lock.is_some(),
            ShortcutSlot::Home => cur.escape.is_some(),
        };
        self.set_shortcut_enabled(slot, !enabled, tx);
    }

    pub fn reset_shortcuts(&mut self) {
        self.stop_recording_shortcut();
        self.shortcut_recorder.swallow_key = None;
        self.config.server.reset_shortcuts();
        self.last_switch_mods = None;
        self.last_lock_chord = None;
        self.last_escape_chord = None;
        self.push_config();
    }

    pub fn handle_shortcut_physical_input(
        &mut self,
        slot: ShortcutSlot,
        ev: &PhysicalInput,
        tx: &mut Tx,
    ) -> bool {
        match *ev {
            PhysicalInput::FocusLost => {
                self.shortcut_recorder.swallow_key = None;
                if self.shortcut_recorder.active == Some(slot) {
                    self.stop_recording_shortcut();
                    return true;
                }
                false
            }
            PhysicalInput::Modifiers(mods) => {
                if self.shortcut_recorder.active != Some(slot) {
                    return false;
                }
                self.sync_phys_mods_with_event(mods);
                self.shortcut_recorder.held_mods =
                    hotkey_mods_from_rune(mods, self.shortcut_recorder.altgr_held);
                true
            }
            PhysicalInput::Key {
                key,
                mods,
                pressed,
                repeat,
            } => {
                if self.shortcut_recorder.swallow_key == Some(key) {
                    if !pressed {
                        self.shortcut_recorder.swallow_key = None;
                    }
                    return true;
                }

                if self.shortcut_recorder.active != Some(slot) {
                    if pressed {
                        self.shortcut_recorder.swallow_key = None;
                    }
                    return false;
                }

                // Right Alt (AltGr) is reserved for international typing.
                if key == PhysicalKeyCode::AltRight {
                    self.shortcut_recorder.altgr_held = pressed;
                    self.shortcut_recorder
                        .held_phys
                        .update(HidUsage::ALT_RIGHT, pressed);
                    if pressed {
                        // On Windows, AltGr synthesizes fake Ctrl+Alt — strip them from held mods.
                        self.shortcut_recorder.held_phys.0 &= !(ModifierMask::CONTROL_LEFT
                            | ModifierMask::CONTROL_RIGHT
                            | ModifierMask::ALT_LEFT);
                        self.shortcut_recorder.held_mods = hotkey_mods_from_rune(mods, true);
                        if !repeat {
                            self.toast("Use Left Alt — AltGr is reserved for typing", tx);
                        }
                    } else {
                        self.sync_phys_mods_with_event(mods);
                        self.shortcut_recorder.held_mods = hotkey_mods_from_rune(mods, false);
                    }
                    return true;
                }

                let usage = physical_key_to_hid(key);

                // Modifier press/release updates live preview and waits for a trigger key.
                if let Some(mod_usage) = usage.filter(|u| u.is_modifier()) {
                    self.shortcut_recorder.held_phys.update(mod_usage, pressed);
                    if self.shortcut_recorder.altgr_held {
                        self.shortcut_recorder.held_mods = hotkey_mods_from_rune(mods, true);
                    } else if pressed {
                        let from_phys =
                            HotkeyMods::from_modifier_mask(self.shortcut_recorder.held_phys)
                                .unwrap_or(HotkeyMods::EMPTY);
                        self.shortcut_recorder.held_mods =
                            hotkey_mods_from_rune(mods, false) | from_phys;
                    } else {
                        let mut next = hotkey_mods_from_rune(mods, false);
                        if matches!(
                            key,
                            PhysicalKeyCode::ControlLeft | PhysicalKeyCode::ControlRight
                        ) && !self.shortcut_recorder.held_phys.ctrl()
                        {
                            next = HotkeyMods(next.bits() & !HotkeyMods::CTRL.bits());
                        }
                        if key == PhysicalKeyCode::AltLeft
                            && !self
                                .shortcut_recorder
                                .held_phys
                                .contains(ModifierMask::ALT_LEFT)
                        {
                            next = HotkeyMods(next.bits() & !HotkeyMods::ALT.bits());
                        }
                        if matches!(
                            key,
                            PhysicalKeyCode::ShiftLeft | PhysicalKeyCode::ShiftRight
                        ) && !self.shortcut_recorder.held_phys.shift()
                        {
                            next = HotkeyMods(next.bits() & !HotkeyMods::SHIFT.bits());
                        }
                        if matches!(
                            key,
                            PhysicalKeyCode::SuperLeft | PhysicalKeyCode::SuperRight
                        ) && !self.shortcut_recorder.held_phys.meta()
                        {
                            next = HotkeyMods(next.bits() & !HotkeyMods::SUPER.bits());
                        }
                        self.shortcut_recorder.held_mods = next;
                    }
                    return true;
                }

                if !pressed || repeat {
                    return true;
                }

                let event_mods_empty =
                    !mods.ctrl && !mods.alt && !mods.shift && !mods.logo && !mods.cmd;

                // Unmodified Escape always cancels recording cleanly.
                if key == PhysicalKeyCode::Escape
                    && event_mods_empty
                    && (self.shortcut_recorder.altgr_held
                        || self.shortcut_recorder.held_phys == ModifierMask::EMPTY)
                {
                    self.stop_recording_shortcut();
                    self.shortcut_recorder.swallow_key = Some(key);
                    return true;
                }

                if self.shortcut_recorder.altgr_held {
                    self.toast("Use Left Alt — AltGr is reserved for typing", tx);
                    return true;
                }

                let active_mods = hotkey_mods_from_rune(mods, false)
                    | HotkeyMods::from_modifier_mask(self.shortcut_recorder.held_phys)
                        .unwrap_or(HotkeyMods::EMPTY);

                // Unmodified Backspace or Delete disables this shortcut slot.
                if matches!(key, PhysicalKeyCode::Backspace | PhysicalKeyCode::Delete)
                    && active_mods.is_empty()
                {
                    self.set_shortcut_enabled(slot, false, tx);
                    self.shortcut_recorder.swallow_key = Some(key);
                    self.toast("Shortcut disabled", tx);
                    return true;
                }

                let Some(usage) = usage else {
                    self.toast("Unsupported key for shortcut", tx);
                    return true;
                };

                let current = self.config.server.parsed_hotkeys();
                match slot {
                    ShortcutSlot::Jump => {
                        if arrow_to_edge(usage).is_none()
                            || active_mods.validate_for_switch().is_err()
                        {
                            self.toast("Press Ctrl, Alt, or Super + any Arrow key", tx);
                            return true;
                        }
                        if let Some(other) = jump_conflict(current, active_mods) {
                            self.toast(format!("Already used by {}", other.title()), tx);
                            return true;
                        }
                        self.config.server.set_switch_shortcut(Some(active_mods));
                        self.last_switch_mods = Some(active_mods);
                    }
                    ShortcutSlot::Lock | ShortcutSlot::Home => {
                        let chord = HotkeyChord::new(active_mods, usage);
                        if let Err(reason) = chord.validate_for_lock_or_escape() {
                            self.toast(reason, tx);
                            return true;
                        }
                        if let Some(other) = chord_conflict(slot, current, chord) {
                            self.toast(format!("Already used by {}", other.title()), tx);
                            return true;
                        }
                        if slot == ShortcutSlot::Lock {
                            self.config.server.set_lock_shortcut(Some(chord));
                            self.last_lock_chord = Some(chord);
                        } else {
                            self.config.server.set_escape_shortcut(Some(chord));
                            self.last_escape_chord = Some(chord);
                        }
                    }
                }

                self.push_config();
                self.stop_recording_shortcut();
                self.shortcut_recorder.swallow_key = Some(key);
                true
            }
        }
    }

    pub fn quit_app(&mut self, tx: &mut Tx) {
        self.stop_recording_shortcut();
        self.shortcut_recorder.swallow_key = None;
        updater::zeroize_string(&mut self.sudo_password.text);
        self.sudo_password.clear();
        if let UpdateCheckState::AwaitingSudoPassword { stage_dir, .. } = &self.update_state {
            updater::cleanup_staged_dir(stage_dir);
        }
        let _ = self.ctrl.quit_tx.send(());
        tx.quit();
    }
}

fn hotkey_mods_from_rune(mods: Mods, altgr_held: bool) -> HotkeyMods {
    let mut out = HotkeyMods::EMPTY;
    if !altgr_held {
        if mods.ctrl || (!cfg!(target_os = "macos") && mods.cmd) {
            out |= HotkeyMods::CTRL;
        }
        if mods.alt {
            out |= HotkeyMods::ALT;
        }
    }
    if mods.shift {
        out |= HotkeyMods::SHIFT;
    }
    if mods.logo || (cfg!(target_os = "macos") && mods.cmd) {
        out |= HotkeyMods::SUPER;
    }
    out
}

fn jump_conflict(current: CaptureHotkeys, candidate_mods: HotkeyMods) -> Option<ShortcutSlot> {
    if current
        .lock
        .is_some_and(|c| c.mods == candidate_mods && arrow_to_edge(c.key).is_some())
    {
        return Some(ShortcutSlot::Lock);
    }
    if current
        .escape
        .is_some_and(|c| c.mods == candidate_mods && arrow_to_edge(c.key).is_some())
    {
        return Some(ShortcutSlot::Home);
    }
    None
}

fn chord_conflict(
    slot: ShortcutSlot,
    current: CaptureHotkeys,
    candidate: HotkeyChord,
) -> Option<ShortcutSlot> {
    if slot == ShortcutSlot::Lock && current.escape == Some(candidate) {
        return Some(ShortcutSlot::Home);
    }
    if slot == ShortcutSlot::Home && current.lock == Some(candidate) {
        return Some(ShortcutSlot::Lock);
    }
    if current.switch_mods == Some(candidate.mods) && arrow_to_edge(candidate.key).is_some() {
        return Some(ShortcutSlot::Jump);
    }
    None
}

pub fn physical_key_to_hid(key: PhysicalKeyCode) -> Option<HidUsage> {
    use PhysicalKeyCode as K;
    let usage = match key {
        K::KeyA => HidUsage::keyboard(0x04),
        K::KeyB => HidUsage::keyboard(0x05),
        K::KeyC => HidUsage::keyboard(0x06),
        K::KeyD => HidUsage::keyboard(0x07),
        K::KeyE => HidUsage::keyboard(0x08),
        K::KeyF => HidUsage::keyboard(0x09),
        K::KeyG => HidUsage::keyboard(0x0A),
        K::KeyH => HidUsage::keyboard(0x0B),
        K::KeyI => HidUsage::keyboard(0x0C),
        K::KeyJ => HidUsage::keyboard(0x0D),
        K::KeyK => HidUsage::keyboard(0x0E),
        K::KeyL => HidUsage::KEY_L,
        K::KeyM => HidUsage::keyboard(0x10),
        K::KeyN => HidUsage::keyboard(0x11),
        K::KeyO => HidUsage::keyboard(0x12),
        K::KeyP => HidUsage::keyboard(0x13),
        K::KeyQ => HidUsage::keyboard(0x14),
        K::KeyR => HidUsage::keyboard(0x15),
        K::KeyS => HidUsage::keyboard(0x16),
        K::KeyT => HidUsage::keyboard(0x17),
        K::KeyU => HidUsage::keyboard(0x18),
        K::KeyV => HidUsage::keyboard(0x19),
        K::KeyW => HidUsage::keyboard(0x1A),
        K::KeyX => HidUsage::keyboard(0x1B),
        K::KeyY => HidUsage::keyboard(0x1C),
        K::KeyZ => HidUsage::keyboard(0x1D),
        K::Digit1 => HidUsage::keyboard(0x1E),
        K::Digit2 => HidUsage::keyboard(0x1F),
        K::Digit3 => HidUsage::keyboard(0x20),
        K::Digit4 => HidUsage::keyboard(0x21),
        K::Digit5 => HidUsage::keyboard(0x22),
        K::Digit6 => HidUsage::keyboard(0x23),
        K::Digit7 => HidUsage::keyboard(0x24),
        K::Digit8 => HidUsage::keyboard(0x25),
        K::Digit9 => HidUsage::keyboard(0x26),
        K::Digit0 => HidUsage::keyboard(0x27),
        K::Enter => HidUsage::keyboard(0x28),
        K::Escape => HidUsage::ESCAPE,
        K::Backspace => HidUsage::BACKSPACE,
        K::Tab => HidUsage::keyboard(0x2B),
        K::Space => HidUsage::keyboard(0x2C),
        K::Minus => HidUsage::keyboard(0x2D),
        K::Equal => HidUsage::keyboard(0x2E),
        K::BracketLeft => HidUsage::keyboard(0x2F),
        K::BracketRight => HidUsage::keyboard(0x30),
        K::Backslash => HidUsage::keyboard(0x31),
        K::Semicolon => HidUsage::keyboard(0x33),
        K::Quote => HidUsage::keyboard(0x34),
        K::Backquote => HidUsage::keyboard(0x35),
        K::Comma => HidUsage::keyboard(0x36),
        K::Period => HidUsage::keyboard(0x37),
        K::Slash => HidUsage::keyboard(0x38),
        K::F1 => HidUsage::keyboard(0x3A),
        K::F2 => HidUsage::keyboard(0x3B),
        K::F3 => HidUsage::keyboard(0x3C),
        K::F4 => HidUsage::keyboard(0x3D),
        K::F5 => HidUsage::keyboard(0x3E),
        K::F6 => HidUsage::keyboard(0x3F),
        K::F7 => HidUsage::keyboard(0x40),
        K::F8 => HidUsage::keyboard(0x41),
        K::F9 => HidUsage::keyboard(0x42),
        K::F10 => HidUsage::keyboard(0x43),
        K::F11 => HidUsage::keyboard(0x44),
        K::F12 => HidUsage::keyboard(0x45),
        K::PrintScreen => HidUsage::PRINT_SCREEN,
        K::ScrollLock => HidUsage::SCROLL_LOCK,
        K::Pause => HidUsage::PAUSE,
        K::Insert => HidUsage::INSERT,
        K::Home => HidUsage::HOME,
        K::PageUp => HidUsage::PAGE_UP,
        K::Delete => HidUsage::DELETE,
        K::End => HidUsage::END,
        K::PageDown => HidUsage::PAGE_DOWN,
        K::ArrowRight => HidUsage::ARROW_RIGHT,
        K::ArrowLeft => HidUsage::ARROW_LEFT,
        K::ArrowDown => HidUsage::ARROW_DOWN,
        K::ArrowUp => HidUsage::ARROW_UP,
        K::NumpadDivide => HidUsage::keyboard(0x54),
        K::NumpadMultiply => HidUsage::keyboard(0x55),
        K::NumpadSubtract => HidUsage::keyboard(0x56),
        K::NumpadAdd => HidUsage::keyboard(0x57),
        K::NumpadEnter => HidUsage::keyboard(0x58),
        K::Numpad1 => HidUsage::keyboard(0x59),
        K::Numpad2 => HidUsage::keyboard(0x5A),
        K::Numpad3 => HidUsage::keyboard(0x5B),
        K::Numpad4 => HidUsage::keyboard(0x5C),
        K::Numpad5 => HidUsage::keyboard(0x5D),
        K::Numpad6 => HidUsage::keyboard(0x5E),
        K::Numpad7 => HidUsage::keyboard(0x5F),
        K::Numpad8 => HidUsage::keyboard(0x60),
        K::Numpad9 => HidUsage::keyboard(0x61),
        K::Numpad0 => HidUsage::keyboard(0x62),
        K::NumpadDecimal => HidUsage::keyboard(0x63),
        K::IntlBackslash => HidUsage::ISO_BACKSLASH,
        K::ContextMenu => HidUsage::CONTEXT_MENU,
        K::NumpadEqual => HidUsage::keyboard(0x67),
        K::F13 => HidUsage::keyboard(0x68),
        K::F14 => HidUsage::keyboard(0x69),
        K::F15 => HidUsage::keyboard(0x6A),
        K::F16 => HidUsage::keyboard(0x6B),
        K::F17 => HidUsage::keyboard(0x6C),
        K::F18 => HidUsage::keyboard(0x6D),
        K::F19 => HidUsage::keyboard(0x6E),
        K::F20 => HidUsage::keyboard(0x6F),
        K::F21 => HidUsage::keyboard(0x70),
        K::F22 => HidUsage::keyboard(0x71),
        K::F23 => HidUsage::keyboard(0x72),
        K::F24 => HidUsage::keyboard(0x73),
        K::ControlLeft => HidUsage::CONTROL_LEFT,
        K::ShiftLeft => HidUsage::SHIFT_LEFT,
        K::AltLeft => HidUsage::ALT_LEFT,
        K::SuperLeft => HidUsage::META_LEFT,
        K::ControlRight => HidUsage::CONTROL_RIGHT,
        K::ShiftRight => HidUsage::SHIFT_RIGHT,
        K::AltRight => HidUsage::ALT_RIGHT,
        K::SuperRight => HidUsage::META_RIGHT,
        _ => return None,
    };
    Some(usage)
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

    // Commit edited Port, Server, or Advanced text fields once they lose focus.
    if s.commit_text_fields() {
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
    s.stop_recording_shortcut();
    s.shortcut_recorder.swallow_key = None;
    s.show_nightly_warning = false;
    s.window_vis.set_visible(false);
    // If no system tray is available, closing the window quits Glide cleanly instead of
    // leaving an invisible background process. Read the atomic directly so an async tray
    // registration that just finished is always observed.
    if !s.ctrl.tray_active.load(Ordering::SeqCst) {
        s.quit_app(tx);
    }
}

fn on_reopen(s: &mut Ui, has_visible: bool, _tx: &mut Tx) -> bool {
    if !has_visible && s.window_vis.set_visible(true) {
        let (rev, snap) = s.ctrl.runtime.snapshot_with_rev();
        s.window_vis.record_rev(rev);
        s.apply_status_if_newer(rev, snap);
    }
    false
}

/// Runs the native Rune window on the calling (main) thread.
pub(crate) fn run(
    ctrl: Arc<AppController>,
    rt: tokio::runtime::Handle,
    minimized: bool,
    launch_t0: Instant,
    pending_auto_start: Arc<parking_lot::Mutex<Option<String>>>,
) -> Result<(), String> {
    // Rune defaults to fast_exit (TerminateProcess / _exit inside the event loop), which would
    // skip engine/IPC/tray teardown and post-update relaunch in run_gui_blocking.
    rune::orderly_exit();

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
        .on_close(on_close)
        .on_reopen(on_reopen);

    if minimized {
        app = app.windowless();
    }

    if std::env::var_os("GLIDE_BENCH_STARTUP").is_some() {
        let mut logged = false;
        app = app.on_frame_encoded(move |_| {
            if !logged {
                logged = true;
                let total_ms = launch_t0.elapsed().as_secs_f64() * 1000.0;
                eprintln!("glide-bench: first_frame_ms={total_ms:.1}");
                let marks = rune::marks::marks();
                if let Some((_, first)) = marks.first().copied() {
                    for (name, at) in &marks {
                        let dt = at.saturating_duration_since(first).as_secs_f64() * 1000.0;
                        eprintln!("glide-bench: mark {name} +{dt:.1}ms");
                    }
                }
                for (k, v) in rune::marks::notes() {
                    eprintln!("glide-bench: note {k}={v}");
                }
            }
        });
    }

    let handle = app.handle();
    let window_vis = Arc::new(WindowVisibility::new(!minimized));

    {
        let on_win_vis = Arc::clone(&window_vis);
        let on_win_ctrl = Arc::clone(&ctrl);
        let on_win_handle = handle.clone();
        app = app.on_window(move |_| {
            if on_win_vis.set_visible(true) {
                on_win_vis.push_fresh_snapshot(&on_win_ctrl, &on_win_handle);
            }
            #[cfg(target_os = "linux")]
            if !glide_backend::linux_wayland::is_wayland_session() {
                glide_backend::linux_x11::window_brand::spawn_x11_window_brander(
                    GUI_WIDTH, GUI_HEIGHT,
                );
            }
        });
    }

    // Wire AppController hooks so the tray, `/api/show`, and Ctrl+C control this window.
    {
        let show_handle = handle.clone();
        let show_ctrl = Arc::clone(&ctrl);
        let show_vis = Arc::clone(&window_vis);
        let fallback_url = ctrl.url.clone();
        *ctrl.ui_show.lock() = Some(Arc::new(move || {
            // Queue the latest snapshot BEFORE `open()` so Rune drains it on the same
            // turn before painting the revealed window (zero stale frame on reopen).
            if show_vis.set_visible(true) {
                show_vis.push_fresh_snapshot(&show_ctrl, &show_handle);
            }
            show_handle.open();
            let h = show_handle.clone();
            let url = fallback_url.clone();
            let vis_check = Arc::clone(&show_vis);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1500));
                if let Some(err) = h.window_error() {
                    vis_check.set_visible(false);
                    warn!(error = %err, "Native window could not open; launching browser fallback");
                    let _ = launch_browser_window(&url);
                }
            });
        }));

        let quit_handle = handle.clone();
        *ctrl.ui_quit.lock() = Some(Arc::new(move || {
            quit_handle.quit();
        }));

        let tray_handle = handle.clone();
        *ctrl.ui_tray.lock() = Some(Arc::new(move |active| {
            tray_handle.update_if(move |ui, _| {
                if ui.tray_active == active {
                    return false;
                }
                ui.tray_active = active;
                true
            });
        }));
    }

    let init_ctrl = Arc::clone(&ctrl);
    let init_rt = rt.clone();
    let init_handle = handle.clone();
    let init_vis = Arc::clone(&window_vis);
    app = app.init(move || {
        // Trigger auto-start here (after winit::EventLoop::build has completed its one-time
        // Raw Input registration on Windows) so Glide's capture sink is never overwritten.
        if let Some(role) = pending_auto_start.lock().take() {
            let ctrl_auto = Arc::clone(&init_ctrl);
            init_rt.spawn(async move {
                if let Err(e) = ctrl_auto.start_engine(&role).await {
                    warn!(error = %e, "Failed auto-starting Glide engine");
                }
            });
        }
        let mut ui = Ui::new(init_ctrl, init_rt, init_vis);
        ui.handle = Some(init_handle);
        ui
    });

    if !headless_snap {
        spawn_background_init(Arc::clone(&ctrl), &rt, handle.clone());
        spawn_runtime_bridge(
            Arc::clone(&ctrl),
            &rt,
            handle.clone(),
            Arc::clone(&window_vis),
        );
        spawn_discovery_loop(Arc::clone(&ctrl), &rt, handle, window_vis);
    }

    app.try_run()
}

fn dummy_init() -> Ui {
    unreachable!("App::init closure always constructs Ui")
}

/// Runs non-critical startup work (TLS identity load/generation and display diagnostics)
/// off the UI thread so the native window opens immediately.
fn spawn_background_init(
    ctrl: Arc<AppController>,
    rt: &tokio::runtime::Handle,
    handle: AppHandle<Ui>,
) {
    let os_kind = glide_backend::current_os_kind();
    let os_label_str = os_label(os_kind);
    rt.spawn(async move {
        // Sync tray state once in case the background tray task finished before ui_tray was wired.
        let tray_now = ctrl.tray_active.load(Ordering::SeqCst);
        handle.update_if(move |ui, _| {
            if ui.tray_active == tray_now {
                return false;
            }
            ui.tray_active = tray_now;
            true
        });

        let ctrl_id = Arc::clone(&ctrl);
        let fp = tokio::task::spawn_blocking(move || ctrl_id.load_identity())
            .await
            .ok()
            .flatten();
        if let Some(fp) = fp {
            handle.update(move |ui, _| ui.fingerprint = Some(fp));
        }

        let ctrl_doc = Arc::clone(&ctrl);
        let doc = tokio::task::spawn_blocking(move || {
            let res = glide_backend::run_doctor().ok();
            if let Some(ref d) = res {
                ctrl_doc.runtime.update(|s| {
                    s.geometry = d.geometry.clone();
                });
            }
            res
        })
        .await
        .ok()
        .flatten();

        let (doctor_display, doctor_text) = match doc {
            Some(d) => {
                let mut lines = vec![
                    format!("OS       {os_label_str} ({os_kind:?})"),
                    format!("Display  {}", d.display_info),
                    String::new(),
                ];
                for note in &d.notes {
                    lines.push(format!("• {note}"));
                }
                (d.display_info, lines.join("\n"))
            }
            None => (
                "Display detected".into(),
                format!("OS       {os_label_str} ({os_kind:?})\n"),
            ),
        };
        handle.update(move |ui, _| {
            ui.doctor_display = doctor_display;
            ui.doctor_text = doctor_text;
        });

        // Automatically check for new dev builds on launch ONLY when opted into the Nightly track.
        // Stable stays 100% offline unless the user clicks Check.
        if ctrl.config.lock().update_channel == UpdateChannel::Nightly {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let current = env!("CARGO_PKG_VERSION");
            if let Ok(state) = tokio::task::spawn_blocking(move || {
                updater::check_release(UpdateChannel::Nightly, current)
            })
            .await
            {
                handle.update_if(move |ui, tx| {
                    if ui.config.update_channel != UpdateChannel::Nightly
                        || !matches!(ui.update_state, UpdateCheckState::Idle)
                    {
                        return false;
                    }
                    match state {
                        UpdateCheckState::Available(avail) => {
                            let label = avail.version.clone();
                            ui.update_state = UpdateCheckState::Available(avail);
                            ui.toast(format!("Glide {label} is available"), tx);
                            true
                        }
                        UpdateCheckState::UpToDate { latest } => {
                            ui.update_state = UpdateCheckState::UpToDate { latest };
                            true
                        }
                        _ => false,
                    }
                });
            }
        }
    });
}

fn spawn_runtime_bridge(
    ctrl: Arc<AppController>,
    rt: &tokio::runtime::Handle,
    handle: AppHandle<Ui>,
    vis: Arc<WindowVisibility>,
) {
    let mut rx = ctrl.runtime.subscribe();
    let mut vis_rx = vis.subscribe();
    rt.spawn(async move {
        loop {
            if !vis.is_visible() {
                // Window is hidden in the tray: park with zero wakeups and zero snapshot
                // clones until it reopens, then coalesce all missed updates into one push.
                if vis_rx.wait_for(|&v| v).await.is_err() {
                    break;
                }
                while matches!(
                    rx.try_recv(),
                    Ok(()) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
                ) {}
                if vis.is_visible() {
                    vis.push_fresh_snapshot(&ctrl, &handle);
                }
                continue;
            }

            tokio::select! {
                res = rx.recv() => {
                    match res {
                        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if !vis.is_visible() {
                                continue;
                            }
                            while matches!(
                                rx.try_recv(),
                                Ok(()) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
                            ) {}
                            vis.push_fresh_snapshot(&ctrl, &handle);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                res = vis_rx.wait_for(|&v| !v) => {
                    if res.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn spawn_discovery_loop(
    ctrl: Arc<AppController>,
    rt: &tokio::runtime::Handle,
    handle: AppHandle<Ui>,
    vis: Arc<WindowVisibility>,
) {
    let mut vis_rx = vis.subscribe();
    rt.spawn(async move {
        let mut force_refresh = true;
        loop {
            if !vis.is_visible() {
                // Window is hidden in the tray: perform 0 LAN scans and 0 UI wakeups until reopened.
                if vis_rx.wait_for(|&v| v).await.is_err() {
                    break;
                }
                force_refresh = true;
            }

            let (running, mode_is_client, connected) = ctrl.runtime.read(|snap| {
                let running = snap.mode == "server" || snap.mode == "client";
                let mode_is_client = snap.mode == "client";
                let connected = snap.status.starts_with("Connected");
                (running, mode_is_client, connected)
            });

            if !running || (mode_is_client && !connected) {
                force_refresh = false;
                tokio::select! {
                    (servers, ips) = run_discovery(&ctrl) => {
                        if vis.is_visible() {
                            handle.update_if(move |ui, _| {
                                if ui.local_ips == ips && ui.discovered == servers {
                                    return false;
                                }
                                ui.local_ips = ips;
                                ui.discovered = servers;
                                true
                            });
                        }
                    }
                    res = vis_rx.wait_for(|&v| !v) => {
                        if res.is_err() {
                            break;
                        }
                        continue;
                    }
                }
            } else if force_refresh {
                // Running server or connected client: refresh local IPs on initial open /
                // reopen without issuing a 2s UDP LAN broadcast scan.
                force_refresh = false;
                let ips = local_ipv4_addresses_async().await;
                if vis.is_visible() {
                    handle.update_if(move |ui, _| {
                        if ui.local_ips == ips {
                            return false;
                        }
                        ui.local_ips = ips;
                        true
                    });
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(4500)) => {}
                res = vis_rx.wait_for(|&v| !v) => {
                    if res.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

async fn run_discovery(ctrl: &AppController) -> (Vec<DiscoveredPeer>, Vec<String>) {
    let port = ctrl.config.lock().client.discovery_port;
    let found = discover_servers(port, Duration::from_millis(2000), None, false)
        .await
        .unwrap_or_default();
    // Read our fingerprint after discovery completes so async startup identity load has populated it.
    let my_fp = ctrl.fingerprint.lock().clone();
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
    (list, local_ipv4_addresses_async().await)
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
    s.hostname = "gaming-workstation".into();
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
    s.hostname = "ubuntu-dev".into();
    s.os_kind = OsKind::LinuxX11;
    s.os_label = "Linux";
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
            filename: "glide_0.5.0_amd64.deb".into(),
            bytes: 10_620_844,
            direction: "sent".into(),
            saved_path: None,
        },
        FileTransferEntry {
            filename: "notes.md".into(),
            bytes: 4_820,
            direction: "received".into(),
            saved_path: Some("~/Downloads/Glide/notes.md".into()),
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
    ("client-profiles", |s| {
        s.theme = ThemePref::Light;
        scene_client_connected(s);
        s.status.local_addr = Some("192.168.1.42:24800".into());
        s.known_servers = vec![
            KnownServer {
                name: "workstation".into(),
                address: "192.168.1.42:24800".into(),
                fingerprint: Some("ab:cd:ef".into()),
                edge: Edge::Right,
                last_connected_unix: 2000,
            },
            KnownServer {
                name: "studio-pc".into(),
                address: "192.168.1.88:24800".into(),
                fingerprint: Some("12:34:56".into()),
                edge: Edge::Left,
                last_connected_unix: 1000,
            },
        ];
        s.saved_servers_expanded = true;
    }),
    ("client-profiles-collapsed", |s| {
        s.theme = ThemePref::Light;
        scene_client_connected(s);
        s.status.local_addr = Some("192.168.1.42:24800".into());
        s.known_servers = vec![
            KnownServer {
                name: "workstation".into(),
                address: "192.168.1.42:24800".into(),
                fingerprint: Some("ab:cd:ef".into()),
                edge: Edge::Right,
                last_connected_unix: 2000,
            },
            KnownServer {
                name: "studio-pc".into(),
                address: "192.168.1.88:24800".into(),
                fingerprint: Some("12:34:56".into()),
                edge: Edge::Left,
                last_connected_unix: 1000,
            },
        ];
        s.saved_servers_expanded = false;
    }),
    ("input-server-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Input;
        s.role = Role::Server;
        s.is_windows = true;
        s.os_kind = OsKind::Windows;
        s.os_label = "Windows";
    }),
    ("input-server-dark", |s| {
        s.theme = ThemePref::Dark;
        s.tab = Tab::Input;
        s.role = Role::Server;
        s.is_windows = true;
        s.os_kind = OsKind::Windows;
        s.os_label = "Windows";
    }),
    ("input-client-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Input;
        s.role = Role::Client;
        s.hostname = "ubuntu-dev".into();
        s.is_windows = false;
        s.os_kind = OsKind::LinuxX11;
        s.os_label = "Linux";
        s.config.client.key_remap.push(KeyRemapRule {
            from: "CapsLock".into(),
            to: "Escape".into(),
        });
    }),
    ("input-client-dark", |s| {
        s.theme = ThemePref::Dark;
        s.tab = Tab::Input;
        s.role = Role::Client;
        s.hostname = "ubuntu-dev".into();
        s.is_windows = false;
        s.os_kind = OsKind::LinuxX11;
        s.os_label = "Linux";
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
    ("clipboard-filled-dark", |s| {
        s.theme = ThemePref::Dark;
        scene_clipboard_filled(s);
    }),
    ("clipboard-hidden", |s| {
        s.theme = ThemePref::Dark;
        scene_clipboard_filled(s);
        s.config.hide_clipboard_history = true;
    }),
    ("clipboard-off", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Clipboard;
        s.config.clipboard_history = false;
    }),
    ("settings-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Settings;
        s.hostname = "ubuntu-dev".into();
        s.is_windows = false;
        s.os_kind = OsKind::LinuxX11;
        s.os_label = "Linux";
        s.config.update_channel = UpdateChannel::Stable;
        s.show_advanced = false;
    }),
    ("settings-dark", |s| {
        s.theme = ThemePref::Dark;
        s.tab = Tab::Settings;
        s.hostname = "ubuntu-dev".into();
        s.is_windows = false;
        s.os_kind = OsKind::LinuxX11;
        s.os_label = "Linux";
        s.config.update_channel = UpdateChannel::Stable;
        s.show_advanced = false;
    }),
    ("settings-windows-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Settings;
        s.is_windows = true;
        s.os_kind = OsKind::Windows;
        s.os_label = "Windows";
        s.hostname = "gaming-workstation".into();
        s.show_advanced = false;
    }),
    ("settings-advanced-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Settings;
        s.is_windows = false;
        s.os_label = "Linux";
        s.show_advanced = true;
        s.fingerprint = Some("ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89".into());
        s.doctor_display = "DISPLAY=:0 (screen 0)".into();
        s.doctor_text = "OS       Linux (LinuxX11)\nDisplay  DISPLAY=:0 (screen 0)\n\n• Native X11 session detected.\n• XTest extension: OK (v2.2)\n• XInput2 extension: OK (v2.2)\n• XFixes extension: OK (v5.0)".into();
    }),
    ("settings-update-ready", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Settings;
        s.is_windows = true;
        s.os_kind = OsKind::Windows;
        s.os_label = "Windows";
        s.hostname = "gaming-workstation".into();
        s.config.update_channel = UpdateChannel::Nightly;
        s.update_state = UpdateCheckState::Available(Box::new(updater::AvailableUpdate {
            channel: UpdateChannel::Nightly,
            version: "nightly (7b68d47)".into(),
            short_label: "nightly 7b68d47".into(),
            html_url: updater::RELEASES_PAGE_URL.into(),
            is_channel_switch: false,
            binary_asset: Some(updater::UpdateAsset {
                name: "glide-windows-x86_64-standalone.exe".into(),
                download_url: format!(
                    "{}nightly/glide-windows-x86_64-standalone.exe",
                    updater::ASSET_URL_PREFIX
                ),
                size: 21_788_672,
            }),
            msi_asset: Some(updater::UpdateAsset {
                name: "glide-windows-x86_64-installer.msi".into(),
                download_url: format!(
                    "{}nightly/glide-windows-x86_64-installer.msi",
                    updater::ASSET_URL_PREFIX
                ),
                size: 8_658_944,
            }),
            deb_asset: Some(updater::UpdateAsset {
                name: "glide_0.5.0_amd64.deb".into(),
                download_url: format!("{}nightly/glide_0.5.0_amd64.deb", updater::ASSET_URL_PREFIX),
                size: 10_620_844,
            }),
            checksums_url: Some(format!("{}nightly/SHA256SUMS.txt", updater::ASSET_URL_PREFIX)),
        }));
    }),
    ("settings-nightly-warn-light", |s| {
        s.theme = ThemePref::Light;
        s.tab = Tab::Settings;
        s.is_windows = false;
        s.os_label = "Linux";
        s.show_advanced = false;
        s.config.update_channel = UpdateChannel::Stable;
        s.show_nightly_warning = true;
    }),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn window_visibility_parks_when_hidden_and_wakes_on_reopen() {
        let vis = Arc::new(WindowVisibility::new(false));
        assert!(!vis.is_visible());

        // Setting false->false is not a reopen transition.
        assert!(!vis.set_visible(false));

        let mut rx = vis.subscribe();
        let waiter = tokio::spawn(async move {
            rx.wait_for(|&v| v).await.is_ok()
        });

        // Give the task a chance to park while hidden.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        // Transition false->true reports reopen and wakes the parked waiter immediately.
        assert!(vis.set_visible(true));
        assert!(vis.is_visible());
        assert!(!vis.set_visible(true));
        assert!(tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should wake immediately on reopen")
            .unwrap());

        // Closing transitions back to hidden and wakes !v waiters.
        let mut hide_rx = vis.subscribe();
        let hide_waiter = tokio::spawn(async move {
            hide_rx.wait_for(|&v| !v).await.is_ok()
        });
        tokio::task::yield_now().await;
        assert!(!hide_waiter.is_finished());
        assert!(!vis.set_visible(false));
        assert!(!vis.is_visible());
        assert!(tokio::time::timeout(Duration::from_secs(1), hide_waiter)
            .await
            .expect("hide waiter should wake on close")
            .unwrap());
    }

    #[tokio::test]
    async fn saved_servers_collapse_expand_and_reset_on_empty() {
        use glide_core::config::record_known_server;

        struct CleanupDir(PathBuf);
        impl Drop for CleanupDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "glide-ui-saved-servers-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = CleanupDir(dir.clone());

        let config_path = dir.join("config.toml");
        let known_path = glide_core::config::known_servers_path_for_config(&config_path);
        record_known_server(
            &known_path,
            KnownServer {
                name: "workstation".into(),
                address: "192.168.1.42:24800".into(),
                fingerprint: Some("ab:cd:ef".into()),
                edge: Edge::Right,
                last_connected_unix: 2000,
            },
        )
        .unwrap();
        record_known_server(
            &known_path,
            KnownServer {
                name: "studio-pc".into(),
                address: "192.168.1.88:24800".into(),
                fingerprint: Some("12:34:56".into()),
                edge: Edge::Left,
                last_connected_unix: 1000,
            },
        )
        .unwrap();

        let (quit_tx, _quit_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctrl = Arc::new(AppController::new(
            config_path,
            "http://127.0.0.1:0".into(),
            quit_tx,
        ));
        let vis = Arc::new(WindowVisibility::new(true));
        let mut ui = Ui::new(ctrl, tokio::runtime::Handle::current(), vis);

        // Saved servers starts collapsed by default even when known servers exist.
        assert_eq!(ui.known_servers.len(), 2);
        assert!(!ui.saved_servers_expanded);

        ui.tab = Tab::Devices;
        ui.role = Role::Client;

        let mut rt = rune::Runtime::new(
            ui,
            view,
            (GUI_WIDTH as f32, GUI_HEIGHT as f32),
            rune::Env::new(false, 1.0),
        );
        rt.frame(0.0);

        // Collapsed state: toggle button is present with expanded=false, card is omitted,
        // and the section sits compactly at 26px height.
        let toggle_info = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("known-servers-toggle"))
            .expect("known-servers-toggle should render when known_servers is non-empty");
        assert_eq!(toggle_info.role, "button");
        assert_eq!(toggle_info.expanded, Some(false));
        assert_eq!(toggle_info.label.as_deref(), Some("Saved servers (2)"));
        assert!(toggle_info.focusable);
        assert!(rt.find("known-servers-card").is_none());

        let sec_collapsed_h = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("known-servers-sec"))
            .expect("known-servers-sec should render")
            .rect[3];
        assert!(
            (sec_collapsed_h - 26.0).abs() < 1.0,
            "collapsed section should be 26px tall, got {sec_collapsed_h}"
        );

        // Click the disclosure toggle to expand.
        let (tx, ty) = rt.find("known-servers-toggle").unwrap();
        rt.pointer_down(tx, ty);
        rt.pointer_up(tx, ty);
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);

        assert!(rt.state.saved_servers_expanded);
        let toggle_expanded = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("known-servers-toggle"))
            .unwrap();
        assert_eq!(toggle_expanded.expanded, Some(true));
        assert!(rt.find("known-servers-card").is_some());
        assert!(rt.find("known-row-0").is_some());
        let sec_expanded = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("known-servers-sec"))
            .unwrap();
        let card_expanded = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("known-servers-card"))
            .unwrap();
        assert!(
            sec_expanded.rect[3] > sec_collapsed_h + 20.0,
            "expanded section should grow to hold the list, got rect={} target={} vs collapsed={sec_collapsed_h}",
            sec_expanded.rect[3],
            sec_expanded.target[3]
        );
        assert!(
            card_expanded.rect[3] > 20.0,
            "expanded card should receive remaining height, got rect={} target={}",
            card_expanded.rect[3],
            card_expanded.target[3]
        );

        // Click again to collapse cleanly.
        let (tx, ty) = rt.find("known-servers-toggle").unwrap();
        rt.pointer_down(tx, ty);
        rt.pointer_up(tx, ty);
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);

        assert!(!rt.state.saved_servers_expanded);
        assert!(rt.find("known-servers-card").is_none());
        let toggle_recollapsed = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("known-servers-toggle"))
            .unwrap();
        assert_eq!(toggle_recollapsed.expanded, Some(false));
        let sec_recollapsed_h = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("known-servers-sec"))
            .unwrap()
            .rect[3];
        assert!(
            (sec_recollapsed_h - 26.0).abs() < 1.0,
            "re-collapsed section should return to 26px tall, got {sec_recollapsed_h}"
        );

        // Expand, remove one server: list is non-empty so expanded state is preserved.
        rt.state.saved_servers_expanded = true;
        remove_known_server(&known_path, "12:34:56").unwrap();
        rt.state.refresh_known_servers();
        assert_eq!(rt.state.known_servers.len(), 1);
        assert!(rt.state.saved_servers_expanded);

        // Remove the last server and refresh: saved_servers_expanded resets to false.
        remove_known_server(&known_path, "ab:cd:ef").unwrap();
        rt.state.refresh_known_servers();
        assert!(rt.state.known_servers.is_empty());
        assert!(!rt.state.saved_servers_expanded);

        rt.invalidate();
        rt.frame(0.0);
        assert!(rt.find("known-servers-toggle").is_none());
        assert!(rt.find("known-servers-card").is_none());
    }

    #[tokio::test]
    async fn settings_rows_render_icons_without_clipping() {
        let preview_dir = std::env::var_os("GLIDE_RENDER_PREVIEW_DIR").map(PathBuf::from);
        if let Some(dir) = &preview_dir {
            std::fs::create_dir_all(dir).expect("create preview dir");
        }

        let dir = std::env::temp_dir().join(format!(
            "glide-ui-settings-icons-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        struct CleanupDir(PathBuf);
        impl Drop for CleanupDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = CleanupDir(dir.clone());

        let (quit_tx, _quit_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctrl = Arc::new(AppController::new(
            dir.join("config.toml"),
            "http://127.0.0.1:0".into(),
            quit_tx,
        ));

        let check_scenes: &[(&str, &[&str])] = &[
            (
                "settings-light",
                &[
                    "theme-row",
                    "autostart-row",
                    "update-channel-row",
                    "updates-row",
                    "advanced-row",
                    "sc-jump",
                    "sc-lock",
                    "sc-home",
                ],
            ),
            (
                "settings-dark",
                &[
                    "theme-row",
                    "autostart-row",
                    "update-channel-row",
                    "updates-row",
                    "advanced-row",
                    "sc-jump",
                    "sc-lock",
                    "sc-home",
                ],
            ),
            (
                "settings-windows-light",
                &[
                    "theme-row",
                    "autostart-row",
                    "elevated-row",
                    "update-channel-row",
                    "updates-row",
                    "advanced-row",
                ],
            ),
            (
                "settings-advanced-light",
                &[
                    "theme-row",
                    "advanced-row",
                    "fp-row",
                    "trust-row",
                    "firewall-row",
                    "max-file-mb-row",
                    "diag-head",
                ],
            ),
            (
                "settings-update-ready",
                &["elevated-row", "update-channel-row", "updates-row"],
            ),
            ("settings-nightly-warn-light", &["nightly-warning-modal"]),
            (
                "clipboard-filled",
                &[
                    "clip-toggle-row",
                    "clip-hist-toggle-row",
                    "dnd-toggle-row",
                    "save-folder-row",
                ],
            ),
            (
                "input-server-light",
                &[
                    "srv-thresh-row",
                    "srv-corner-row",
                    "srv-dwell-row",
                    "fs-lock-row",
                    "fs-allow-video-row",
                    "srv-sync-lock-row",
                    "srv-pin-media-row",
                    "srv-unfocus-row",
                ],
            ),
            (
                "input-client-light",
                &[
                    "sens-row",
                    "scroll-row",
                    "dpi-row",
                    "invert-row",
                    "backend-row",
                    "cli-thresh-row",
                    "cli-corner-row",
                    "cli-sync-lock-row",
                    "cli-unfocus-row",
                ],
            ),
            ("server-live", &["row-addr", "row-port", "row-discovery"]),
            ("client-connected", &["row-server", "row-pos"]),
        ];

        for &(scene_name, expected_rows) in check_scenes {
            let scene_fn = SCENES
                .iter()
                .find(|(n, _)| *n == scene_name)
                .map(|(_, f)| *f)
                .unwrap_or_else(|| panic!("scene {scene_name} not found"));

            let vis = Arc::new(WindowVisibility::new(true));
            let mut ui = Ui::new(ctrl.clone(), tokio::runtime::Handle::current(), vis);
            scene_fn(&mut ui);
            let dark = ui.theme.is_dark(false);

            let scale = if preview_dir.is_some() { 2.0 } else { 1.0 };
            let text_sys = rune::text::TextSystem::new(FONTS, ("Inter", "JetBrains Mono"));
            let mut rt = rune::Runtime::with_text(
                ui,
                view,
                (GUI_WIDTH as f32, GUI_HEIGHT as f32),
                rune::Env::new(dark, scale),
                text_sys,
            );
            rt.set_srgb_blend(true);
            rt.frame(0.0);
            rt.settle_all();
            rt.frame(0.0);

            let elems = rt.elements();
            for row_key in expected_rows {
                let info = elems
                    .iter()
                    .find(|e| e.key.as_deref() == Some(*row_key))
                    .unwrap_or_else(|| {
                        panic!("row `{row_key}` missing in scene `{scene_name}`")
                    });
                let min_h = if *row_key == "diag-head" { 24.0 } else { 36.0 };
                assert!(
                    info.rect[2] > 180.0 && info.rect[3] >= min_h,
                    "row `{row_key}` in `{scene_name}` has cramped bounds {:?}",
                    info.rect
                );
                assert!(
                    info.rect[0] + info.rect[2] <= GUI_WIDTH as f32 + 1.0,
                    "row `{row_key}` in `{scene_name}` overflows right edge: {:?}",
                    info.rect
                );
            }

            if let Some(dir) = &preview_dir {
                let render_png = |rt: &mut rune::Runtime<Ui>, filename: &str| {
                    let w = (GUI_WIDTH as f32 * scale) as u32;
                    let h = (GUI_HEIGHT as f32 * scale) as u32;
                    rune::render::cpu_boot::fill_masks(&mut rt.text.atlas, 2);
                    let scene = rune::render::cpu_boot::Scene::new(
                        &rt.instances,
                        &rt.text.atlas,
                        &rt.images,
                        (w, h),
                        rt.clear_color(),
                    )
                    .srgb_blend(rt.srgb_blend())
                    .font_smoothing(rt.font_smoothing());
                    let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
                    let mut target = rune::render::cpu_boot::Target::with_stride(
                        &mut bgra,
                        w,
                        h,
                        w as usize * 4,
                    );
                    rune::render::cpu_boot::paint(&scene, &mut target, 2)
                        .unwrap_or_else(|e| panic!("cpu paint failed for {filename}: {e:?}"));
                    for px in bgra.chunks_exact_mut(4) {
                        px.swap(0, 2);
                    }
                    let out = dir.join(filename);
                    rune::render::write_png(&out, w, h, &bgra).expect("write preview png");
                };

                render_png(&mut rt, &format!("{scene_name}.png"));

                if scene_name == "settings-light" {
                    rt.state.config.server.set_lock_shortcut(None);
                    rt.state
                        .config
                        .server
                        .set_escape_shortcut(Some(HotkeyChord::new(
                            HotkeyMods::EMPTY,
                            HidUsage::keyboard(0x45),
                        )));
                    rt.state.start_recording_shortcut(ShortcutSlot::Jump);
                    rt.state.shortcut_recorder.held_mods = HotkeyMods::CTRL | HotkeyMods::SHIFT;
                    rt.invalidate();
                    rt.frame(0.0);
                    rt.settle_all();
                    rt.frame(0.0);
                    render_png(&mut rt, "settings-shortcuts-active-light.png");
                    rt.state.stop_recording_shortcut();
                }

                if scene_name == "settings-advanced-light" {
                    // Scroll left column down so Security & firewall + Max file size are fully visible.
                    if let Some((lx, ly)) = rt.find("settings-left") {
                        rt.wheel(lx, ly, -360.0);
                        rt.frame(0.0);
                        rt.settle_all();
                        rt.frame(0.0);
                        render_png(&mut rt, "settings-advanced-light-scrolled.png");
                    }
                    // Also render dark mode advanced view.
                    rt.state.theme = ThemePref::Dark;
                    rt.env = rune::Env::new(true, scale);
                    rt.invalidate();
                    rt.frame(0.0);
                    rt.settle_all();
                    rt.frame(0.0);
                    render_png(&mut rt, "settings-advanced-dark.png");
                }
            }
        }
    }

    #[tokio::test]
    async fn shortcuts_ui_record_disable_validate_and_reset() {
        struct CleanupDir(PathBuf);
        impl Drop for CleanupDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let click = |rt: &mut rune::Runtime<Ui>, key: &str| {
            let (x, y) = rt
                .find(key)
                .unwrap_or_else(|| panic!("element `{key}` not found"));
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            rt.frame(0.0);
            rt.settle_all();
            rt.frame(0.0);
        };

        for is_windows in [false, true] {
            let dir = std::env::temp_dir().join(format!(
                "glide-ui-shortcuts-test-{}-{}",
                std::process::id(),
                if is_windows { "win" } else { "linux" }
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let _cleanup = CleanupDir(dir.clone());

            let (quit_tx, _quit_rx) = tokio::sync::mpsc::unbounded_channel();
            let ctrl = Arc::new(AppController::new(
                dir.join("config.toml"),
                "http://127.0.0.1:0".into(),
                quit_tx,
            ));

            // Attach a mock engine channel so we also verify SetHotkeysSuspended + UpdateServerTuning commands.
            let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            let dummy_task = tokio::spawn(async {});
            *ctrl.engine.lock() = Some(crate::ActiveEngine {
                task: dummy_task,
                cmd_tx,
            });

            let vis = Arc::new(WindowVisibility::new(true));
            let mut ui = Ui::new(ctrl.clone(), tokio::runtime::Handle::current(), vis);
            ui.tab = Tab::Settings;
            ui.is_windows = is_windows;
            ui.os_kind = if is_windows {
                OsKind::Windows
            } else {
                OsKind::LinuxX11
            };

            let mut rt = rune::Runtime::new(
                ui,
                view,
                (GUI_WIDTH as f32, GUI_HEIGHT as f32),
                rune::Env::new(false, 1.0),
            );
            rt.frame(0.0);
            rt.settle_all();
            rt.frame(0.0);

            // Defaults: all 3 shortcuts active, Reset defaults button hidden.
            assert!(rt.state.config.server.shortcuts_are_default());
            assert!(rt.find("sc-reset-btn").is_none());

            // 1. Toggle sc-lock-toggle OFF -> Lock becomes None, sc-reset-btn appears.
            click(&mut rt, "sc-lock-toggle");
            assert_eq!(rt.state.config.server.parsed_hotkeys().lock, None);
            assert!(rt.find("sc-reset-btn").is_some());

            // Toggle sc-lock-toggle back ON -> restores default Ctrl+Alt+L and hides sc-reset-btn.
            click(&mut rt, "sc-lock-toggle");
            assert_eq!(
                rt.state.config.server.parsed_hotkeys().lock,
                Some(HotkeyChord::DEFAULT_LOCK)
            );
            assert!(rt.find("sc-reset-btn").is_none());

            // Drain commands so far.
            while cmd_rx.try_recv().is_ok() {}

            // 2. Click sc-home-rec -> enters recording mode and suspends live hotkeys.
            click(&mut rt, "sc-home-rec");
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Home));
            let mut saw_suspend = false;
            while let Ok(cmd) = cmd_rx.try_recv() {
                if matches!(cmd, RuntimeCommand::SetHotkeysSuspended(true)) {
                    saw_suspend = true;
                }
            }
            assert!(saw_suspend, "recording must suspend live hotkeys");

            // Send F12 -> records F12, exits recording mode, resumes live hotkeys.
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::F12,
                mods: Mods::default(),
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.active, None);
            assert_eq!(
                rt.state.config.server.parsed_hotkeys().escape,
                Some(HotkeyChord::new(HotkeyMods::EMPTY, HidUsage::keyboard(0x45)))
            );
            assert_eq!(
                rt.state.shortcut_recorder.swallow_key,
                Some(PhysicalKeyCode::F12)
            );
            let mut saw_resume = false;
            let mut saw_tuning_escape_f12 = false;
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    RuntimeCommand::SetHotkeysSuspended(false) => saw_resume = true,
                    RuntimeCommand::UpdateServerTuning { hotkeys, .. }
                        if hotkeys.escape
                            == Some(HotkeyChord::new(
                                HotkeyMods::EMPTY,
                                HidUsage::keyboard(0x45),
                            )) =>
                    {
                        saw_tuning_escape_f12 = true;
                    }
                    _ => {}
                }
            }
            assert!(saw_resume, "finishing recording must resume live hotkeys");
            assert!(
                saw_tuning_escape_f12,
                "recording must push updated CaptureHotkeys to running engine"
            );

            // Releasing F12 clears swallow_key cleanly.
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::F12,
                mods: Mods::default(),
                pressed: false,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.swallow_key, None);

            // Toggling Home OFF then ON remembers custom F12 binding instead of resetting to ScrollLock.
            click(&mut rt, "sc-home-toggle");
            assert_eq!(rt.state.config.server.parsed_hotkeys().escape, None);
            click(&mut rt, "sc-home-toggle");
            assert_eq!(
                rt.state.config.server.parsed_hotkeys().escape,
                Some(HotkeyChord::new(HotkeyMods::EMPTY, HidUsage::keyboard(0x45)))
            );

            // 3. Click sc-jump-rec -> send Ctrl+Shift+ArrowRight -> records Ctrl+Shift.
            click(&mut rt, "sc-jump-rec");
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Jump));
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::ArrowRight,
                mods: Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::default()
                },
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.active, None);
            assert_eq!(
                rt.state.config.server.parsed_hotkeys().switch_mods,
                Some(HotkeyMods::CTRL | HotkeyMods::SHIFT)
            );
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::ArrowRight,
                mods: Mods::default(),
                pressed: false,
                repeat: false,
            });
            rt.frame(0.0);

            // Also verify Super keycap label adapts to OS (Win on Windows, Super on Linux).
            click(&mut rt, "sc-jump-rec");
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::ArrowLeft,
                mods: Mods {
                    logo: true,
                    ..Mods::default()
                },
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(
                rt.state.config.server.parsed_hotkeys().switch_mods,
                Some(HotkeyMods::SUPER)
            );
            let jump_label = rt
                .elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some("sc-jump-rec"))
                .and_then(|e| e.label)
                .unwrap_or_default();
            if is_windows {
                assert!(
                    jump_label.contains("Win+Arrow"),
                    "Windows keycap should use Win, got {jump_label}"
                );
            } else {
                assert!(
                    jump_label.contains("Super+Arrow"),
                    "Linux keycap should use Super, got {jump_label}"
                );
            }
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::ArrowLeft,
                mods: Mods::default(),
                pressed: false,
                repeat: false,
            });
            rt.frame(0.0);

            // Re-set Jump to Ctrl+Shift for subsequent checks.
            click(&mut rt, "sc-jump-rec");
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::ArrowRight,
                mods: Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::default()
                },
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(
                rt.state.config.server.parsed_hotkeys().switch_mods,
                Some(HotkeyMods::CTRL | HotkeyMods::SHIFT)
            );

            // 4. Click sc-lock-rec -> test rejections (Win+L, cross-slot F12, cross-slot Jump arrow, AltGr) + Escape cancel.
            click(&mut rt, "sc-lock-rec");
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Lock));

            // Win+L rejected (reserved by OS).
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::KeyL,
                mods: Mods {
                    logo: true,
                    ..Mods::default()
                },
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Lock));
            assert!(
                rt.state
                    .active_toast()
                    .is_some_and(|t| t.contains("Win+L")),
                "expected Win+L rejection toast, got {:?}",
                rt.state.active_toast()
            );

            // Cross-slot conflict: F12 is already used by Bring cursor home.
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::F12,
                mods: Mods::default(),
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Lock));
            assert!(
                rt.state
                    .active_toast()
                    .is_some_and(|t| t.contains("Bring cursor home")),
                "expected conflict toast with Home, got {:?}",
                rt.state.active_toast()
            );

            // Cross-slot conflict: Ctrl+Shift+ArrowLeft collides with Jump (Ctrl+Shift+Arrow).
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::ArrowLeft,
                mods: Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::default()
                },
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Lock));
            assert!(
                rt.state
                    .active_toast()
                    .is_some_and(|t| t.contains("Jump to a side")),
                "expected conflict toast with Jump, got {:?}",
                rt.state.active_toast()
            );

            // AltRight (AltGr) rejected and does not populate Ctrl/Alt in held_mods.
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::AltRight,
                mods: Mods {
                    ctrl: true,
                    alt: true,
                    ..Mods::default()
                },
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Lock));
            assert!(rt.state.shortcut_recorder.altgr_held);
            assert_eq!(rt.state.shortcut_recorder.held_mods, HotkeyMods::EMPTY);
            assert!(
                rt.state
                    .active_toast()
                    .is_some_and(|t| t.contains("AltGr")),
                "expected AltGr rejection toast, got {:?}",
                rt.state.active_toast()
            );

            // Unmodified Escape cancels cleanly.
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::Escape,
                mods: Mods::default(),
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.active, None);
            assert!(!rt.state.shortcut_recorder.altgr_held);
            assert_eq!(
                rt.state.config.server.parsed_hotkeys().lock,
                Some(HotkeyChord::DEFAULT_LOCK)
            );
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::Escape,
                mods: Mods::default(),
                pressed: false,
                repeat: false,
            });
            rt.frame(0.0);

            // 5. Click sc-lock-rec -> unmodified Backspace disables Lock.
            click(&mut rt, "sc-lock-rec");
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Lock));
            rt.physical_input(PhysicalInput::Key {
                key: PhysicalKeyCode::Backspace,
                mods: Mods::default(),
                pressed: true,
                repeat: false,
            });
            rt.frame(0.0);
            assert_eq!(rt.state.shortcut_recorder.active, None);
            assert_eq!(rt.state.config.server.parsed_hotkeys().lock, None);
            assert_eq!(rt.state.active_toast(), Some("Shortcut disabled"));

            // 6. Verify switching tabs stops recording and resumes hotkeys.
            click(&mut rt, "sc-home-rec");
            assert_eq!(rt.state.shortcut_recorder.active, Some(ShortcutSlot::Home));
            while cmd_rx.try_recv().is_ok() {}
            click(&mut rt, "nav-Devices");
            assert_eq!(rt.state.tab, Tab::Devices);
            assert_eq!(rt.state.shortcut_recorder.active, None);
            let mut resumed_on_tab_switch = false;
            while let Ok(cmd) = cmd_rx.try_recv() {
                if matches!(cmd, RuntimeCommand::SetHotkeysSuspended(false)) {
                    resumed_on_tab_switch = true;
                }
            }
            assert!(resumed_on_tab_switch);

            // Return to Settings and click sc-reset-btn -> restores all 3 defaults.
            click(&mut rt, "nav-Settings");
            assert!(rt.find("sc-reset-btn").is_some());
            click(&mut rt, "sc-reset-btn");
            assert!(rt.state.config.server.shortcuts_are_default());
            assert_eq!(
                rt.state.config.server.parsed_hotkeys(),
                CaptureHotkeys::default()
            );
            assert!(rt.find("sc-reset-btn").is_none());
        }
    }

    #[tokio::test]
    async fn stable_to_nightly_switch_requires_warning_confirmation() {
        struct CleanupDir(PathBuf);
        impl Drop for CleanupDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "glide-ui-nightly-warn-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = CleanupDir(dir.clone());

        let (quit_tx, _quit_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctrl = Arc::new(AppController::new(
            dir.join("config.toml"),
            "http://127.0.0.1:0".into(),
            quit_tx,
        ));
        let vis = Arc::new(WindowVisibility::new(true));
        let mut ui = Ui::new(ctrl, tokio::runtime::Handle::current(), vis);

        assert_eq!(ui.config.update_channel, UpdateChannel::build_channel());
        assert!(!ui.show_nightly_warning);
        ui.config.update_channel = UpdateChannel::Stable;
        ui.tab = Tab::Settings;
        ui.theme = ThemePref::Light;

        let text_sys = rune::text::TextSystem::new(FONTS, ("Inter", "JetBrains Mono"));
        let mut rt = rune::Runtime::with_text(
            ui,
            view,
            (GUI_WIDTH as f32, GUI_HEIGHT as f32),
            rune::Env::new(false, 1.0),
            text_sys,
        );
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);

        let click_key = |rt: &mut rune::Runtime<Ui>, key: &str| {
            let (x, y) = rt
                .find(key)
                .unwrap_or_else(|| panic!("element `{key}` should exist"));
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            rt.frame(0.0);
            rt.settle_all();
            rt.frame(0.0);
        };

        assert!(rt.find("nightly-warning-modal").is_none());

        // Clicking Nightly from Stable opens the confirmation modal without switching yet.
        click_key(&mut rt, "Nightly");
        assert!(rt.state.show_nightly_warning);
        assert_eq!(rt.state.config.update_channel, UpdateChannel::Stable);

        let modal = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("nightly-warning-modal"))
            .expect("nightly-warning-modal should render inside 900x640");
        assert!(
            modal.rect[0] >= 0.0
                && modal.rect[1] >= 0.0
                && modal.rect[0] + modal.rect[2] <= GUI_WIDTH as f32 + 1.0
                && modal.rect[1] + modal.rect[3] <= GUI_HEIGHT as f32 + 1.0
                && modal.rect[2] >= 400.0
                && modal.rect[3] >= 100.0,
            "nightly-warning-modal bounds should fit cleanly within 900x640, got {:?}",
            modal.rect
        );
        assert!(rt.find("nightly-warn-cancel").is_some());
        assert!(rt.find("nightly-warn-confirm").is_some());

        // Cancel dismisses the modal and stays on Stable.
        click_key(&mut rt, "nightly-warn-cancel");
        assert!(!rt.state.show_nightly_warning);
        assert_eq!(rt.state.config.update_channel, UpdateChannel::Stable);
        assert!(rt.find("nightly-warning-modal").is_none());

        // Trigger Nightly again and confirm the switch.
        click_key(&mut rt, "Nightly");
        assert!(rt.state.show_nightly_warning);
        assert_eq!(rt.state.config.update_channel, UpdateChannel::Stable);

        click_key(&mut rt, "nightly-warn-confirm");
        assert!(!rt.state.show_nightly_warning);
        assert_eq!(rt.state.config.update_channel, UpdateChannel::Nightly);
        assert!(rt.find("nightly-warning-modal").is_none());
        assert!(matches!(rt.state.update_state, UpdateCheckState::Checking));

        // Switching Nightly -> Stable happens immediately with no modal.
        click_key(&mut rt, "Stable");
        assert!(!rt.state.show_nightly_warning);
        assert_eq!(rt.state.config.update_channel, UpdateChannel::Stable);
        assert!(rt.find("nightly-warning-modal").is_none());
    }

    #[tokio::test]
    async fn unfocus_on_leave_toggle_updates_config_and_hides_on_wayland() {
        struct CleanupDir(PathBuf);
        impl Drop for CleanupDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "glide-ui-unfocus-on-leave-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = CleanupDir(dir.clone());

        let (quit_tx, _quit_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctrl = Arc::new(AppController::new(
            dir.join("config.toml"),
            "http://127.0.0.1:0".into(),
            quit_tx,
        ));

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let dummy_task = tokio::spawn(async {});
        *ctrl.engine.lock() = Some(crate::ActiveEngine {
            task: dummy_task,
            cmd_tx,
        });

        let vis = Arc::new(WindowVisibility::new(true));
        let mut ui = Ui::new(ctrl, tokio::runtime::Handle::current(), vis);
        ui.tab = Tab::Input;
        ui.role = Role::Server;
        ui.theme = ThemePref::Light;
        ui.is_windows = true;
        ui.os_kind = OsKind::Windows;
        ui.os_label = "Windows";

        assert!(!ui.config.server.unfocus_on_leave);
        assert!(!ui.config.client.unfocus_on_leave);

        let text_sys = rune::text::TextSystem::new(FONTS, ("Inter", "JetBrains Mono"));
        let mut rt = rune::Runtime::with_text(
            ui,
            view,
            (GUI_WIDTH as f32, GUI_HEIGHT as f32),
            rune::Env::new(false, 1.0),
            text_sys,
        );
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);

        let click_key = |rt: &mut rune::Runtime<Ui>, key: &str| {
            let (x, y) = rt
                .find(key)
                .unwrap_or_else(|| panic!("element `{key}` should exist"));
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            rt.frame(0.0);
            rt.settle_all();
            rt.frame(0.0);
        };

        let assert_row_fits = |rt: &rune::Runtime<Ui>, key: &str| {
            let info = rt
                .elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some(key))
                .unwrap_or_else(|| panic!("row `{key}` should render"));
            assert!(
                info.rect[2] > 180.0
                    && info.rect[3] >= 36.0
                    && info.rect[0] + info.rect[2] <= GUI_WIDTH as f32 + 1.0
                    && info.rect[1] + info.rect[3] <= GUI_HEIGHT as f32 + 1.0,
                "row `{key}` should fit cleanly inside 900x640, got {:?}",
                info.rect
            );
        };

        // 1. Server (non-Wayland): row renders within bounds and toggle flips false -> true -> false.
        assert_row_fits(&rt, "srv-unfocus-row");

        click_key(&mut rt, "srv-unfocus-toggle");
        assert!(rt.state.config.server.unfocus_on_leave);
        let mut saw_srv_true = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(
                cmd,
                RuntimeCommand::UpdateServerTuning {
                    unfocus_on_leave: true,
                    ..
                }
            ) {
                saw_srv_true = true;
            }
        }
        assert!(
            saw_srv_true,
            "toggling srv-unfocus-toggle ON must emit UpdateServerTuning {{ unfocus_on_leave: true }}"
        );

        click_key(&mut rt, "srv-unfocus-toggle");
        assert!(!rt.state.config.server.unfocus_on_leave);
        let mut saw_srv_false = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(
                cmd,
                RuntimeCommand::UpdateServerTuning {
                    unfocus_on_leave: false,
                    ..
                }
            ) {
                saw_srv_false = true;
            }
        }
        assert!(
            saw_srv_false,
            "toggling srv-unfocus-toggle OFF must emit UpdateServerTuning {{ unfocus_on_leave: false }}"
        );

        // 2. Client (Linux X11, with backend-row also visible): row fits within 900x640 and flips false -> true -> false.
        rt.state.role = Role::Client;
        rt.state.is_windows = false;
        rt.state.os_kind = OsKind::LinuxX11;
        rt.state.os_label = "Linux";
        rt.invalidate();
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);

        assert!(rt.find("backend-row").is_some());
        assert_row_fits(&rt, "cli-unfocus-row");

        click_key(&mut rt, "cli-unfocus-toggle");
        assert!(rt.state.config.client.unfocus_on_leave);
        let mut saw_cli_true = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(
                cmd,
                RuntimeCommand::UpdateClientTuning {
                    unfocus_on_leave: true,
                    ..
                }
            ) {
                saw_cli_true = true;
            }
        }
        assert!(
            saw_cli_true,
            "toggling cli-unfocus-toggle ON must emit UpdateClientTuning {{ unfocus_on_leave: true }}"
        );

        click_key(&mut rt, "cli-unfocus-toggle");
        assert!(!rt.state.config.client.unfocus_on_leave);
        let mut saw_cli_false = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(
                cmd,
                RuntimeCommand::UpdateClientTuning {
                    unfocus_on_leave: false,
                    ..
                }
            ) {
                saw_cli_false = true;
            }
        }
        assert!(
            saw_cli_false,
            "toggling cli-unfocus-toggle OFF must emit UpdateClientTuning {{ unfocus_on_leave: false }}"
        );

        // 3. Switching to LinuxWayland hides both cli-unfocus-row and srv-unfocus-row cleanly.
        rt.state.os_kind = OsKind::LinuxWayland;
        rt.state.os_label = "Linux (Wayland)";
        rt.invalidate();
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);
        assert!(rt.find("cli-unfocus-row").is_none());
        assert!(rt.find("cli-unfocus-toggle").is_none());

        rt.state.role = Role::Server;
        rt.invalidate();
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);
        assert!(rt.find("srv-unfocus-row").is_none());
        assert!(rt.find("srv-unfocus-toggle").is_none());
    }

    #[tokio::test]
    async fn allow_fullscreen_videos_toggle_updates_config_and_follows_fullscreen_lock() {
        struct CleanupDir(PathBuf);
        impl Drop for CleanupDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "glide-ui-allow-fullscreen-videos-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = CleanupDir(dir.clone());

        let (quit_tx, _quit_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctrl = Arc::new(AppController::new(
            dir.join("config.toml"),
            "http://127.0.0.1:0".into(),
            quit_tx,
        ));

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let dummy_task = tokio::spawn(async {});
        *ctrl.engine.lock() = Some(crate::ActiveEngine {
            task: dummy_task,
            cmd_tx,
        });

        let vis = Arc::new(WindowVisibility::new(true));
        let mut ui = Ui::new(ctrl, tokio::runtime::Handle::current(), vis);
        ui.tab = Tab::Input;
        ui.role = Role::Server;
        ui.theme = ThemePref::Light;
        ui.is_windows = true;
        ui.os_kind = OsKind::Windows;
        ui.os_label = "Windows";

        assert!(ui.config.server.auto_lock_fullscreen);
        assert!(ui.config.server.allow_fullscreen_videos);

        let text_sys = rune::text::TextSystem::new(FONTS, ("Inter", "JetBrains Mono"));
        let mut rt = rune::Runtime::with_text(
            ui,
            view,
            (GUI_WIDTH as f32, GUI_HEIGHT as f32),
            rune::Env::new(false, 1.0),
            text_sys,
        );
        rt.frame(0.0);
        rt.settle_all();
        rt.frame(0.0);

        let click_key = |rt: &mut rune::Runtime<Ui>, key: &str| {
            let (x, y) = rt
                .find(key)
                .unwrap_or_else(|| panic!("element `{key}` should exist"));
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            rt.frame(0.0);
            rt.settle_all();
            rt.frame(0.0);
        };

        let assert_row_fits = |rt: &rune::Runtime<Ui>, key: &str| {
            let info = rt
                .elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some(key))
                .unwrap_or_else(|| panic!("row `{key}` should render"));
            assert!(
                info.rect[2] > 180.0
                    && info.rect[3] >= 36.0
                    && info.rect[0] + info.rect[2] <= GUI_WIDTH as f32 + 1.0
                    && info.rect[1] + info.rect[3] <= GUI_HEIGHT as f32 + 1.0,
                "row `{key}` should fit cleanly inside 900x640, got {:?}",
                info.rect
            );
        };

        // 1. With auto_lock_fullscreen = true, fs-allow-video-row (and the full 8-row edges-card)
        // fits cleanly inside 900x640, and toggling flips true -> false -> true with live commands.
        assert_row_fits(&rt, "fs-allow-video-row");
        assert_row_fits(&rt, "srv-unfocus-row");

        click_key(&mut rt, "fs-allow-video-toggle");
        assert!(!rt.state.config.server.allow_fullscreen_videos);
        let mut saw_video_false = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(
                cmd,
                RuntimeCommand::UpdateServerTuning {
                    auto_lock_fullscreen: true,
                    allow_fullscreen_videos: false,
                    ..
                }
            ) {
                saw_video_false = true;
            }
        }
        assert!(
            saw_video_false,
            "toggling fs-allow-video-toggle OFF must emit UpdateServerTuning {{ allow_fullscreen_videos: false }}"
        );

        click_key(&mut rt, "fs-allow-video-toggle");
        assert!(rt.state.config.server.allow_fullscreen_videos);
        let mut saw_video_true = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(
                cmd,
                RuntimeCommand::UpdateServerTuning {
                    auto_lock_fullscreen: true,
                    allow_fullscreen_videos: true,
                    ..
                }
            ) {
                saw_video_true = true;
            }
        }
        assert!(
            saw_video_true,
            "toggling fs-allow-video-toggle ON must emit UpdateServerTuning {{ allow_fullscreen_videos: true }}"
        );

        // 2. Flip allow_fullscreen_videos to false (non-default), then toggle fs-lock-toggle OFF
        // and back ON to confirm fs-allow-video-row hides cleanly and restores with saved value.
        click_key(&mut rt, "fs-allow-video-toggle");
        assert!(!rt.state.config.server.allow_fullscreen_videos);
        while cmd_rx.try_recv().is_ok() {}

        click_key(&mut rt, "fs-lock-toggle");
        assert!(!rt.state.config.server.auto_lock_fullscreen);
        assert!(!rt.state.config.server.allow_fullscreen_videos);
        assert!(rt.find("fs-allow-video-row").is_none());
        assert!(rt.find("fs-allow-video-toggle").is_none());
        let mut saw_lock_off = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(
                cmd,
                RuntimeCommand::UpdateServerTuning {
                    auto_lock_fullscreen: false,
                    allow_fullscreen_videos: false,
                    ..
                }
            ) {
                saw_lock_off = true;
            }
        }
        assert!(
            saw_lock_off,
            "toggling fs-lock-toggle OFF must emit UpdateServerTuning {{ auto_lock_fullscreen: false, allow_fullscreen_videos: false }}"
        );

        click_key(&mut rt, "fs-lock-toggle");
        assert!(rt.state.config.server.auto_lock_fullscreen);
        assert!(!rt.state.config.server.allow_fullscreen_videos);
        assert_row_fits(&rt, "fs-allow-video-row");
        assert!(rt.find("fs-allow-video-toggle").is_some());
        let mut saw_lock_on = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(
                cmd,
                RuntimeCommand::UpdateServerTuning {
                    auto_lock_fullscreen: true,
                    allow_fullscreen_videos: false,
                    ..
                }
            ) {
                saw_lock_on = true;
            }
        }
        assert!(
            saw_lock_on,
            "toggling fs-lock-toggle back ON must preserve allow_fullscreen_videos: false in UpdateServerTuning"
        );
    }
}
