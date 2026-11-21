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
    /// Optional custom folder for received files (defaults to `Downloads/Glide` when `None`).
    #[serde(default)]
    pub receive_dir: Option<String>,
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
            receive_dir: None,
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

    /// Normalizes a configured custom receive directory (`Some("  ")` -> `None`).
    pub fn normalized_receive_dir(raw: Option<&str>) -> Option<String> {
        raw.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// Resolves the effective receive folder (expanding leading `~/` or falling back to `Downloads/Glide`).
    pub fn resolved_receive_dir(&self) -> Result<PathBuf> {
        let norm = Self::normalized_receive_dir(self.receive_dir.as_deref());
        glide_backend::os_integration::resolve_receive_dir(norm.as_deref())
    }

    /// Returns a `ServerConfig` populated with top-level shared settings (`max_file_mb`, `receive_dir`).
    pub fn effective_server_config(&self) -> ServerConfig {
        let mut cfg = self.server.clone();
        cfg.max_file_bytes = Some(self.max_file_bytes());
        cfg.receive_dir = Self::normalized_receive_dir(self.receive_dir.as_deref());
        cfg
    }

    /// Returns a `ClientConfig` populated with top-level shared settings (`max_file_mb`, `receive_dir`).
    pub fn effective_client_config(&self) -> ClientConfig {
        let mut cfg = self.client.clone();
        cfg.max_file_bytes = Some(self.max_file_bytes());
        cfg.receive_dir = Self::normalized_receive_dir(self.receive_dir.as_deref());
        cfg
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse TOML config {}", path.display()))?;
        cfg.max_file_mb = cfg.max_file_mb_clamped();
        cfg.receive_dir = Self::normalized_receive_dir(cfg.receive_dir.as_deref());
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut normalized = self.clone();
        normalized.max_file_mb = normalized.max_file_mb_clamped();
        normalized.receive_dir = Self::normalized_receive_dir(normalized.receive_dir.as_deref());
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
    /// Keep media, volume, and brightness keys on this PC instead of forwarding them to remote clients.
    #[serde(default = "default_true")]
    pub pin_media_keys_local: bool,
    /// Enable bidirectional clipboard sharing (UTF-8 text & PNG/DIB images).
    pub clipboard: bool,
    /// Microseconds to coalesce high-polling-rate mouse deltas (1000 = 1000Hz max cadence; 0 = immediate).
    pub motion_coalesce_us: u64,
    /// Directory storing the server's Ed25519 TLS identity.
    pub identity_dir: Option<PathBuf>,
    /// Effective maximum file transfer size in bytes (populated from top-level `max_file_mb`).
    #[serde(skip)]
    pub max_file_bytes: Option<usize>,
    /// Effective custom receive folder (populated from top-level `receive_dir`).
    #[serde(skip)]
    pub receive_dir: Option<String>,
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
            pin_media_keys_local: true,
            clipboard: true,
            motion_coalesce_us: 1000,
            identity_dir: None,
            max_file_bytes: None,
            receive_dir: None,
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

    pub fn resolved_receive_dir(&self) -> Result<PathBuf> {
        let norm = GlideConfigFile::normalized_receive_dir(self.receive_dir.as_deref());
        glide_backend::os_integration::resolve_receive_dir(norm.as_deref())
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
    /// Effective custom receive folder (populated from top-level `receive_dir`).
    #[serde(skip)]
    pub receive_dir: Option<String>,
    /// Optional path to `known_servers.toml` for auto-recording connected server profiles.
    #[serde(skip)]
    pub known_servers_path: Option<PathBuf>,
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
            receive_dir: None,
            known_servers_path: None,
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

    pub fn resolved_receive_dir(&self) -> Result<PathBuf> {
        let norm = GlideConfigFile::normalized_receive_dir(self.receive_dir.as_deref());
        glide_backend::os_integration::resolve_receive_dir(norm.as_deref())
    }
}

/// Maximum number of saved servers retained in `known_servers.toml`.
pub const MAX_KNOWN_SERVERS: usize = 12;

/// Default path for the saved server profiles store (`<config_dir>/known_servers.toml`).
pub fn default_known_servers_path() -> PathBuf {
    default_config_dir().join("known_servers.toml")
}

/// Resolves `known_servers.toml` adjacent to a specific config file, falling back to default config dir.
pub fn known_servers_path_for_config(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("known_servers.toml"))
        .unwrap_or_else(default_known_servers_path)
}

fn default_right_edge() -> Edge {
    Edge::Right
}

/// Current Unix timestamp in seconds.
pub fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A previously connected Glide server saved for one-click profile switching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownServer {
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default = "default_right_edge")]
    pub edge: Edge,
    #[serde(default)]
    pub last_connected_unix: u64,
}

/// TOML-persisted list of recently connected Glide servers (`known_servers.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownServersStore {
    #[serde(default)]
    pub servers: Vec<KnownServer>,
}

impl KnownServersStore {
    /// Loads `known_servers.toml`, returning an empty store if the file does not exist yet.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read known servers store {}", path.display()))?;
        let mut store: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse known servers store {}", path.display()))?;
        store.normalize();
        Ok(store)
    }

    /// Saves `known_servers.toml` atomically (temp file in same directory + rename),
    /// sorted by most recently connected and capped at `MAX_KNOWN_SERVERS`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut normalized = self.clone();
        normalized.normalize();
        let body = toml::to_string_pretty(&normalized)?;
        write_file_atomic(path, &body)
            .with_context(|| format!("failed to write known servers store {}", path.display()))?;
        Ok(())
    }

    fn normalize(&mut self) {
        let existing = std::mem::take(&mut self.servers);
        for entry in existing {
            self.upsert_inner(entry);
        }
        self.sort_and_cap();
    }

    fn sort_and_cap(&mut self) {
        self.servers.sort_by(|a, b| {
            b.last_connected_unix
                .cmp(&a.last_connected_unix)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.address.cmp(&b.address))
        });
        self.servers.truncate(MAX_KNOWN_SERVERS);
    }

    fn upsert_inner(&mut self, mut entry: KnownServer) {
        entry.name = entry.name.trim().to_owned();
        entry.address = entry.address.trim().to_owned();
        if entry.address.is_empty() {
            return;
        }
        if entry.name.is_empty() {
            entry.name = entry.address.clone();
        }
        entry.fingerprint = entry
            .fingerprint
            .map(|fp| fp.trim().to_owned())
            .filter(|fp| !fp.is_empty());

        let entry_fp_norm = entry
            .fingerprint
            .as_deref()
            .map(normalize_known_fingerprint)
            .filter(|fp| !fp.is_empty());

        let mut preserved_fp: Option<String> = None;
        self.servers.retain(|existing| {
            let existing_fp_norm = existing
                .fingerprint
                .as_deref()
                .map(normalize_known_fingerprint)
                .filter(|fp| !fp.is_empty());

            // When both entries have fingerprints, deduplicate ONLY by normalized fingerprint so
            // two distinct servers on different LANs sharing the same RFC1918 IP never clobber
            // each other. Deduplicate by address only when at least one entry lacks a fingerprint.
            let should_replace = match (&entry_fp_norm, &existing_fp_norm) {
                (Some(a), Some(b)) => a == b,
                _ => existing.address.eq_ignore_ascii_case(&entry.address),
            };
            if should_replace {
                if entry.fingerprint.is_none() && preserved_fp.is_none() {
                    preserved_fp = existing.fingerprint.clone();
                }
                false
            } else {
                true
            }
        });
        if entry.fingerprint.is_none() {
            entry.fingerprint = preserved_fp;
        }
        self.servers.push(entry);
    }

    /// Inserts or updates a server entry (deduplicating by normalized fingerprint when both entries
    /// have fingerprints, or by address when at least one lacks a fingerprint), sorted by most
    /// recently connected and capped at `MAX_KNOWN_SERVERS`.
    pub fn upsert(&mut self, mut entry: KnownServer) {
        if entry.last_connected_unix == 0 {
            entry.last_connected_unix = current_unix_secs();
        }
        self.upsert_inner(entry);
        self.sort_and_cap();
    }

    /// Returns all saved servers sorted by most recently connected (capped at `MAX_KNOWN_SERVERS`).
    pub fn list(&self) -> Vec<KnownServer> {
        let mut out = self.clone();
        out.sort_and_cap();
        out.servers
    }

    /// Finds a saved server by exact case-insensitive name first, then address, then normalized fingerprint.
    pub fn find(&self, query: &str) -> Option<&KnownServer> {
        let q = query.trim();
        if q.is_empty() {
            return None;
        }
        let q_fp = normalize_known_fingerprint(q);
        self.servers
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(q))
            .or_else(|| {
                self.servers
                    .iter()
                    .find(|s| s.address.eq_ignore_ascii_case(q))
            })
            .or_else(|| {
                if q_fp.is_empty() {
                    None
                } else {
                    self.servers.iter().find(|s| {
                        s.fingerprint
                            .as_deref()
                            .is_some_and(|fp| normalize_known_fingerprint(fp) == q_fp)
                    })
                }
            })
    }

    /// Removes any saved server matching `address_or_name` (by normalized fingerprint, address, or name).
    pub fn remove(&mut self, address_or_name: &str) -> bool {
        let q = address_or_name.trim();
        if q.is_empty() {
            return false;
        }
        let q_fp = normalize_known_fingerprint(q);
        let before = self.servers.len();
        self.servers.retain(|s| {
            let fp_match = !q_fp.is_empty()
                && s.fingerprint
                    .as_deref()
                    .is_some_and(|fp| normalize_known_fingerprint(fp) == q_fp);
            !fp_match && !s.address.eq_ignore_ascii_case(q) && !s.name.eq_ignore_ascii_case(q)
        });
        self.servers.len() != before
    }
}

static ATOMIC_WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Atomically writes `content` to `path` via a unique temporary file in the same directory + rename.
fn write_file_atomic(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let seq = ATOMIC_WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let fname = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("glide");
    let tmp_path = parent.join(format!(".{fname}.tmp.{}.{seq}", std::process::id()));
    if let Err(e) = fs::write(&tmp_path, content) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e).with_context(|| format!("failed to write temp file {}", tmp_path.display()));
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e)
            .with_context(|| format!("failed to atomically replace {}", path.display()));
    }
    Ok(())
}

/// Normalizes a server fingerprint for comparison (strips optional `SHA256:` prefix, colons,
/// and whitespace, and lowercases ASCII characters).
pub fn normalize_known_fingerprint(fp: &str) -> String {
    let trimmed = fp.trim();
    let stripped = trimmed
        .strip_prefix("SHA256:")
        .or_else(|| trimmed.strip_prefix("sha256:"))
        .unwrap_or(trimmed);
    stripped
        .chars()
        .filter(|c| *c != ':' && !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Upserts a server profile into `known_servers.toml` at `path` and persists it atomically.
/// Parse or I/O errors are propagated rather than silently wiping existing saved profiles.
pub fn record_known_server(path: &Path, entry: KnownServer) -> Result<KnownServersStore> {
    let mut store = KnownServersStore::load(path)?;
    store.upsert(entry);
    store.save(path)?;
    Ok(store)
}

/// Lists saved servers from `known_servers.toml` at `path`, sorted by most recently connected.
pub fn list_known_servers(path: &Path) -> Result<Vec<KnownServer>> {
    Ok(KnownServersStore::load(path)?.list())
}

/// Forgets a saved server from `known_servers.toml` at `path` by address, name, or fingerprint.
pub fn remove_known_server(path: &Path, address_or_name: &str) -> Result<bool> {
    let mut store = KnownServersStore::load(path)?;
    let removed = store.remove(address_or_name);
    if removed {
        store.save(path)?;
    }
    Ok(removed)
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

    #[test]
    fn pin_media_keys_local_defaults_true_and_roundtrips() {
        let legacy: GlideConfigFile = toml::from_str(
            r#"
            [server]
            edge_threshold_px = 5
            "#,
        )
        .unwrap();
        assert!(legacy.server.pin_media_keys_local);
        assert!(ServerConfig::default().pin_media_keys_local);

        let mut cfg = GlideConfigFile::default();
        cfg.server.pin_media_keys_local = false;
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let parsed: GlideConfigFile = toml::from_str(&raw).unwrap();
        assert!(!parsed.server.pin_media_keys_local);
    }

    #[test]
    fn receive_dir_defaults_none_normalizes_and_roundtrips() {
        let legacy: GlideConfigFile = toml::from_str("").unwrap();
        assert_eq!(legacy.receive_dir, None);
        assert_eq!(legacy.effective_server_config().receive_dir, None);
        assert_eq!(legacy.effective_client_config().receive_dir, None);

        // Blank receive_dir normalizes to None on load/save
        let tmp = std::env::temp_dir().join(format!(
            "glide-cfg-receive-dir-{}.toml",
            std::process::id()
        ));
        let cfg_blank = GlideConfigFile {
            receive_dir: Some("   ".into()),
            ..GlideConfigFile::default()
        };
        cfg_blank.save(&tmp).unwrap();
        let reloaded_blank = GlideConfigFile::load(&tmp).unwrap();
        assert_eq!(reloaded_blank.receive_dir, None);

        // Non-empty custom receive_dir persists and propagates to effective server/client configs
        let cfg_custom = GlideConfigFile {
            receive_dir: Some("  ~/Inbox/GlideFiles  ".into()),
            ..GlideConfigFile::default()
        };
        cfg_custom.save(&tmp).unwrap();
        let reloaded = GlideConfigFile::load(&tmp).unwrap();
        assert_eq!(reloaded.receive_dir.as_deref(), Some("~/Inbox/GlideFiles"));
        assert_eq!(
            reloaded.effective_server_config().receive_dir.as_deref(),
            Some("~/Inbox/GlideFiles")
        );
        assert_eq!(
            reloaded.effective_client_config().receive_dir.as_deref(),
            Some("~/Inbox/GlideFiles")
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn known_servers_store_load_save_upsert_dedup_remove_and_cap() {
        let tmp = std::env::temp_dir().join(format!(
            "glide-known-servers-test-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);

        // Missing file loads cleanly as empty
        let empty = KnownServersStore::load(&tmp).unwrap();
        assert!(empty.servers.is_empty());

        // Record two servers and verify descending recency sort
        record_known_server(
            &tmp,
            KnownServer {
                name: "office-pc".into(),
                address: "192.168.1.10:24800".into(),
                fingerprint: Some("aa:bb:cc".into()),
                edge: Edge::Left,
                last_connected_unix: 1000,
            },
        )
        .unwrap();
        record_known_server(
            &tmp,
            KnownServer {
                name: "studio-win".into(),
                address: "192.168.1.20:24800".into(),
                fingerprint: Some("dd:ee:ff".into()),
                edge: Edge::Right,
                last_connected_unix: 2000,
            },
        )
        .unwrap();

        let listed = list_known_servers(&tmp).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "studio-win");
        assert_eq!(listed[1].name, "office-pc");

        // Upsert by same fingerprint updates IP/edge/timestamp without duplicating
        record_known_server(
            &tmp,
            KnownServer {
                name: "office-pc".into(),
                address: "192.168.1.99:24800".into(),
                fingerprint: Some("AA:BB:CC".into()),
                edge: Edge::Top,
                last_connected_unix: 3000,
            },
        )
        .unwrap();
        let after_fp_dedup = list_known_servers(&tmp).unwrap();
        assert_eq!(after_fp_dedup.len(), 2);
        assert_eq!(after_fp_dedup[0].name, "office-pc");
        assert_eq!(after_fp_dedup[0].address, "192.168.1.99:24800");
        assert_eq!(after_fp_dedup[0].edge, Edge::Top);

        // Upsert by same address without fingerprint preserves existing fingerprint
        record_known_server(
            &tmp,
            KnownServer {
                name: "studio-renamed".into(),
                address: "192.168.1.20:24800".into(),
                fingerprint: None,
                edge: Edge::Bottom,
                last_connected_unix: 4000,
            },
        )
        .unwrap();
        let store = KnownServersStore::load(&tmp).unwrap();
        assert_eq!(store.servers.len(), 2);
        let studio = store.find("studio-renamed").unwrap();
        assert_eq!(studio.fingerprint.as_deref(), Some("dd:ee:ff"));
        assert_eq!(studio.edge, Edge::Bottom);
        assert!(store.find("192.168.1.99:24800").is_some());

        // Cap at MAX_KNOWN_SERVERS keeping the most recent entries
        for i in 0..(MAX_KNOWN_SERVERS + 4) {
            record_known_server(
                &tmp,
                KnownServer {
                    name: format!("srv-{i}"),
                    address: format!("10.0.0.{}:24800", i + 1),
                    fingerprint: Some(format!("fp-{i:02}")),
                    edge: Edge::Right,
                    last_connected_unix: 5000 + i as u64,
                },
            )
            .unwrap();
        }
        let capped = list_known_servers(&tmp).unwrap();
        assert_eq!(capped.len(), MAX_KNOWN_SERVERS);
        assert_eq!(capped[0].name, format!("srv-{}", MAX_KNOWN_SERVERS + 3));

        // Remove by name and by address
        assert!(remove_known_server(&tmp, &format!("srv-{}", MAX_KNOWN_SERVERS + 3)).unwrap());
        assert!(!remove_known_server(&tmp, "does-not-exist").unwrap());
        assert_eq!(list_known_servers(&tmp).unwrap().len(), MAX_KNOWN_SERVERS - 1);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn known_servers_same_ip_distinct_fingerprints_do_not_clobber_each_other() {
        let mut store = KnownServersStore::default();
        store.upsert(KnownServer {
            name: "home-desktop".into(),
            address: "192.168.1.10:24800".into(),
            fingerprint: Some("aa:bb:cc:dd".into()),
            edge: Edge::Right,
            last_connected_unix: 1000,
        });
        store.upsert(KnownServer {
            name: "office-desktop".into(),
            address: "192.168.1.10:24800".into(),
            fingerprint: Some("11:22:33:44".into()),
            edge: Edge::Left,
            last_connected_unix: 2000,
        });

        // Both servers sharing 192.168.1.10:24800 must coexist!
        assert_eq!(store.servers.len(), 2);
        assert_eq!(store.servers[0].name, "office-desktop");
        assert_eq!(store.servers[1].name, "home-desktop");

        // Normalized fingerprint update (no colons, uppercase, SHA256: prefix) deduplicates home-desktop only
        store.upsert(KnownServer {
            name: "home-desktop-renamed".into(),
            address: "192.168.1.10:24800".into(),
            fingerprint: Some("SHA256:AABBCCDD".into()),
            edge: Edge::Top,
            last_connected_unix: 3000,
        });
        assert_eq!(store.servers.len(), 2);
        assert_eq!(store.servers[0].name, "home-desktop-renamed");
        assert_eq!(store.servers[1].name, "office-desktop");

        // Removing by fingerprint removes ONLY that entry, leaving the other server on the same IP intact
        assert!(store.remove("11:22:33:44"));
        assert_eq!(store.servers.len(), 1);
        assert_eq!(store.servers[0].name, "home-desktop-renamed");
    }

    #[test]
    fn record_known_server_propagates_corrupt_toml_without_clobbering_file() {
        let tmp = std::env::temp_dir().join(format!(
            "glide-known-corrupt-test-{}.toml",
            std::process::id()
        ));
        let corrupt_body = "[[servers]\nname = \"broken\n";
        fs::write(&tmp, corrupt_body).unwrap();

        let err = record_known_server(
            &tmp,
            KnownServer {
                name: "new-pc".into(),
                address: "192.168.1.50:24800".into(),
                fingerprint: Some("ab:cd".into()),
                edge: Edge::Right,
                last_connected_unix: 500,
            },
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("failed to parse known servers store"),
            "unexpected error: {err:#}"
        );
        // Existing corrupt file must NOT be wiped or overwritten!
        assert_eq!(fs::read_to_string(&tmp).unwrap(), corrupt_body);
        let _ = fs::remove_file(&tmp);
    }
}
