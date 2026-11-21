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
const WINDOWS_BINARY_ASSET: &str = "glide-windows-x86_64.exe";
const LINUX_BINARY_ASSET: &str = "glide-linux-x86_64";
const MAX_CHECKSUMS_BYTES: usize = 64 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;

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
    /// Standalone executable asset for the current OS (`glide-windows-x86_64.exe` / `glide-linux-x86_64`).
    pub binary_asset: Option<UpdateAsset>,
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
            self.binary_asset.is_some()
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

fn select_assets_for_platform(
    assets: &[GithubAsset],
    want_windows: bool,
) -> (Option<UpdateAsset>, Option<UpdateAsset>, Option<String>) {
    let mut binary_asset = None;
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
        if want_windows {
            if a.name == WINDOWS_BINARY_ASSET {
                binary_asset = Some(UpdateAsset {
                    name: a.name.clone(),
                    download_url: a.browser_download_url.clone(),
                    size: a.size,
                });
            }
        } else {
            if a.name == LINUX_BINARY_ASSET {
                binary_asset = Some(UpdateAsset {
                    name: a.name.clone(),
                    download_url: a.browser_download_url.clone(),
                    size: a.size,
                });
            } else if a.name.starts_with("glide_") && a.name.ends_with("_amd64.deb") {
                deb_asset = Some(UpdateAsset {
                    name: a.name.clone(),
                    download_url: a.browser_download_url.clone(),
                    size: a.size,
                });
            }
        }
    }

    (binary_asset, deb_asset, checksums_url)
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
    let (binary_asset, deb_asset, checksums_url) =
        select_assets_for_platform(&release.assets, want_windows);
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

#[cfg(target_os = "linux")]
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

/// Downloads, SHA-256 verifies, and atomically installs `update`, returning the executable path to
/// relaunch once the current process has released its ports and tray icon.
///
/// `on_progress` receives download percentage (`0..=100`). `on_installing` fires right after the
/// checksum passes and before the OS install/swap runs.
pub fn download_and_install_update<Fp, Fi>(
    update: &AvailableUpdate,
    current_version: &str,
    allow_sudo_fallback: bool,
    mut on_progress: Fp,
    mut on_installing: Fi,
) -> Result<PathBuf, String>
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
        .map_err(|e| format!("Failed locating current Glide executable: {e}"))?;

    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .new_agent();

    #[cfg(target_os = "windows")]
    {
        let _ = allow_sudo_fallback;
        let asset = update
            .binary_asset
            .as_ref()
            .ok_or_else(|| "Release does not include a Windows binary".to_string())?;
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
        // replacement directly into its original path so taskbar pins & autostart stay intact.
        fs::rename(&current_exe, &backup).map_err(|e| {
            let _ = fs::remove_file(&staged);
            format!(
                "Cannot replace {} (move Glide out of Program Files or run as Administrator): {e}",
                current_exe.display()
            )
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
        Ok(current_exe)
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

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
            return Ok(current_exe);
        }

        // System-wide install (e.g. /usr/bin/glide from .deb): download + verify the .deb into a
        // private 0700 temp directory and install via pkexec (GUI polkit) or sudo (CLI).
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

        on_installing();
        let elevator = if !allow_sudo_fallback && command_exists("pkexec") {
            Some("pkexec")
        } else if allow_sudo_fallback && command_exists("sudo") {
            Some("sudo")
        } else if command_exists("pkexec") {
            Some("pkexec")
        } else {
            None
        };

        let Some(elevator) = elevator else {
            let _ = fs::remove_dir_all(&stage_dir);
            return Err(
                "Administrator elevation (pkexec) unavailable; download the .deb manually".into(),
            );
        };

        let status = std::process::Command::new(elevator)
            .arg("dpkg")
            .arg("-i")
            .arg(&deb_path)
            .status()
            .map_err(|e| {
                let _ = fs::remove_dir_all(&stage_dir);
                format!("Failed launching {elevator} dpkg -i: {e}")
            })?;

        let _ = fs::remove_dir_all(&stage_dir);
        if !status.success() {
            return Err(format!(
                "Package install cancelled or failed (exit {})",
                status.code().unwrap_or(-1)
            ));
        }

        let relaunch = if Path::new("/usr/bin/glide").exists() {
            PathBuf::from("/usr/bin/glide")
        } else {
            current_exe
        };
        info!(
            version = %update.version,
            exe = %relaunch.display(),
            "Installed verified Glide .deb update on Linux"
        );
        Ok(relaunch)
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (
            checksums_url,
            current_exe,
            agent,
            allow_sudo_fallback,
            &mut on_progress,
            &mut on_installing,
        );
        Err("Self-update is only supported on Windows and Linux".into())
    }
}

/// Removes any leftover `<current_exe>.old` backup left behind by a Windows self-update.
/// Runs asynchronously on a tiny background thread so startup latency is never affected.
pub fn cleanup_old_binary() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let backup = append_os_suffix(&exe, ".old");
    if !backup.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..16 {
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
    let mut cmd = std::process::Command::new(exe_path);
    if let Some(cfg) = config_path {
        cmd.arg("--config").arg(cfg);
    }
    cmd.arg("gui");
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("Failed relaunching {}: {e}", exe_path.display()))
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
}
