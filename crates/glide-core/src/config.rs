//! Clean TOML configuration structures and defaults for Glide server and client.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use glide_backend::InputBackendKind;
use glide_hid::HidUsage;
use glide_net::discovery::{machine_hostname, DEFAULT_DISCOVERY_PORT};
use glide_proto::Edge;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_QUIC_PORT: u16 = 24800;

/// Returns the default OS-native config directory for Glide:
/// - Linux: `~/.config/glide`
/// - Windows: `%APPDATA%\glide\config`
/// - macOS: `~/Library/Application Support/dev.glide.glide`
pub fn default_config_dir() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("dev", "glide", "glide") {
        dirs.config_dir().to_path_buf()
    } else {
        PathBuf::from(".glide")
    }
}

/// Declarative physical key remap rule (e.g. `CapsLock` -> `ControlLeft`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRemapRule {
    pub from: String,
    pub to: String,
}

/// Compiles declarative key remap rules into a fast `HidUsage -> HidUsage` lookup map.
pub fn compile_key_remaps(rules: &[KeyRemapRule]) -> HashMap<HidUsage, HidUsage> {
    let mut map = HashMap::new();
    for rule in rules {
        if let (Some(from), Some(to)) = (
            HidUsage::from_name(&rule.from),
            HidUsage::from_name(&rule.to),
        ) {
            map.insert(from.canonicalize(), to.canonicalize());
        }
    }
    map
}

pub const DEFAULT_MAX_FILE_MB: u32 = 256;
pub const MIN_MAX_FILE_MB: u32 = 1;
pub const MAX_MAX_FILE_MB: u32 = 4096;

fn default_true() -> bool {
    true
}

fn default_max_file_mb() -> u32 {
    DEFAULT_MAX_FILE_MB
}

/// Top-level `glide.toml` file supporting both `[server]` and `[client]` sections
/// so the same machine can switch roles effortlessly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlideConfigFile {
    /// Last active GUI role (`"server"` or `"client"`), restored on next launch.
    #[serde(default)]
    pub last_role: Option<String>,
    /// Automatically start the server or connect as client when the GUI launches.
    #[serde(default)]
    pub auto_start_on_launch: bool,
    /// Keep an in-memory history of recently synced clipboard items (on by default).
    #[serde(default = "default_true")]
    pub clipboard_history: bool,
    /// Hide clipboard history entries behind a privacy cover until explicitly revealed.
    #[serde(default)]
    pub hide_clipboard_history: bool,
    /// Maximum single file transfer size in megabytes (default `256`, safely clamped to `1..=4096`).
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u32,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub client: ClientConfig,
}

impl Default for GlideConfigFile {
    fn default() -> Self {
        Self {
            last_role: None,
            auto_start_on_launch: false,
            clipboard_history: true,
            hide_clipboard_history: false,
            max_file_mb: DEFAULT_MAX_FILE_MB,
            server: ServerConfig::default(),
            client: ClientConfig::default(),
        }
    }
}

impl GlideConfigFile {
    /// Clamps a requested max file size in MB to the safe range `1..=4096`.
    pub fn clamp_max_file_mb(mb: u32) -> u32 {
        mb.clamp(MIN_MAX_FILE_MB, MAX_MAX_FILE_MB)
    }

    /// Returns `max_file_mb` clamped safely to `1..=4096`.
    pub fn max_file_mb_clamped(&self) -> u32 {
        Self::clamp_max_file_mb(self.max_file_mb)
    }

    /// Converts a MB limit into a wire-safe byte ceiling (clamped to `1..=4096` MB and `u32::MAX` bytes).
    pub fn max_file_bytes_for_mb(mb: u32) -> usize {
        let bytes = u64::from(Self::clamp_max_file_mb(mb)).saturating_mul(1024 * 1024);
        bytes.min(u32::MAX as u64).min(usize::MAX as u64) as usize
    }

    /// Returns the effective single-file transfer ceiling in bytes.
    pub fn max_file_bytes(&self) -> usize {
        Self::max_file_bytes_for_mb(self.max_file_mb)
    }

    /// Returns a `ServerConfig` populated with top-level shared settings (such as `max_file_mb`).
    pub fn effective_server_config(&self) -> ServerConfig {
        let mut cfg = self.server.clone();
        cfg.max_file_bytes = Some(self.max_file_bytes());
        cfg
    }

    /// Returns a `ClientConfig` populated with top-level shared settings (such as `max_file_mb`).
    pub fn effective_client_config(&self) -> ClientConfig {
        let mut cfg = self.client.clone();
        cfg.max_file_bytes = Some(self.max_file_bytes());
        cfg
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse TOML config {}", path.display()))?;
        cfg.max_file_mb = cfg.max_file_mb_clamped();
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut normalized = self.clone();
        normalized.max_file_mb = normalized.max_file_mb_clamped();
        let body = toml::to_string_pretty(&normalized)?;
        fs::write(path, body)
            .with_context(|| format!("failed to write config file {}", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Human-readable server name (defaults to real machine hostname).
    pub name: String,
    /// UDP address to bind QUIC listener (default `0.0.0.0:24800`).
    pub bind: SocketAddr,
    /// Enable LocalSend-style LAN auto-discovery beacons.
    pub discovery: bool,
    /// UDP port for LAN discovery (default `24801`).
    pub discovery_port: u16,
    /// Physical key that unconditionally returns control to the server desktop.
    pub escape_key: String,
    /// Outward push threshold in pixels at a screen border before switching.
    pub edge_threshold_px: i32,
    /// Corner dead-zone in pixels (prevents accidental switches on close `X` / Show Desktop).
    pub dead_corner_px: i32,
    /// Optional edge dwell delay in milliseconds before crossing (0 = instant).
    pub edge_dwell_ms: u64,
    /// Automatically pause edge switching while a fullscreen game/app is active.
    pub auto_lock_fullscreen: bool,
    /// Enable `Ctrl+Alt+Arrow` direct screen switching and `Ctrl+Alt+L` cursor lock toggle.
    pub enable_hotkeys: bool,
    /// Sync CapsLock and NumLock state onto clients on screen entry.
    pub sync_lock_keys: bool,
    /// Enable bidirectional clipboard sharing (UTF-8 text & PNG/DIB images).
    pub clipboard: bool,
    /// Microseconds to coalesce high-polling-rate mouse deltas (1000 = 1000Hz max cadence; 0 = immediate).
    pub motion_coalesce_us: u64,
    /// Directory storing the server's Ed25519 TLS identity.
    pub identity_dir: Option<PathBuf>,
    /// Effective maximum file transfer size in bytes (populated from top-level `max_file_mb`).
    #[serde(skip)]
    pub max_file_bytes: Option<usize>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: machine_hostname(),
            bind: format!("0.0.0.0:{DEFAULT_QUIC_PORT}").parse().unwrap(),
            discovery: true,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            escape_key: "ScrollLock".into(),
            edge_threshold_px: 4,
            dead_corner_px: 24,
            edge_dwell_ms: 0,
            auto_lock_fullscreen: true,
            enable_hotkeys: true,
            sync_lock_keys: true,
            clipboard: true,
            motion_coalesce_us: 1000,
            identity_dir: None,
            max_file_bytes: None,
        }
    }
}

impl ServerConfig {
    pub fn parsed_escape_key(&self) -> HidUsage {
        HidUsage::from_name(&self.escape_key).unwrap_or(HidUsage::SCROLL_LOCK)
    }

    pub fn resolved_identity_dir(&self) -> PathBuf {
        self.identity_dir
            .clone()
            .unwrap_or_else(|| default_config_dir().join("identity"))
    }

    pub fn resolved_max_file_bytes(&self) -> usize {
        self.max_file_bytes
            .unwrap_or(glide_proto::DEFAULT_MAX_FILE_BYTES)
            .min(u32::MAX as usize)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    /// Human-readable client name (defaults to real machine hostname).
    pub name: String,
    /// Optional explicit server address (`host:port`). If omitted, auto-discovers on LAN!
    pub server: Option<String>,
    /// Optional server hostname filter when using LAN auto-discovery.
    pub server_name: Option<String>,
    /// Which edge of the *server* this client sits on (`left`, `right`, `top`, `bottom`).
    pub edge: Edge,
    /// UDP port for LAN discovery (default `24801`).
    pub discovery_port: u16,
    /// Optional pinned SHA-256 fingerprint. If omitted, uses automatic Trust-On-First-Use (TOFU).
    pub trusted_fingerprint: Option<String>,
    /// Skip TLS fingerprint verification (local debugging only).
    pub insecure_skip_verify: bool,
    /// Mouse speed multiplier applied to incoming raw relative deltas (default `1.0`).
    pub sensitivity: f32,
    /// Automatically scale incoming mouse deltas by `client_dpi_scale / server_dpi_scale`.
    pub auto_dpi_scale: bool,
    /// Scroll wheel speed multiplier (default `1.0`).
    pub scroll_speed: f32,
    /// Invert scroll direction ("natural scrolling").
    pub invert_scroll: bool,
    /// Outward push threshold in pixels to return control back to the server.
    pub edge_threshold_px: i32,
    /// Corner dead-zone in pixels on the return edge.
    pub dead_corner_px: i32,
    /// Sync CapsLock / NumLock state when entering this client.
    pub sync_lock_keys: bool,
    /// Input injection backend (`auto`, `xtest`, or `uinput` on Linux).
    pub input_backend: InputBackendKind,
    /// Declarative per-client physical key remapping rules.
    pub key_remap: Vec<KeyRemapRule>,
    /// Enable bidirectional clipboard sharing.
    pub clipboard: bool,
    /// Path to persist TOFU server fingerprints.
    pub tofu_store_path: Option<PathBuf>,
    /// Effective maximum file transfer size in bytes (populated from top-level `max_file_mb`).
    #[serde(skip)]
    pub max_file_bytes: Option<usize>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            name: machine_hostname(),
            server: None,
            server_name: None,
            edge: Edge::Right,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            trusted_fingerprint: None,
            insecure_skip_verify: false,
            sensitivity: 1.0,
            auto_dpi_scale: false,
            scroll_speed: 1.0,
            invert_scroll: false,
            edge_threshold_px: 4,
            dead_corner_px: 24,
            sync_lock_keys: true,
            input_backend: InputBackendKind::Auto,
            key_remap: Vec::new(),
            clipboard: true,
            tofu_store_path: None,
            max_file_bytes: None,
        }
    }
}

impl ClientConfig {
    pub fn resolved_tofu_path(&self) -> PathBuf {
        self.tofu_store_path
            .clone()
            .unwrap_or_else(|| default_config_dir().join("trusted_server.fp"))
    }

    pub fn resolved_max_file_bytes(&self) -> usize {
        self.max_file_bytes
            .unwrap_or(glide_proto::DEFAULT_MAX_FILE_BYTES)
            .min(u32::MAX as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_last_role_and_auto_start() {
        // Older configs without last_role / auto_start_on_launch / max_file_mb parse cleanly.
        let legacy: GlideConfigFile = toml::from_str(
            r#"
            [server]
            edge_threshold_px = 6

            [client]
            sensitivity = 1.5
            "#,
        )
        .unwrap();
        assert_eq!(legacy.last_role, None);
        assert!(!legacy.auto_start_on_launch);
        assert!(legacy.clipboard_history);
        assert!(!legacy.hide_clipboard_history);
        assert_eq!(legacy.max_file_mb, DEFAULT_MAX_FILE_MB);
        assert_eq!(legacy.max_file_bytes(), glide_proto::DEFAULT_MAX_FILE_BYTES);
        assert_eq!(legacy.server.edge_threshold_px, 6);
        assert!((legacy.client.sensitivity - 1.5).abs() < f32::EPSILON);

        let mut cfg = GlideConfigFile {
            last_role: Some("client".into()),
            auto_start_on_launch: true,
            clipboard_history: false,
            hide_clipboard_history: true,
            max_file_mb: 1024,
            ..GlideConfigFile::default()
        };
        cfg.client.sensitivity = 1.35;
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let parsed: GlideConfigFile = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.last_role.as_deref(), Some("client"));
        assert!(parsed.auto_start_on_launch);
        assert!(!parsed.clipboard_history);
        assert!(parsed.hide_clipboard_history);
        assert_eq!(parsed.max_file_mb, 1024);
        assert_eq!(parsed.max_file_bytes(), 1024 * 1024 * 1024);
        assert_eq!(
            parsed.effective_server_config().resolved_max_file_bytes(),
            1024 * 1024 * 1024
        );
        assert_eq!(
            parsed.effective_client_config().resolved_max_file_bytes(),
            1024 * 1024 * 1024
        );
        assert!((parsed.client.sensitivity - 1.35).abs() < f32::EPSILON);
    }

    #[test]
    fn max_file_mb_clamps_safely_and_persists() {
        assert_eq!(GlideConfigFile::clamp_max_file_mb(0), MIN_MAX_FILE_MB);
        assert_eq!(GlideConfigFile::clamp_max_file_mb(512), 512);
        assert_eq!(GlideConfigFile::clamp_max_file_mb(99999), MAX_MAX_FILE_MB);
        assert_eq!(GlideConfigFile::max_file_bytes_for_mb(0), 1024 * 1024);
        assert_eq!(
            GlideConfigFile::max_file_bytes_for_mb(4096),
            u32::MAX as usize
        );

        let tmp = std::env::temp_dir().join(format!(
            "glide-cfg-max-file-{}.toml",
            std::process::id()
        ));
        let cfg = GlideConfigFile {
            max_file_mb: 8192,
            ..GlideConfigFile::default()
        };
        cfg.save(&tmp).unwrap();
        let reloaded = GlideConfigFile::load(&tmp).unwrap();
        assert_eq!(reloaded.max_file_mb, MAX_MAX_FILE_MB);
        let _ = std::fs::remove_file(&tmp);
    }
}
