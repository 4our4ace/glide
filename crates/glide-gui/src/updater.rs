//! On-demand GitHub Releases update checker (zero background network traffic).
//!
//! Only contacts GitHub when the user explicitly clicks "Check" in Settings.

use serde::Deserialize;
use std::time::Duration;

pub const RELEASES_PAGE_URL: &str = "https://github.com/4our4ace/glide/releases";
pub const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/4our4ace/glide/releases/latest";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpdateCheckState {
    #[default]
    Idle,
    Checking,
    UpToDate {
        latest: String,
    },
    Available {
        version: String,
        url: String,
    },
    Error(String),
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: Option<String>,
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

/// Performs a single blocking HTTPS check against GitHub Releases (`latest`).
pub fn check_latest_release(current_version: &str) -> UpdateCheckState {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(6)))
        .build()
        .new_agent();

    let resp = agent
        .get(LATEST_RELEASE_API_URL)
        .header("User-Agent", &format!("Glide/{current_version}"))
        .header("Accept", "application/vnd.github+json")
        .call();

    let mut resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => {
            return UpdateCheckState::UpToDate {
                latest: format!("v{}", current_version.trim_start_matches(['v', 'V'])),
            };
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

    let latest_tag = release.tag_name.trim().to_owned();
    let display_tag = if latest_tag.starts_with(['v', 'V']) {
        latest_tag.clone()
    } else {
        format!("v{latest_tag}")
    };

    if is_newer_version(current_version, &latest_tag) {
        let url = release
            .html_url
            .filter(|u| u.starts_with("https://"))
            .unwrap_or_else(|| RELEASES_PAGE_URL.to_owned());
        UpdateCheckState::Available {
            version: display_tag,
            url,
        }
    } else {
        UpdateCheckState::UpToDate {
            latest: display_tag,
        }
    }
}

/// Opens an `https://` URL in the user's default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("Refusing to open non-HTTPS URL".into());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = url;
        Err("Opening browser links is unsupported on this OS".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
