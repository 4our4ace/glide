//! GitHub Releases update checker & cryptographically verified self-updater.
//!
//! Supports two release channels:
//! - **Stable** (`UpdateChannel::Stable`): official versioned releases (`/releases/latest`).
//!   Never contacts GitHub automatically — only checks when the user clicks "Check" or switches
//!   channels.
//! - **Nightly** (`UpdateChannel::Nightly`): rolling development builds from the `nightly` branch
//!   (`/releases/tags/nightly`). Automatically checks once on launch and tracks exact 7-char git
//!   commit SHAs so Nightly-to-Nightly updates and Nightly↔Stable switching both work in one click.
//!
//! Security guarantees:
//! - API and asset URLs are strictly pinned to `https://github.com/4our4ace/glide/...` with
//!   `https_only(true)` enforced across redirects.
//! - Every downloaded binary or `.deb` package MUST match `SHA256SUMS.txt` from the same release
//!   using constant-time digest comparison before any install step runs.
//! - Downloads are streamed directly to exclusively created (`create_new(true)`) staging files with
//!   a hard 128 MiB ceiling, and cleaned up on any failure.

use glide_core::config::{UpdateChannel, BUILD_SHA};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, info, warn};

pub const REPO_URL_PREFIX: &str = "https://github.com/4our4ace/glide/";
pub const ASSET_URL_PREFIX: &str = "https://github.com/4our4ace/glide/releases/download/";
pub const RELEASES_PAGE_URL: &str = "https://github.com/4our4ace/glide/releases";
pub const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/4our4ace/glide/releases/latest";
pub const NIGHTLY_RELEASE_API_URL: &str =
    "https://api.github.com/repos/4our4ace/glide/releases/tags/nightly";

const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";
const WINDOWS_BINARY_ASSET: &str = "glide-windows-x86_64-standalone.exe";
const WINDOWS_BINARY_LEGACY_ASSET: &str = "glide-windows-x86_64.exe";
const WINDOWS_MSI_ASSET: &str = "glide-windows-x86_64-installer.msi";
const WINDOWS_MSI_LEGACY_ASSET: &str = "glide-windows-x86_64.msi";
const LINUX_BINARY_ASSET: &str = "glide-linux-x86_64-standalone";
const LINUX_BINARY_LEGACY_ASSET: &str = "glide-linux-x86_64";
const MAX_CHECKSUMS_BYTES: usize = 64 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const RELAUNCH_WAIT_PID_ENV: &str = "GLIDE_RELAUNCH_WAIT_PID";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAsset {
    pub name: String,
    pub download_url: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub channel: UpdateChannel,
    /// Human-readable target label (e.g. `"v0.4.0"` or `"nightly (7b68d47)"`).
    pub version: String,
    /// Compact label for status badges (e.g. `"v0.4.0"` or `"7b68d47"`).
    pub short_label: String,
    /// Verified GitHub Releases HTML URL for manual fallback.
    pub html_url: String,
    /// True when switching between Stable and Nightly rather than updating within the same track.
    pub is_channel_switch: bool,
    /// Standalone executable asset for the current OS (`glide-windows-x86_64-standalone.exe` / `glide-linux-x86_64-standalone`).
    pub binary_asset: Option<UpdateAsset>,
    /// Windows `.msi` installer asset (`glide-windows-x86_64-installer.msi`).
    pub msi_asset: Option<UpdateAsset>,
    /// Debian/Ubuntu `.deb` package asset (Linux only, used when running from `/usr/bin/glide`).
    pub deb_asset: Option<UpdateAsset>,
    /// Verified download URL for `SHA256SUMS.txt` from this release (mandatory for self-update).
    pub checksums_url: Option<String>,
}

impl AvailableUpdate {
    /// Returns `true` when this release carries a checksum manifest and a matching installable asset
    /// for the current platform.
    pub fn can_self_update(&self) -> bool {
        if self.checksums_url.is_none() {
            return false;
        }
        #[cfg(target_os = "windows")]
        {
            self.binary_asset.is_some() || self.msi_asset.is_some()
        }
        #[cfg(target_os = "linux")]
        {
            self.binary_asset.is_some() || self.deb_asset.is_some()
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateInstallOutcome {
    /// Update was installed in place; relaunch this executable cleanly after shutdown.
    InstalledRelaunch(PathBuf),
    /// An external OS installer (e.g. Windows `msiexec`) was launched out-of-process;
    /// quit Glide cleanly so the installer can finish without relaunching directly.
    ExternalInstallerLaunched,
    /// Linux `.deb` was downloaded and SHA-256 verified inside a private `0700` staging
    /// directory and is waiting for the current user's `sudo` password in the GUI modal.
    NeedsSudoPassword {
        deb_path: PathBuf,
        stage_dir: PathBuf,
        username: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SudoInstallError {
    /// `sudo` rejected the password; the verified `.deb` remains staged for immediate retry.
    WrongPassword(String),
    /// Non-password error (`not in sudoers`, `dpkg` error, etc.); staging directory cleaned up.
    Fatal(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdateCheckState {
    #[default]
    Idle,
    Checking,
    UpToDate {
        latest: String,
    },
    Available(Box<AvailableUpdate>),
    Downloading {
        version: String,
        progress_pct: u8,
    },
    Installing {
        version: String,
    },
    AwaitingSudoPassword {
        version: String,
        deb_path: PathBuf,
        stage_dir: PathBuf,
        username: String,
        error: Option<String>,
    },
    Error(String),
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    target_commitish: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

/// Parses a `v0.3.0` or `0.3.0-rc1` tag into `(major, minor, patch)`.
pub fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    let core = trimmed.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Returns `true` if `latest` is strictly newer than `current`.
pub fn is_newer_version(current: &str, latest: &str) -> bool {
    match (parse_semver(current), parse_semver(latest)) {
        (Some(cur), Some(lat)) => lat > cur,
        _ => false,
    }
}

/// Validates that a release URL points strictly inside the official Glide GitHub repository.
pub fn validate_release_url(url: &str) -> Result<(), String> {
    if !url.starts_with(REPO_URL_PREFIX) {
        return Err("Refusing to open URL outside the Glide GitHub repository".into());
    }
    glide_backend::os_integration::validate_https_url(url).map_err(|e| e.to_string())?;
    let path = &url[REPO_URL_PREFIX.len()..];
    if path.split(['/', '?', '#']).any(|seg| seg == "..") {
        return Err("Refusing to open URL containing path traversal".into());
    }
    Ok(())
}

/// Validates that an asset download URL points strictly to a Glide GitHub Release download.
pub fn validate_asset_download_url(url: &str) -> Result<(), String> {
    if !url.starts_with(ASSET_URL_PREFIX) {
        return Err("Refusing to download update asset outside Glide GitHub Releases".into());
    }
    glide_backend::os_integration::validate_https_url(url).map_err(|e| e.to_string())?;
    let path = &url[ASSET_URL_PREFIX.len()..];
    if path.split(['/', '?', '#']).any(|seg| seg == "..") {
        return Err("Refusing to download asset URL containing path traversal".into());
    }
    Ok(())
}

/// Restricts release links returned by the GitHub API to the official Glide repository.
pub fn sanitize_release_url(html_url: Option<&str>) -> String {
    html_url
        .filter(|u| validate_release_url(u).is_ok())
        .map(str::to_owned)
        .unwrap_or_else(|| RELEASES_PAGE_URL.to_owned())
}

fn sanitize_hex_sha(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !(7..=40).contains(&trimmed.len()) || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(trimmed[..7].to_ascii_lowercase())
}

/// Extracts a 7-character lowercase git commit SHA from a nightly GitHub release's title, body,
/// or `target_commitish` (e.g. `"Glide Nightly (7b68d47)"` -> `Some("7b68d47")`).
fn extract_nightly_sha(release: &GithubRelease) -> Option<String> {
    if let Some(ref title) = release.name {
        if let Some(start) = title.rfind('(') {
            if let Some(end) = title[start + 1..].find(')') {
                let inside = &title[start + 1..start + 1 + end];
                if let Some(sha) = sanitize_hex_sha(inside) {
                    return Some(sha);
                }
            }
        }
    }
    if let Some(ref body) = release.body {
        for line in body.lines() {
            let trimmed = line.trim();
            for prefix in ["Commit:", "commit:", "Build commit:", "SHA:", "sha:"] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let token = rest.trim().trim_matches('`').split_whitespace().next()?;
                    if let Some(sha) = sanitize_hex_sha(token) {
                        return Some(sha);
                    }
                }
            }
        }
    }
    if let Some(ref commitish) = release.target_commitish {
        if let Some(sha) = sanitize_hex_sha(commitish) {
            return Some(sha);
        }
    }
    None
}

struct PlatformAssets {
    binary_asset: Option<UpdateAsset>,
    msi_asset: Option<UpdateAsset>,
    deb_asset: Option<UpdateAsset>,
    checksums_url: Option<String>,
}

fn select_assets_for_platform(assets: &[GithubAsset], want_windows: bool) -> PlatformAssets {
    let mut binary_asset = None;
    let mut legacy_binary_asset = None;
    let mut msi_asset = None;
    let mut legacy_msi_asset = None;
    let mut deb_asset = None;
    let mut checksums_url = None;

    for a in assets {
        if validate_asset_download_url(&a.browser_download_url).is_err() {
            continue;
        }
        if a.name == CHECKSUMS_ASSET_NAME {
            checksums_url = Some(a.browser_download_url.clone());
            continue;
        }
        let make_asset = || UpdateAsset {
            name: a.name.clone(),
            download_url: a.browser_download_url.clone(),
            size: a.size,
        };
        if want_windows {
            if a.name == WINDOWS_BINARY_ASSET {
                binary_asset = Some(make_asset());
            } else if a.name == WINDOWS_BINARY_LEGACY_ASSET {
                legacy_binary_asset = Some(make_asset());
            } else if a.name == WINDOWS_MSI_ASSET {
                msi_asset = Some(make_asset());
            } else if a.name == WINDOWS_MSI_LEGACY_ASSET {
                legacy_msi_asset = Some(make_asset());
            }
        } else if a.name == LINUX_BINARY_ASSET {
            binary_asset = Some(make_asset());
        } else if a.name == LINUX_BINARY_LEGACY_ASSET {
            legacy_binary_asset = Some(make_asset());
        } else if a.name.starts_with("glide_") && a.name.ends_with("_amd64.deb") {
            deb_asset = Some(make_asset());
        }
    }

    PlatformAssets {
        binary_asset: binary_asset.or(legacy_binary_asset),
        msi_asset: msi_asset.or(legacy_msi_asset),
        deb_asset,
        checksums_url,
    }
}

fn evaluate_release(
    selected_channel: UpdateChannel,
    current_channel: UpdateChannel,
    current_version: &str,
    current_sha: &str,
    release: &GithubRelease,
    want_windows: bool,
) -> UpdateCheckState {
    let html_url = sanitize_release_url(release.html_url.as_deref());
    let PlatformAssets {
        binary_asset,
        msi_asset,
        deb_asset,
        checksums_url,
    } = select_assets_for_platform(&release.assets, want_windows);
    let is_channel_switch = selected_channel != current_channel;

    match selected_channel {
        UpdateChannel::Stable => {
            let latest_tag = release.tag_name.trim().to_owned();
            let display_tag = if latest_tag.starts_with(['v', 'V']) {
                latest_tag.clone()
            } else {
                format!("v{latest_tag}")
            };

            if is_channel_switch || is_newer_version(current_version, &latest_tag) {
                UpdateCheckState::Available(Box::new(AvailableUpdate {
                    channel: UpdateChannel::Stable,
                    version: display_tag.clone(),
                    short_label: display_tag,
                    html_url,
                    is_channel_switch,
                    binary_asset,
                    msi_asset,
                    deb_asset,
                    checksums_url,
                }))
            } else {
                UpdateCheckState::UpToDate {
                    latest: display_tag,
                }
            }
        }
        UpdateChannel::Nightly => {
            let remote_sha = extract_nightly_sha(release);
            let (version, short_label) = match &remote_sha {
                Some(sha) => (format!("nightly ({sha})"), format!("nightly {sha}")),
                None => ("nightly".to_string(), "nightly".to_string()),
            };

            let same_nightly_commit = !is_channel_switch
                && current_sha != "unknown"
                && remote_sha
                    .as_deref()
                    .is_some_and(|r| r.eq_ignore_ascii_case(current_sha));

            if same_nightly_commit {
                UpdateCheckState::UpToDate { latest: version }
            } else {
                UpdateCheckState::Available(Box::new(AvailableUpdate {
                    channel: UpdateChannel::Nightly,
                    version,
                    short_label,
                    html_url,
                    is_channel_switch,
                    binary_asset,
                    msi_asset,
                    deb_asset,
                    checksums_url,
                }))
            }
        }
    }
}

/// Checks GitHub Releases for the requested `channel` (`Stable` or `Nightly`).
pub fn check_release(channel: UpdateChannel, current_version: &str) -> UpdateCheckState {
    let api_url = match channel {
        UpdateChannel::Stable => LATEST_RELEASE_API_URL,
        UpdateChannel::Nightly => NIGHTLY_RELEASE_API_URL,
    };

    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .new_agent();

    let resp = agent
        .get(api_url)
        .header("User-Agent", &format!("Glide/{current_version}"))
        .header("Accept", "application/vnd.github+json")
        .call();

    let mut resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => {
            return match channel {
                UpdateChannel::Stable => UpdateCheckState::UpToDate {
                    latest: format!("v{}", current_version.trim_start_matches(['v', 'V'])),
                },
                UpdateChannel::Nightly => {
                    UpdateCheckState::Error("No nightly build published yet".into())
                }
            };
        }
        Err(ureq::Error::StatusCode(403 | 429)) => {
            return UpdateCheckState::Error("GitHub rate limit reached; try again shortly".into());
        }
        Err(_) => {
            return UpdateCheckState::Error("Couldn't reach GitHub (offline?)".into());
        }
    };

    let release: GithubRelease = match resp.body_mut().read_json() {
        Ok(r) => r,
        Err(_) => {
            return UpdateCheckState::Error("Unexpected response from GitHub".into());
        }
    };

    evaluate_release(
        channel,
        UpdateChannel::build_channel(),
        current_version,
        BUILD_SHA,
        &release,
        cfg!(target_os = "windows"),
    )
}

/// Backwards-compatible wrapper checking the Stable channel.
#[allow(dead_code)]
pub fn check_latest_release(current_version: &str) -> UpdateCheckState {
    check_release(UpdateChannel::Stable, current_version)
}

/// Parses a GNU `sha256sum` manifest and returns the 32-byte digest for `asset_name`.
pub fn parse_sha256_for_asset(manifest: &str, asset_name: &str) -> Option<[u8; 32]> {
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let hex_part = parts.next()?.trim();
        let file_part = parts.next()?.trim().trim_start_matches('*');
        let normalized_name = file_part.strip_prefix("./").unwrap_or(file_part);
        if normalized_name == asset_name {
            return decode_sha256_hex(hex_part);
        }
    }
    None
}

fn decode_sha256_hex(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn ct_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn fetch_expected_sha256(
    agent: &ureq::Agent,
    checksums_url: &str,
    asset_name: &str,
    current_version: &str,
) -> Result<[u8; 32], String> {
    validate_asset_download_url(checksums_url)?;
    let resp = agent
        .get(checksums_url)
        .header("User-Agent", &format!("Glide/{current_version}"))
        .call()
        .map_err(|e| format!("Failed downloading SHA256SUMS.txt: {e}"))?;

    let mut manifest_bytes = Vec::new();
    resp.into_body()
        .into_reader()
        .take((MAX_CHECKSUMS_BYTES + 1) as u64)
        .read_to_end(&mut manifest_bytes)
        .map_err(|e| format!("Failed reading SHA256SUMS.txt: {e}"))?;
    if manifest_bytes.len() > MAX_CHECKSUMS_BYTES {
        return Err("SHA256SUMS.txt exceeded maximum allowed size".into());
    }
    let manifest = String::from_utf8(manifest_bytes)
        .map_err(|_| "SHA256SUMS.txt was not valid UTF-8".to_string())?;
    parse_sha256_for_asset(&manifest, asset_name)
        .ok_or_else(|| format!("SHA256SUMS.txt has no checksum entry for {asset_name}"))
}

fn download_verified_file<F>(
    agent: &ureq::Agent,
    asset: &UpdateAsset,
    expected_sha: &[u8; 32],
    dest_path: &Path,
    current_version: &str,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(u8),
{
    validate_asset_download_url(&asset.download_url)?;
    if asset.size > MAX_BINARY_BYTES {
        return Err("Update asset exceeds maximum allowed size".into());
    }

    let resp = agent
        .get(&asset.download_url)
        .header("User-Agent", &format!("Glide/{current_version}"))
        .call()
        .map_err(|e| format!("Failed downloading {}: {e}", asset.name))?;

    let total_size = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(asset.size);
    if total_size > MAX_BINARY_BYTES {
        return Err("Update download exceeded 128 MiB limit".into());
    }

    let _ = fs::remove_file(dest_path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest_path)
        .map_err(|e| format!("Failed creating staging file: {e}"))?;

    let mut reader = resp.into_body().into_reader();
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_pct: u8 = 0;
    on_progress(0);

    let copy_res: Result<(), String> = (|| {
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("Download interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            downloaded = downloaded.saturating_add(n as u64);
            if downloaded > MAX_BINARY_BYTES {
                return Err("Update download exceeded 128 MiB limit".into());
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])
                .map_err(|e| format!("Failed writing update file: {e}"))?;
            if let Some(raw_pct) = downloaded.saturating_mul(100).checked_div(total_size) {
                let pct = raw_pct.min(100) as u8;
                if pct != last_pct {
                    last_pct = pct;
                    on_progress(pct);
                }
            }
        }
        file.flush()
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("Failed flushing update file: {e}"))?;
        Ok(())
    })();

    drop(file);
    if let Err(e) = copy_res {
        let _ = fs::remove_file(dest_path);
        return Err(e);
    }

    let digest: [u8; 32] = hasher.finalize().into();
    if !ct_eq_32(&digest, expected_sha) {
        let _ = fs::remove_file(dest_path);
        return Err("Update checksum verification failed (SHA-256 mismatch)".into());
    }
    on_progress(100);
    Ok(())
}

fn append_os_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn is_dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".glide-update-probe-{}", std::process::id()));
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(f) => {
            drop(f);
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

/// Downloads, SHA-256 verifies, and installs `update` (or stages a verified `.deb` awaiting the
/// current user's `sudo` password in GUI mode).
///
/// `on_progress` receives download percentage (`0..=100`). `on_installing` fires right after the
/// checksum passes and before the OS install/swap runs.
pub fn download_and_install_update<Fp, Fi>(
    update: &AvailableUpdate,
    current_version: &str,
    allow_interactive_sudo: bool,
    mut on_progress: Fp,
    mut on_installing: Fi,
) -> Result<UpdateInstallOutcome, String>
where
    Fp: FnMut(u8),
    Fi: FnMut(),
{
    let checksums_url = update
        .checksums_url
        .as_deref()
        .ok_or_else(|| "Release is missing SHA256SUMS.txt; refusing unverified update".to_string())?;

    let current_exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map(|p| strip_windows_verbatim_prefix(&p))
        .map_err(|e| format!("Failed locating current Glide executable: {e}"))?;

    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .new_agent();

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;

        let _ = allow_interactive_sudo;
        let parent = current_exe
            .parent()
            .ok_or_else(|| "Current executable has no parent directory".to_string())?;

        // Writable install directory (per-user MSI in %LOCALAPPDATA%\Programs\Glide\ or portable .exe):
        // fast, zero-UAC atomic in-place swap followed by clean shutdown, relaunch, and .old cleanup.
        if is_dir_writable(parent) {
            if let Some(asset) = update.binary_asset.as_ref() {
                let expected_sha =
                    fetch_expected_sha256(&agent, checksums_url, &asset.name, current_version)?;
                let staged = append_os_suffix(&current_exe, &format!(".new.{}", std::process::id()));
                download_verified_file(
                    &agent,
                    asset,
                    &expected_sha,
                    &staged,
                    current_version,
                    &mut on_progress,
                )?;

                on_installing();
                let backup = append_os_suffix(&current_exe, ".old");
                let _ = fs::remove_file(&backup);

                // Windows permits renaming a running .exe on the same volume, then moving the verified
                // replacement directly into its original path so Start Menu shortcuts, taskbar pins,
                // and autostart stay intact.
                fs::rename(&current_exe, &backup).map_err(|e| {
                    let _ = fs::remove_file(&staged);
                    format!("Cannot replace {}: {e}", current_exe.display())
                })?;

                if let Err(e) = fs::rename(&staged, &current_exe) {
                    let _ = fs::rename(&backup, &current_exe);
                    let _ = fs::remove_file(&staged);
                    return Err(format!("Failed moving verified update into place: {e}"));
                }

                info!(
                    version = %update.version,
                    exe = %current_exe.display(),
                    "Installed verified Glide update on Windows"
                );
                return Ok(UpdateInstallOutcome::InstalledRelaunch(current_exe));
            }
        }

        // Protected directory (e.g. C:\Program Files\Glide) or MSI-only release: download & verify
        // the MSI package into a temp staging dir and launch Windows Installer (`msiexec /i ... /passive`).
        let msi_asset = update.msi_asset.as_ref().ok_or_else(|| {
            format!(
                "Cannot write to {} and release has no Windows .msi installer",
                parent.display()
            )
        })?;
        let expected_sha =
            fetch_expected_sha256(&agent, checksums_url, &msi_asset.name, current_version)?;
        let stage_dir = std::env::temp_dir().join(format!("glide-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&stage_dir);
        fs::create_dir_all(&stage_dir)
            .map_err(|e| format!("Failed creating update staging directory: {e}"))?;
        let msi_path = stage_dir.join(&msi_asset.name);

        if let Err(e) = download_verified_file(
            &agent,
            msi_asset,
            &expected_sha,
            &msi_path,
            current_version,
            &mut on_progress,
        ) {
            let _ = fs::remove_dir_all(&stage_dir);
            return Err(e);
        }

        on_installing();
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        std::process::Command::new("msiexec.exe")
            .arg("/i")
            .arg(&msi_path)
            .arg("/passive")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|e| {
                let _ = fs::remove_dir_all(&stage_dir);
                format!("Failed launching msiexec.exe: {e}")
            })?;

        info!(
            version = %update.version,
            msi = %msi_path.display(),
            "Launched verified Windows MSI installer for Glide update"
        );
        Ok(UpdateInstallOutcome::ExternalInstallerLaunched)
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        use std::process::Stdio;

        let parent = current_exe
            .parent()
            .ok_or_else(|| "Current executable has no parent directory".to_string())?;

        if is_dir_writable(parent) {
            let asset = update
                .binary_asset
                .as_ref()
                .ok_or_else(|| "Release does not include a Linux binary".to_string())?;
            let expected_sha =
                fetch_expected_sha256(&agent, checksums_url, &asset.name, current_version)?;
            let staged = parent.join(format!(".glide-update-{}.new", std::process::id()));
            download_verified_file(
                &agent,
                asset,
                &expected_sha,
                &staged,
                current_version,
                &mut on_progress,
            )?;

            on_installing();
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).map_err(|e| {
                let _ = fs::remove_file(&staged);
                format!("Failed setting executable permission: {e}")
            })?;
            fs::rename(&staged, &current_exe).map_err(|e| {
                let _ = fs::remove_file(&staged);
                format!("Failed replacing {}: {e}", current_exe.display())
            })?;

            info!(
                version = %update.version,
                exe = %current_exe.display(),
                "Installed verified Glide standalone update on Linux"
            );
            return Ok(UpdateInstallOutcome::InstalledRelaunch(current_exe));
        }

        // System-wide install (e.g. /usr/bin/glide from .deb): download + verify the .deb into a
        // private 0700 temp directory and install via `sudo` (authenticating as the current user,
        // NOT root via Polkit/pkexec which fails on Debian and corporate laptops without a root
        // password or local polkit admin group membership).
        let deb_asset = update.deb_asset.as_ref().ok_or_else(|| {
            format!(
                "{} is root-owned and release has no .deb package for automatic install",
                current_exe.display()
            )
        })?;
        let expected_sha =
            fetch_expected_sha256(&agent, checksums_url, &deb_asset.name, current_version)?;

        let stage_dir = std::env::temp_dir().join(format!("glide-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&stage_dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&stage_dir)
            .map_err(|e| format!("Failed creating update staging directory: {e}"))?;
        let deb_path = stage_dir.join(&deb_asset.name);

        if let Err(e) = download_verified_file(
            &agent,
            deb_asset,
            &expected_sha,
            &deb_path,
            current_version,
            &mut on_progress,
        ) {
            let _ = fs::remove_dir_all(&stage_dir);
            return Err(e);
        }

        if command_exists("sudo") {
            // 1. If passwordless or cached `sudo` is already active, install silently right away.
            let cached_ok = std::process::Command::new("sudo")
                .arg("-n")
                .arg("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

            if cached_ok {
                on_installing();
                let status = std::process::Command::new("sudo")
                    .arg("-n")
                    .arg("--")
                    .arg("dpkg")
                    .arg("-i")
                    .arg(&deb_path)
                    .stdin(Stdio::null())
                    .status()
                    .map_err(|e| {
                        let _ = fs::remove_dir_all(&stage_dir);
                        format!("Failed launching sudo dpkg -i: {e}")
                    })?;
                let _ = fs::remove_dir_all(&stage_dir);
                if !status.success() {
                    return Err(format!(
                        "Package install failed (exit {})",
                        status.code().unwrap_or(-1)
                    ));
                }
                let relaunch = linux_deb_relaunch_path(&current_exe);
                info!(
                    version = %update.version,
                    exe = %relaunch.display(),
                    "Installed verified Glide .deb update via cached sudo"
                );
                return Ok(UpdateInstallOutcome::InstalledRelaunch(relaunch));
            }

            // 2. CLI mode (attached terminal): prompt directly via interactive `sudo`.
            if allow_interactive_sudo {
                on_installing();
                let status = std::process::Command::new("sudo")
                    .arg("--")
                    .arg("dpkg")
                    .arg("-i")
                    .arg(&deb_path)
                    .status()
                    .map_err(|e| {
                        let _ = fs::remove_dir_all(&stage_dir);
                        format!("Failed launching sudo dpkg -i: {e}")
                    })?;
                let _ = fs::remove_dir_all(&stage_dir);
                if !status.success() {
                    return Err(format!(
                        "Package install cancelled or failed (exit {})",
                        status.code().unwrap_or(-1)
                    ));
                }
                let relaunch = linux_deb_relaunch_path(&current_exe);
                info!(
                    version = %update.version,
                    exe = %relaunch.display(),
                    "Installed verified Glide .deb update via interactive sudo"
                );
                return Ok(UpdateInstallOutcome::InstalledRelaunch(relaunch));
            }

            // 3. GUI mode: prompt for the current user's `sudo` password inside Glide's secure modal.
            return Ok(UpdateInstallOutcome::NeedsSudoPassword {
                deb_path,
                stage_dir,
                username: current_unix_username(),
            });
        }

        // Only fall back to pkexec when `sudo` is not installed on the system at all.
        if !command_exists("pkexec") {
            let _ = fs::remove_dir_all(&stage_dir);
            return Err(
                "Neither sudo nor pkexec is available; download and install the .deb manually"
                    .into(),
            );
        }

        on_installing();
        let status = std::process::Command::new("pkexec")
            .arg("dpkg")
            .arg("-i")
            .arg(&deb_path)
            .status()
            .map_err(|e| {
                let _ = fs::remove_dir_all(&stage_dir);
                format!("Failed launching pkexec dpkg -i: {e}")
            })?;

        let _ = fs::remove_dir_all(&stage_dir);
        if !status.success() {
            return Err(format!(
                "Package install cancelled or failed (exit {})",
                status.code().unwrap_or(-1)
            ));
        }

        let relaunch = linux_deb_relaunch_path(&current_exe);
        info!(
            version = %update.version,
            exe = %relaunch.display(),
            "Installed verified Glide .deb update on Linux"
        );
        Ok(UpdateInstallOutcome::InstalledRelaunch(relaunch))
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (
            checksums_url,
            current_exe,
            agent,
            allow_interactive_sudo,
            &mut on_progress,
            &mut on_installing,
        );
        Err("Self-update is only supported on Windows and Linux".into())
    }
}

/// Overwrites a secret string's buffer with zeroes and clears its length.
pub fn zeroize_string(secret: &mut String) {
    // SAFETY: Writing zero bytes over the UTF-8 buffer and then calling `clear()` leaves `secret`
    // as a valid empty UTF-8 string while preventing the password from lingering in heap memory.
    unsafe {
        let bytes = secret.as_bytes_mut();
        for b in bytes.iter_mut() {
            std::ptr::write_volatile(b, 0);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    secret.clear();
}

/// Safely removes a Glide update staging directory created under the system temp directory.
pub fn cleanup_staged_dir(stage_dir: &Path) {
    let temp = std::env::temp_dir();
    let is_glide_stage = stage_dir.parent() == Some(temp.as_path())
        && stage_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with("glide-update-"));
    if is_glide_stage {
        let _ = fs::remove_dir_all(stage_dir);
    }
}

#[cfg(target_os = "linux")]
fn linux_deb_relaunch_path(fallback: &Path) -> PathBuf {
    if Path::new("/usr/bin/glide").exists() {
        PathBuf::from("/usr/bin/glide")
    } else {
        fallback.to_path_buf()
    }
}

#[cfg(target_os = "linux")]
fn current_unix_username() -> String {
    if let Ok(user) = std::env::var("USER") {
        let trimmed = user.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Ok(out) = std::process::Command::new("id").arg("-un").output() {
        if out.status.success() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if !name.is_empty() {
                return name;
            }
        }
    }
    "your user".to_owned()
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn classify_sudo_failure(stderr: &str, exit_code: i32) -> SudoInstallError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("incorrect password")
        || lower.contains("sorry, try again")
        || lower.contains("authentication failure")
        || lower.contains("no password was provided")
    {
        return SudoInstallError::WrongPassword(
            "Incorrect password — please try again.".to_owned(),
        );
    }
    if lower.contains("not in the sudoers") || lower.contains("may not run sudo") {
        return SudoInstallError::Fatal(
            "Your user does not have sudo permission to install packages.".to_owned(),
        );
    }
    let detail = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if detail.is_empty() {
        SudoInstallError::Fatal(format!("Package install failed (exit {exit_code})"))
    } else {
        let truncated: String = detail.chars().take(180).collect();
        SudoInstallError::Fatal(format!("Package install failed: {truncated}"))
    }
}

/// Installs an already downloaded & SHA-256 verified `.deb` package using the current user's
/// `sudo` password (piped once via `sudo -S -k -p ""`, never passed on argv/env or saved to disk).
///
/// Always wipes `password` in memory before returning. On `SudoInstallError::WrongPassword`,
/// keeps `stage_dir` intact so the user can retry without re-downloading.
pub fn install_staged_deb_with_sudo(
    deb_path: &Path,
    stage_dir: &Path,
    password: &mut String,
) -> Result<PathBuf, SudoInstallError> {
    #[cfg(target_os = "linux")]
    {
        use std::process::Stdio;

        if !deb_path.is_file() || deb_path.parent() != Some(stage_dir) {
            zeroize_string(password);
            cleanup_staged_dir(stage_dir);
            return Err(SudoInstallError::Fatal(
                "Staged .deb update file is missing; please retry the update.".into(),
            ));
        }

        let mut child = match std::process::Command::new("sudo")
            .arg("-S")
            .arg("-k")
            .arg("-p")
            .arg("")
            .arg("--")
            .arg("dpkg")
            .arg("-i")
            .arg(deb_path)
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                zeroize_string(password);
                cleanup_staged_dir(stage_dir);
                return Err(SudoInstallError::Fatal(format!(
                    "Failed launching sudo dpkg -i: {e}"
                )));
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(password.as_bytes());
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
            // Explicitly drop stdin so sudo sees immediate EOF if authentication fails.
            drop(stdin);
        }
        zeroize_string(password);

        let output = match child.wait_with_output() {
            Ok(out) => out,
            Err(e) => {
                cleanup_staged_dir(stage_dir);
                return Err(SudoInstallError::Fatal(format!(
                    "Failed waiting for sudo dpkg -i: {e}"
                )));
            }
        };

        if output.status.success() {
            cleanup_staged_dir(stage_dir);
            let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/bin/glide"));
            let relaunch = linux_deb_relaunch_path(&current_exe);
            info!(
                exe = %relaunch.display(),
                "Installed verified Glide .deb update via user sudo"
            );
            return Ok(relaunch);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let err = classify_sudo_failure(&stderr, output.status.code().unwrap_or(-1));
        if matches!(err, SudoInstallError::Fatal(_)) {
            cleanup_staged_dir(stage_dir);
        }
        Err(err)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (deb_path, stage_dir);
        zeroize_string(password);
        Err(SudoInstallError::Fatal(
            "sudo .deb installation is only supported on Linux".into(),
        ))
    }
}

/// Strips Windows `\\?\` verbatim drive prefixes returned by `fs::canonicalize` (e.g.
/// `\\?\C:\Users\...\glide.exe` -> `C:\Users\...\glide.exe`) while leaving UNC paths intact.
fn strip_windows_verbatim_prefix(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\'
        {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// If Glide was just relaunched by an older Glide process after self-updating, blocks briefly
/// (at most 8s) until that parent PID has fully exited so its `.old` binary lock and network
/// ports are completely released before startup continues.
pub fn wait_for_relaunch_parent() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(pid) = std::env::var_os(RELAUNCH_WAIT_PID_ENV)
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<u32>().ok()))
        else {
            return;
        };
        if pid != 0 && pid != std::process::id() {
            debug!(pid, "Waiting for previous Glide instance to finish exiting after update");
            glide_backend::os_integration::wait_for_process_exit(pid, Duration::from_secs(8));
        }
    });
}

/// Removes any leftover `<current_exe>.old` backup or abandoned `<current_exe>.new.*` staging
/// files left behind by a Windows self-update.
///
/// Attempts immediate removal once (which typically succeeds right after `wait_for_relaunch_parent`),
/// and falls back to a tiny background retry thread if Windows Defender / process teardown still
/// holds a brief handle on `.old`.
pub fn cleanup_old_binary() {
    let Ok(exe) = std::env::current_exe().map(|p| strip_windows_verbatim_prefix(&p)) else {
        return;
    };
    let backup = append_os_suffix(&exe, ".old");
    let new_prefix = exe
        .file_name()
        .map(|n| format!("{}.new.", n.to_string_lossy()));
    let parent_dir = exe.parent().map(Path::to_path_buf);

    let mut pending_old = backup.exists();
    if pending_old && fs::remove_file(&backup).is_ok() {
        debug!(path = %backup.display(), "Cleaned up previous Glide binary after update");
        pending_old = false;
    }

    if let (Some(dir), Some(prefix)) = (parent_dir, new_prefix) {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(&prefix) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    if !pending_old {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..24 {
            match fs::remove_file(&backup) {
                Ok(()) => {
                    debug!(path = %backup.display(), "Cleaned up previous Glide binary after update");
                    return;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
        warn!(path = %backup.display(), "Could not remove old binary backup after update");
    });
}

/// Relaunches Glide cleanly after the exiting process has released all sockets, hooks, and tray.
pub fn spawn_relaunch(exe_path: &Path, config_path: Option<&Path>) -> Result<(), String> {
    use std::process::Stdio;

    let clean_exe = strip_windows_verbatim_prefix(exe_path);
    let pid = std::process::id().to_string();

    let build_cmd = || {
        let mut cmd = std::process::Command::new(&clean_exe);
        if let Some(cfg) = config_path {
            cmd.arg("--config").arg(cfg);
        }
        cmd.arg("gui")
            .env(RELAUNCH_WAIT_PID_ENV, &pid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    };

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

        let base_flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        let mut cmd = build_cmd();
        cmd.creation_flags(base_flags | CREATE_BREAKAWAY_FROM_JOB);
        if cmd.spawn().is_ok() {
            return Ok(());
        }
        let mut fallback = build_cmd();
        fallback.creation_flags(base_flags);
        fallback
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed relaunching {}: {e}", clean_exe.display()))
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut cmd = build_cmd();
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed relaunching {}: {e}", clean_exe.display()))
    }
}

/// Opens a verified Glide GitHub release `https://` URL in the user's default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    validate_release_url(url)?;
    glide_backend::os_integration::open_url(url).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_assets(tag: &str) -> Vec<GithubAsset> {
        vec![
            GithubAsset {
                name: WINDOWS_BINARY_ASSET.into(),
                browser_download_url: format!(
                    "https://github.com/4our4ace/glide/releases/download/{tag}/{WINDOWS_BINARY_ASSET}"
                ),
                size: 20_000_000,
            },
            GithubAsset {
                name: WINDOWS_MSI_ASSET.into(),
                browser_download_url: format!(
                    "https://github.com/4our4ace/glide/releases/download/{tag}/{WINDOWS_MSI_ASSET}"
                ),
                size: 11_000_000,
            },
            GithubAsset {
                name: LINUX_BINARY_ASSET.into(),
                browser_download_url: format!(
                    "https://github.com/4our4ace/glide/releases/download/{tag}/{LINUX_BINARY_ASSET}"
                ),
                size: 30_000_000,
            },
            GithubAsset {
                name: "glide_0.3.0_amd64.deb".into(),
                browser_download_url: format!(
                    "https://github.com/4our4ace/glide/releases/download/{tag}/glide_0.3.0_amd64.deb"
                ),
                size: 10_000_000,
            },
            GithubAsset {
                name: CHECKSUMS_ASSET_NAME.into(),
                browser_download_url: format!(
                    "https://github.com/4our4ace/glide/releases/download/{tag}/{CHECKSUMS_ASSET_NAME}"
                ),
                size: 264,
            },
        ]
    }

    #[test]
    fn semver_parsing_and_comparison() {
        assert_eq!(parse_semver("0.3.0"), Some((0, 3, 0)));
        assert_eq!(parse_semver("v0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_semver("v1.2.3-beta.1"), Some((1, 2, 3)));
        assert!(is_newer_version("0.3.0", "v0.3.1"));
        assert!(is_newer_version("0.3.0", "v0.4.0"));
        assert!(is_newer_version("0.3.0", "v1.0.0"));
        assert!(!is_newer_version("0.3.0", "v0.3.0"));
        assert!(!is_newer_version("0.3.0", "v0.2.9"));
    }

    #[test]
    fn release_url_restricted_to_glide_repo() {
        assert_eq!(
            sanitize_release_url(Some(
                "https://github.com/4our4ace/glide/releases/tag/v0.4.0"
            )),
            "https://github.com/4our4ace/glide/releases/tag/v0.4.0"
        );
        assert_eq!(
            sanitize_release_url(Some("https://evil.example/glide")),
            RELEASES_PAGE_URL
        );
        assert_eq!(
            sanitize_release_url(Some("https://github.com/other/glide/releases")),
            RELEASES_PAGE_URL
        );
        assert_eq!(
            sanitize_release_url(Some("https://github.com/4our4ace/glide/\"&calc")),
            RELEASES_PAGE_URL
        );
        assert_eq!(
            sanitize_release_url(Some("https://github.com/4our4ace/glide/releases\r\n")),
            RELEASES_PAGE_URL
        );
        assert_eq!(
            sanitize_release_url(Some("https://github.com/4our4ace/glide/../other/repo")),
            RELEASES_PAGE_URL
        );
        assert_eq!(sanitize_release_url(None), RELEASES_PAGE_URL);
        assert!(open_url("https://evil.example/payload").is_err());
        assert!(open_url("http://github.com/4our4ace/glide/releases").is_err());
        assert!(open_url("https://github.com/4our4ace/glide/releases\0").is_err());
    }

    #[test]
    fn asset_download_url_restricted_to_glide_releases() {
        assert!(validate_asset_download_url(
            "https://github.com/4our4ace/glide/releases/download/nightly/glide-windows-x86_64.exe"
        )
        .is_ok());
        assert!(validate_asset_download_url(
            "https://github.com/other/glide/releases/download/nightly/glide-windows-x86_64.exe"
        )
        .is_err());
        assert!(validate_asset_download_url(
            "https://github.com/4our4ace/glide/releases/download/nightly/../evil.exe"
        )
        .is_err());
        assert!(validate_asset_download_url(
            "http://github.com/4our4ace/glide/releases/download/nightly/glide-windows-x86_64.exe"
        )
        .is_err());
    }

    #[test]
    fn parses_sha256sums_manifest_strictly() {
        let hex_a = "12b5322cc60fcc9f5640ed884aeaee04048e11c2e9782e6740059bbc583646f8";
        let hex_b = "bf7e268b15cea75d378d850e751f616b57edaf24766bb4c91c23f83c6eeb6ae5";
        let manifest = format!(
            "# comment\n{hex_a}  glide-windows-x86_64.exe\n{hex_b} *./glide_0.3.0_amd64.deb\nbadhex  other.bin\n"
        );
        let a = parse_sha256_for_asset(&manifest, "glide-windows-x86_64.exe").unwrap();
        assert_eq!(a[0], 0x12);
        assert_eq!(a[31], 0xf8);
        assert!(parse_sha256_for_asset(&manifest, "glide_0.3.0_amd64.deb").is_some());
        assert!(parse_sha256_for_asset(&manifest, "other.bin").is_none());
        assert!(parse_sha256_for_asset(&manifest, "missing.bin").is_none());
    }

    #[test]
    fn nightly_sha_extraction_and_channel_switching() {
        let nightly_rel = GithubRelease {
            tag_name: "nightly".into(),
            name: Some("Glide Nightly (7b68d47)".into()),
            body: Some("Commit: 7b68d47abcdef".into()),
            target_commitish: Some("nightly".into()),
            html_url: Some("https://github.com/4our4ace/glide/releases/tag/nightly".into()),
            assets: sample_assets("nightly"),
        };
        assert_eq!(extract_nightly_sha(&nightly_rel).as_deref(), Some("7b68d47"));

        // Nightly on same SHA -> up to date
        assert_eq!(
            evaluate_release(
                UpdateChannel::Nightly,
                UpdateChannel::Nightly,
                "0.3.0",
                "7b68d47",
                &nightly_rel,
                true,
            ),
            UpdateCheckState::UpToDate {
                latest: "nightly (7b68d47)".into(),
            }
        );

        // Nightly on older (or reverted) SHA -> Available!
        let state = evaluate_release(
            UpdateChannel::Nightly,
            UpdateChannel::Nightly,
            "0.3.0",
            "1111111",
            &nightly_rel,
            true,
        );
        match state {
            UpdateCheckState::Available(avail) => {
                assert_eq!(avail.version, "nightly (7b68d47)");
                assert!(!avail.is_channel_switch);
                assert!(avail.binary_asset.is_some());
                assert!(avail.msi_asset.is_some());
                assert!(avail.checksums_url.is_some());
            }
            other => panic!("expected Available, got {other:?}"),
        }

        // Stable -> Nightly switch always offers Nightly even on same commit
        let state = evaluate_release(
            UpdateChannel::Nightly,
            UpdateChannel::Stable,
            "0.3.0",
            "7b68d47",
            &nightly_rel,
            false,
        );
        match state {
            UpdateCheckState::Available(avail) => {
                assert!(avail.is_channel_switch);
                assert!(avail.binary_asset.is_some());
                assert!(avail.deb_asset.is_some());
            }
            other => panic!("expected Available for channel switch, got {other:?}"),
        }

        // Nightly -> Stable switch offers v0.3.0 even when version numbers match
        let stable_rel = GithubRelease {
            tag_name: "v0.3.0".into(),
            name: Some("Glide v0.3.0".into()),
            body: None,
            target_commitish: Some("main".into()),
            html_url: Some("https://github.com/4our4ace/glide/releases/tag/v0.3.0".into()),
            assets: sample_assets("v0.3.0"),
        };
        let state = evaluate_release(
            UpdateChannel::Stable,
            UpdateChannel::Nightly,
            "0.3.0",
            "7b68d47",
            &stable_rel,
            true,
        );
        match state {
            UpdateCheckState::Available(avail) => {
                assert!(avail.is_channel_switch);
                assert_eq!(avail.version, "v0.3.0");
            }
            other => panic!("expected Available for Nightly->Stable rollback, got {other:?}"),
        }
    }

    #[test]
    fn strips_windows_verbatim_prefix_safely() {
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\C:\Users\alice\glide.exe")),
            PathBuf::from(r"C:\Users\alice\glide.exe")
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\UNC\server\share\glide.exe")),
            PathBuf::from(r"\\?\UNC\server\share\glide.exe")
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new("/usr/bin/glide")),
            PathBuf::from("/usr/bin/glide")
        );
    }

    #[test]
    fn classifies_sudo_errors_for_retry_vs_fatal() {
        assert!(matches!(
            classify_sudo_failure("Sorry, try again.\nsudo: 1 incorrect password attempt\n", 1),
            SudoInstallError::WrongPassword(_)
        ));
        assert!(matches!(
            classify_sudo_failure("sudo: no password was provided\n", 1),
            SudoInstallError::WrongPassword(_)
        ));
        assert!(matches!(
            classify_sudo_failure("alice is not in the sudoers file.\n", 1),
            SudoInstallError::Fatal(msg) if msg.contains("sudo permission")
        ));
        assert!(matches!(
            classify_sudo_failure("dpkg: error processing archive glide.deb\n", 2),
            SudoInstallError::Fatal(msg) if msg.contains("dpkg: error")
        ));
    }

    #[test]
    fn zeroize_string_clears_buffer_and_length() {
        let mut s = String::from("super-secret-password");
        zeroize_string(&mut s);
        assert!(s.is_empty());
    }

    #[test]
    fn cleanup_staged_dir_only_removes_glide_temp_dirs() {
        let temp = std::env::temp_dir();
        let ok_dir = temp.join(format!("glide-update-test-{}", std::process::id()));
        let other_dir = temp.join(format!("not-glide-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&ok_dir);
        let _ = fs::create_dir_all(&other_dir);

        cleanup_staged_dir(&ok_dir);
        cleanup_staged_dir(&other_dir);

        assert!(!ok_dir.exists());
        assert!(other_dir.exists());
        let _ = fs::remove_dir_all(&other_dir);
    }

    #[test]
    fn legacy_and_descriptive_asset_names_both_resolve() {
        // Legacy v0.3.0 release filenames still resolve cleanly for Nightly -> Stable rollback.
        let legacy_assets = vec![
            GithubAsset {
                name: WINDOWS_BINARY_LEGACY_ASSET.into(),
                browser_download_url: format!("{ASSET_URL_PREFIX}v0.3.0/{WINDOWS_BINARY_LEGACY_ASSET}"),
                size: 20_000_000,
            },
            GithubAsset {
                name: LINUX_BINARY_LEGACY_ASSET.into(),
                browser_download_url: format!("{ASSET_URL_PREFIX}v0.3.0/{LINUX_BINARY_LEGACY_ASSET}"),
                size: 30_000_000,
            },
            GithubAsset {
                name: CHECKSUMS_ASSET_NAME.into(),
                browser_download_url: format!("{ASSET_URL_PREFIX}v0.3.0/{CHECKSUMS_ASSET_NAME}"),
                size: 256,
            },
        ];
        let win_legacy = select_assets_for_platform(&legacy_assets, true);
        assert_eq!(
            win_legacy.binary_asset.as_ref().map(|a| a.name.as_str()),
            Some(WINDOWS_BINARY_LEGACY_ASSET)
        );
        let lin_legacy = select_assets_for_platform(&legacy_assets, false);
        assert_eq!(
            lin_legacy.binary_asset.as_ref().map(|a| a.name.as_str()),
            Some(LINUX_BINARY_LEGACY_ASSET)
        );

        // When both legacy and new names exist, new descriptive names win.
        let mut mixed = legacy_assets;
        mixed.extend(sample_assets("nightly"));
        let win_new = select_assets_for_platform(&mixed, true);
        assert_eq!(
            win_new.binary_asset.as_ref().map(|a| a.name.as_str()),
            Some(WINDOWS_BINARY_ASSET)
        );
        assert_eq!(
            win_new.msi_asset.as_ref().map(|a| a.name.as_str()),
            Some(WINDOWS_MSI_ASSET)
        );
        let lin_new = select_assets_for_platform(&mixed, false);
        assert_eq!(
            lin_new.binary_asset.as_ref().map(|a| a.name.as_str()),
            Some(LINUX_BINARY_ASSET)
        );
    }
}
