//! On-demand GitHub Releases update checker (zero background network traffic).
//!
//! Only contacts GitHub when the user explicitly clicks "Check" in Settings.

use serde::Deserialize;
use std::time::Duration;

pub const REPO_URL_PREFIX: &str = "https://github.com/4our4ace/glide/";
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

/// Restricts release links returned by the GitHub API to the official Glide repository.
pub fn sanitize_release_url(html_url: Option<&str>) -> String {
    html_url
        .filter(|u| validate_release_url(u).is_ok())
        .map(str::to_owned)
        .unwrap_or_else(|| RELEASES_PAGE_URL.to_owned())
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
        let url = sanitize_release_url(release.html_url.as_deref());
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

/// Opens a verified Glide GitHub release `https://` URL in the user's default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    validate_release_url(url)?;
    glide_backend::os_integration::open_url(url).map_err(|e| e.to_string())
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
}
