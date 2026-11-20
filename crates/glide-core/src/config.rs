//! Clean TOML configuration structures and defaults for Glide server and client.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use glide_hid::HidUsage;
use glide_net::discovery::{machine_hostname, DEFAULT_DISCOVERY_PORT};
use glide_proto::Edge;
use serde::{Deserialize, Serialize};
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

/// Top-level `glide.toml` file supporting both `[server]` and `[client]` sections
/// so the same machine can switch roles effortlessly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlideConfigFile {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub client: ClientConfig,
}

impl GlideConfigFile {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("failed to parse TOML config {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self)?;
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
    /// Enable bidirectional clipboard sharing (UTF-8 text & PNG images).
    pub clipboard: bool,
    /// Microseconds to coalesce high-polling-rate mouse deltas (1000 = 1000Hz max cadence; 0 = immediate).
    pub motion_coalesce_us: u64,
    /// Directory storing the server's Ed25519 TLS identity.
    pub identity_dir: Option<PathBuf>,
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
            clipboard: true,
            motion_coalesce_us: 1000,
            identity_dir: None,
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
    /// Outward push threshold in pixels to return control back to the server.
    pub edge_threshold_px: i32,
    /// Enable bidirectional clipboard sharing.
    pub clipboard: bool,
    /// Path to persist TOFU server fingerprints.
    pub tofu_store_path: Option<PathBuf>,
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
            edge_threshold_px: 4,
            clipboard: true,
            tofu_store_path: None,
        }
    }
}

impl ClientConfig {
    pub fn resolved_tofu_path(&self) -> PathBuf {
        self.tofu_store_path
            .clone()
            .unwrap_or_else(|| default_config_dir().join("trusted_server.fp"))
    }
}
