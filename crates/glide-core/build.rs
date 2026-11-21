use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GLIDE_BUILD_SHA");
    println!("cargo:rerun-if-env-changed=GLIDE_BUILD_CHANNEL");

    let git_dir = Path::new("../../.git");
    let head_path = git_dir.join("HEAD");
    if head_path.exists() {
        println!("cargo:rerun-if-changed={}", head_path.display());
        if let Ok(head) = std::fs::read_to_string(&head_path) {
            if let Some(rel_ref) = head.trim().strip_prefix("ref: ") {
                let ref_path = git_dir.join(rel_ref.trim());
                if ref_path.exists() {
                    println!("cargo:rerun-if-changed={}", ref_path.display());
                }
            }
        }
    }

    let channel = match std::env::var("GLIDE_BUILD_CHANNEL") {
        Ok(v) if v.trim().eq_ignore_ascii_case("nightly") => "nightly",
        _ => "stable",
    };

    let sha = std::env::var("GLIDE_BUILD_SHA")
        .ok()
        .and_then(|s| sanitize_short_sha(&s))
        .or_else(git_short_sha)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GLIDE_BUILD_CHANNEL={channel}");
    println!("cargo:rustc-env=GLIDE_BUILD_SHA={sha}");
}

fn sanitize_short_sha(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() < 7 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(trimmed[..7].to_ascii_lowercase())
}

fn git_short_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    sanitize_short_sha(&text)
}
