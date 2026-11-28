//! Step 10: Zero-friction OS integration helpers (Autostart, Windows Firewall,
//! Windows Elevated Task helper, and safe file-drop destination folder).

use anyhow::{anyhow, Context, Result};
use std::path::{Component, Path, PathBuf};

/// Maximum URL length accepted by [`open_url`] and [`open_loopback_url`].
pub const MAX_URL_LEN: usize = 2048;

fn user_home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE")
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
            .map(PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
            .map(PathBuf::from)
    }
}

fn default_receive_dir_path() -> PathBuf {
    match user_home_dir() {
        Some(home) => home.join("Downloads").join("Glide"),
        None => PathBuf::from(".").join("Glide"),
    }
}

/// Returns `true` if `s` has a root or Windows drive/UNC prefix (checked cross-platform so
/// paths like `~/C:\Windows` are rejected everywhere).
fn has_drive_or_root_prefix(s: &str) -> bool {
    let path = Path::new(s);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return true;
    }
    let bytes = s.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Resolves a custom or default file-receive directory without modifying the filesystem.
///
/// - `None` resolves to `~/Downloads/Glide` (or `%USERPROFILE%\Downloads\Glide` on Windows).
/// - `Some(raw)` validates that the path is non-empty and free of NUL bytes, expands a leading
///   `~`, `~/`, or `~\` against the user's home directory (trimming repeated leading slashes and
///   rejecting rooted/drive-prefixed remainders such as `~/C:\Windows`), and rejects bare relative
///   paths so files never land in an arbitrary process CWD.
pub fn resolve_receive_dir(custom: Option<&str>) -> Result<PathBuf> {
    let Some(raw) = custom else {
        return Ok(default_receive_dir_path());
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("receive directory path cannot be empty"));
    }
    if trimmed.contains('\0') {
        return Err(anyhow!("receive directory path contains a NUL byte"));
    }

    if trimmed == "~" {
        return user_home_dir()
            .ok_or_else(|| anyhow!("cannot expand '~': user home directory is unavailable"));
    }

    if trimmed.starts_with("~/") || trimmed.starts_with("~\\") {
        let home = user_home_dir()
            .ok_or_else(|| anyhow!("cannot expand '~/': user home directory is unavailable"))?;
        let rest = trimmed[1..].trim_start_matches(['/', '\\']);
        if rest.is_empty() {
            return Ok(home);
        }
        if has_drive_or_root_prefix(rest) {
            return Err(anyhow!(
                "invalid rooted or drive-prefixed path after '~': '{trimmed}'"
            ));
        }
        return Ok(home.join(rest));
    }

    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(anyhow!(
            "receive directory must be an absolute path or start with '~/': '{trimmed}'"
        ));
    }

    Ok(path)
}

/// Resolves and creates (if needed) the destination folder for received files.
pub fn ensure_receive_dir(custom: Option<&str>) -> Result<PathBuf> {
    let dir = resolve_receive_dir(custom)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create receive directory {}", dir.display()))?;
    Ok(dir)
}

/// Returns (and creates if needed) the default destination folder for received files:
/// `~/Downloads/Glide` on Linux or `%USERPROFILE%\Downloads\Glide` on Windows.
pub fn file_receive_dir() -> Result<PathBuf> {
    ensure_receive_dir(None)
}

/// Strips Windows verbatim prefixes (`\\?\` / `\\?\UNC\`) and normalizes `/` -> `\` for Shell APIs.
#[allow(dead_code)]
fn normalize_windows_shell_path(raw: &str) -> String {
    let stripped = if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        raw.to_owned()
    };
    stripped.replace('/', "\\")
}

/// Converts `path` into an absolute path (preserving symlink names) and normalizes it for OS APIs.
fn absolutize_os_path(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(normalize_windows_shell_path(&abs.to_string_lossy()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        abs
    }
}

/// Computes the parent directory to open when revealing a file (falling back to `.` for bare
/// relative filenames so a file path is never passed directly to a folder opener).
fn reveal_parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Reaps a spawned Unix helper process on a detached thread so Glide never accumulates zombies.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reap_child_in_background(mut child: std::process::Child) {
    let _ = std::thread::Builder::new()
        .name("glide-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(target_os = "windows")]
fn encode_wide_null(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(target_os = "windows")]
fn windows_shell_open(path: &Path) -> Result<()> {
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let wide = encode_wide_null(path);
    let res = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            op.as_ptr(),
            wide.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (res as isize) > 32 {
        Ok(())
    } else {
        anyhow::bail!(
            "ShellExecuteW failed opening {} (code {})",
            path.display(),
            res as isize
        )
    }
}

#[cfg(target_os = "windows")]
fn windows_reveal_in_explorer(path: &Path) -> Result<()> {
    use std::ptr;
    use windows_sys::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems};

    let com_hr = unsafe {
        CoInitializeEx(
            ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    let should_uninit = com_hr >= 0;
    if com_hr < 0 && com_hr != RPC_E_CHANGED_MODE {
        return windows_shell_open(reveal_parent_dir(path));
    }

    struct ComGuard(bool);
    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }
    let _com_guard = ComGuard(should_uninit);

    let wide = encode_wide_null(path);
    let pidl = unsafe { ILCreateFromPathW(wide.as_ptr()) };
    if !pidl.is_null() {
        // Vista+: passing cidl == 0 with a file PIDL opens its parent folder and selects the item
        // reliably even when the filename contains commas, spaces, or Unicode characters.
        let hr = unsafe { SHOpenFolderAndSelectItems(pidl, 0, ptr::null(), 0) };
        unsafe {
            ILFree(pidl);
        }
        if hr >= 0 {
            return Ok(());
        }
    }

    windows_shell_open(reveal_parent_dir(path))
}

/// Opens a file or folder in the user's OS default handler / file manager without invoking a shell.
pub fn open_path_in_os(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("path does not exist: {}", path.display());
    }
    let abs = absolutize_os_path(path);

    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};
        let child = Command::new("xdg-open")
            .arg(&abs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to open {} with xdg-open", abs.display()))?;
        reap_child_in_background(child);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        windows_shell_open(&abs)
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        let child = Command::new("open")
            .arg("--")
            .arg(&abs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to open {} with open", abs.display()))?;
        reap_child_in_background(child);
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = abs;
        anyhow::bail!("opening local paths is unsupported on this OS")
    }
}

/// Reveals a file in the OS file manager (or opens the folder directly if `path` is a directory).
pub fn reveal_in_file_manager(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("path does not exist: {}", path.display());
    }
    let abs = absolutize_os_path(path);
    if abs.is_dir() {
        return open_path_in_os(&abs);
    }

    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};
        let target = reveal_parent_dir(&abs);
        let child = Command::new("xdg-open")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to reveal {} with xdg-open", target.display()))?;
        reap_child_in_background(child);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        windows_reveal_in_explorer(&abs)
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        let child = Command::new("open")
            .arg("-R")
            .arg("--")
            .arg(&abs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to reveal {} with open", abs.display()))?;
        reap_child_in_background(child);
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = abs;
        anyhow::bail!("revealing local files is unsupported on this OS")
    }
}

/// Checks whether Glide is currently configured to start automatically on login.
pub fn is_autostart_enabled() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_autostart_path().is_some_and(|p| p.exists())
    }
    #[cfg(target_os = "windows")]
    {
        windows_is_autostart_enabled()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

/// Enables or disables starting Glide automatically at user login.
pub fn set_autostart(enabled: bool, args: &str) -> Result<()> {
    validate_autostart_args(args)?;

    #[cfg(target_os = "linux")]
    {
        let path = linux_autostart_path().context("could not determine XDG autostart path")?;
        if !enabled {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/bin/glide"));
        let exe_str = exe
            .to_str()
            .context("current executable path is not valid UTF-8")?;
        let desktop = build_linux_desktop_entry(exe_str, args)?;
        std::fs::write(&path, desktop)?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        windows_set_autostart(enabled, args)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (enabled, args);
        Ok(())
    }
}

#[cfg(any(target_os = "windows", test))]
fn escape_ps_single_quoted(s: &str) -> String {
    // PowerShell treats ASCII apostrophe and Unicode single-quote variants as
    // single-quoted string delimiters; doubling each escapes it literally.
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        out.push(ch);
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            out.push(ch);
        }
    }
    out
}

#[cfg(any(target_os = "windows", test))]
fn encode_ps_encoded_command(script: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(any(target_os = "windows", test))]
fn build_elevated_ps_launcher(inner_script: &str) -> String {
    let encoded = encode_ps_encoded_command(inner_script);
    format!(
        "$ErrorActionPreference = 'Stop'; \
         $p = Start-Process powershell -Verb RunAs -WindowStyle Hidden -Wait -PassThru \
         -ArgumentList @('-NoProfile', '-WindowStyle', 'Hidden', '-EncodedCommand', '{encoded}'); \
         if ($null -eq $p) {{ exit 1 }}; exit $p.ExitCode"
    )
}

#[cfg(any(target_os = "windows", test))]
fn build_firewall_script() -> &'static str {
    "$ErrorActionPreference = 'Stop'; \
     Remove-NetFirewallRule -DisplayName 'Glide KVM (QUIC + Discovery)' -ErrorAction SilentlyContinue; \
     New-NetFirewallRule -DisplayName 'Glide KVM (QUIC + Discovery)' -Direction Inbound -Protocol UDP -LocalPort 24800,24801 -RemoteAddress LocalSubnet -Action Allow -Profile Any"
}

#[cfg(any(target_os = "windows", test))]
fn build_elevated_task_script(exe: &std::path::Path, username: Option<&str>) -> String {
    let exe_esc = escape_ps_single_quoted(&exe.to_string_lossy());
    let user_expr = match username.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => format!("'{}'", escape_ps_single_quoted(u)),
        None => "$env:USERNAME".to_owned(),
    };
    format!(
        "$ErrorActionPreference = 'Stop'; \
         $action = New-ScheduledTaskAction -Execute '{exe_esc}' -Argument 'gui --minimized'; \
         $trigger = New-ScheduledTaskTrigger -AtLogOn; \
         $principal = New-ScheduledTaskPrincipal -UserId {user_expr} -LogonType Interactive -RunLevel Highest; \
         Register-ScheduledTask -TaskName 'GlideKVM' -Action $action -Trigger $trigger -Principal $principal -Force"
    )
}

/// Opens an elevated PowerShell prompt on Windows to allow UDP ports 24800 & 24801
/// from local LAN / direct-link subnets across Private, Domain, and Public/Unidentified profiles.
pub fn configure_firewall() -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let launcher = build_elevated_ps_launcher(build_firewall_script());
        let status = Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-NoProfile",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &launcher,
            ])
            .status()
            .context("failed to invoke PowerShell for firewall rule")?;
        if status.success() {
            Ok(
                "Windows Firewall rule created for UDP 24800 & 24801 (LocalSubnet across all profiles)."
                    .into(),
            )
        } else {
            anyhow::bail!("Firewall helper exited with status {status}")
        }
    }
    #[cfg(target_os = "linux")]
    {
        Ok("On Ubuntu, if UFW is enabled, allow Glide only from your LAN subnet (replace with your subnet), e.g.: sudo ufw allow from 192.168.1.0/24 to any port 24800:24801 proto udp".into())
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Ok("Allow inbound UDP ports 24800 and 24801 on your local network.".into())
    }
}

/// Creates a Windows Task Scheduler task running Glide with Highest Privileges
/// so input continues seamlessly over Administrator windows and UAC prompts.
pub fn configure_elevated_task() -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let exe = std::env::current_exe().context("cannot determine glide.exe path")?;
        let user = std::env::var("USERNAME").ok();
        let ps_cmd = build_elevated_task_script(&exe, user.as_deref());
        let launcher = build_elevated_ps_launcher(&ps_cmd);
        let status = Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-NoProfile",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &launcher,
            ])
            .status()
            .context("failed to invoke PowerShell for elevated task")?;
        if status.success() {
            Ok("Registered elevated Scheduled Task 'GlideKVM' (runs at logon with Admin rights for UAC/elevated windows).".into())
        } else {
            anyhow::bail!("Elevated task setup exited with status {status}")
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok("Elevated Scheduled Task helper is Windows-only. On Ubuntu use: systemctl --user enable --now glide-client.service".into())
    }
}

/// Validates an external `https://` URL before passing it to the OS browser launcher.
pub fn validate_https_url(url: &str) -> Result<()> {
    validate_url_chars_and_len(url)?;
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("Refusing to open non-HTTPS URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        anyhow::bail!("Invalid HTTPS URL authority");
    }
    Ok(())
}

/// Validates a loopback `http://127.0.0.1:<port>`, `http://localhost:<port>`,
/// or `http://[::1]:<port>` URL for the local browser fallback UI.
pub fn validate_loopback_url(url: &str) -> Result<()> {
    validate_url_chars_and_len(url)?;
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| url.strip_prefix("http://localhost:"))
        .or_else(|| url.strip_prefix("http://[::1]:"))
        .ok_or_else(|| anyhow::anyhow!("Refusing to open non-loopback local URL"))?;
    let port_str = rest.split(['/', '?', '#']).next().unwrap_or("");
    let port: u16 = port_str
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid loopback URL port"))?;
    if port == 0 {
        anyhow::bail!("Invalid loopback URL port 0");
    }
    Ok(())
}

fn validate_url_chars_and_len(url: &str) -> Result<()> {
    if url.is_empty() {
        anyhow::bail!("URL must not be empty");
    }
    if url.len() > MAX_URL_LEN {
        anyhow::bail!("URL exceeds maximum allowed length of {MAX_URL_LEN} bytes");
    }
    if url.chars().any(|c| {
        c.is_control()
            || c.is_whitespace()
            || matches!(c, '"' | '\'' | '`' | '\\')
    }) {
        anyhow::bail!("URL contains forbidden characters");
    }
    Ok(())
}

/// Opens a validated `https://` URL in the user's default browser without invoking a shell.
pub fn open_url(url: &str) -> Result<()> {
    validate_https_url(url)?;
    open_validated_url(url)
}

/// Opens a validated loopback URL in the user's default browser without invoking a shell.
pub fn open_loopback_url(url: &str) -> Result<()> {
    validate_loopback_url(url)?;
    open_validated_url(url)
}

fn open_validated_url(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::ptr;
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let operation: Vec<u16> = "open\0".encode_utf16().collect();
        let target: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        let ret = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                operation.as_ptr(),
                target.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            )
        } as isize;
        if ret <= 32 {
            anyhow::bail!("ShellExecuteW failed with code {ret}");
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .context("failed to launch xdg-open")?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .context("failed to launch open")?;
        Ok(())
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = url;
        anyhow::bail!("Opening browser links is unsupported on this OS")
    }
}

fn validate_autostart_args(args: &str) -> Result<()> {
    if args
        .chars()
        .any(|c| c.is_control() || (c.is_whitespace() && c != ' '))
    {
        anyhow::bail!("autostart arguments contain forbidden control or newline characters");
    }
    Ok(())
}

/// Escapes a single argument token for a Freedesktop `.desktop` `Exec=` key.
///
/// Per the Desktop Entry Specification, `.desktop` string unescaping runs before
/// `Exec=` argument unquoting, so quoted `\`, `"`, `$`, and `` ` `` are prefixed
/// with `\\`, and literal `%` is doubled to `%%` to prevent field-code expansion.
#[cfg(any(target_os = "linux", test))]
fn escape_desktop_exec_arg(arg: &str) -> Result<String> {
    if arg.chars().any(|c| c.is_control()) {
        anyhow::bail!("desktop Exec argument contains forbidden control characters");
    }
    let needs_quoting = arg.is_empty()
        || arg.chars().any(|c| {
            matches!(
                c,
                ' ' | '\t'
                    | '\n'
                    | '"'
                    | '\''
                    | '\\'
                    | '>'
                    | '<'
                    | '~'
                    | '|'
                    | '&'
                    | ';'
                    | '$'
                    | '*'
                    | '?'
                    | '#'
                    | '('
                    | ')'
                    | '`'
            )
        });

    if !needs_quoting {
        return Ok(arg.replace('%', "%%"));
    }

    let mut out = String::with_capacity(arg.len() + 8);
    out.push('"');
    for c in arg.chars() {
        match c {
            '\\' => out.push_str(r"\\\\"),
            '"' => out.push_str(r#"\\""#),
            '$' => out.push_str(r"\\$"),
            '`' => out.push_str(r"\\`"),
            '%' => out.push_str("%%"),
            other => out.push(other),
        }
    }
    out.push('"');
    Ok(out)
}

#[cfg(any(target_os = "linux", test))]
fn build_linux_desktop_entry(exe: &str, args: &str) -> Result<String> {
    if exe.is_empty() {
        anyhow::bail!("executable path must not be empty");
    }
    validate_autostart_args(args)?;
    let mut exec_parts = Vec::new();
    exec_parts.push(escape_desktop_exec_arg(exe)?);
    for tok in args.split(' ').filter(|s| !s.is_empty()) {
        exec_parts.push(escape_desktop_exec_arg(tok)?);
    }
    let exec_line = exec_parts.join(" ");
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Glide KVM\n\
         Comment=Ultra-low-latency software KVM\n\
         Exec={exec_line}\n\
         Icon=glide\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    ))
}

#[cfg(target_os = "linux")]
fn linux_autostart_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("autostart").join("glide.desktop"))
}

#[cfg(target_os = "windows")]
fn windows_is_autostart_enabled() -> bool {
    use std::ptr;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    let subkey: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "GlideKVM\0".encode_utf16().collect();
    let mut hkey: HKEY = ptr::null_mut();
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if opened != 0 {
        return false;
    }
    let exists = unsafe {
        RegQueryValueExW(
            hkey,
            value.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        ) == 0
    };
    unsafe {
        RegCloseKey(hkey);
    }
    exists
}

#[cfg(target_os = "windows")]
fn windows_set_autostart(enabled: bool, args: &str) -> Result<()> {
    use std::ptr;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_WRITE, REG_SZ,
    };
    let subkey: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
        .encode_utf16()
        .collect();
    let value_name: Vec<u16> = "GlideKVM\0".encode_utf16().collect();
    let mut hkey: HKEY = ptr::null_mut();
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &mut hkey) };
    if opened != 0 {
        anyhow::bail!("RegOpenKeyExW failed with error {opened}");
    }
    let res = if enabled {
        let exe = std::env::current_exe().context("cannot determine glide.exe path")?;
        let cmd = format!("\"{}\" {}\0", exe.display(), args);
        let wide: Vec<u16> = cmd.encode_utf16().collect();
        unsafe {
            RegSetValueExW(
                hkey,
                value_name.as_ptr(),
                0,
                REG_SZ,
                wide.as_ptr() as *const u8,
                (wide.len() * 2) as u32,
            )
        }
    } else {
        let r = unsafe { RegDeleteValueW(hkey, value_name.as_ptr()) };
        if r == 2 {
            0
        } else {
            r
        } // ERROR_FILE_NOT_FOUND is fine when disabling
    };
    unsafe {
        RegCloseKey(hkey);
    }
    if res != 0 {
        anyhow::bail!("Registry autostart update failed with error {res}");
    }
    Ok(())
}

/// Waits up to `max_wait` for process `pid` to exit cleanly.
///
/// Used by a freshly relaunched Glide process right after self-update so the previous instance
/// finishes releasing its executable lock (`.exe.old`) and network ports before startup continues.
pub fn wait_for_process_exit(pid: u32, max_wait: std::time::Duration) {
    if pid == 0 || pid == std::process::id() {
        return;
    }

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };

        let timeout_ms = max_wait.as_millis().min(u32::MAX as u128) as u32;
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !handle.is_null() {
                let _ = WaitForSingleObject(handle, timeout_ms);
                CloseHandle(handle);
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Event-driven wait via Linux 5.3+ pidfd_open + poll(2) — 0 polling, instant wake on exit.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0u32) };
        if pidfd >= 0 {
            let fd = pidfd as libc::c_int;
            let deadline = std::time::Instant::now() + max_wait;
            loop {
                let rem = deadline.saturating_duration_since(std::time::Instant::now());
                if rem.is_zero() {
                    break;
                }
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let timeout_ms = rem.as_millis().min(i32::MAX as u128) as libc::c_int;
                let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                }
                break;
            }
            unsafe {
                libc::close(fd);
            }
        } else {
            let err = std::io::Error::last_os_error();
            // ESRCH means the process already exited; only fall back to /proc on ENOSYS/EPERM.
            if err.raw_os_error() != Some(libc::ESRCH) {
                let proc_dir = PathBuf::from(format!("/proc/{pid}"));
                let deadline = std::time::Instant::now() + max_wait;
                while proc_dir.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = max_wait;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn decode_base64_utf16le(b64: &str) -> String {
        fn val(c: u8) -> u8 {
            match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => panic!("unexpected base64 char: {}", c as char),
            }
        }
        let bytes = b64.as_bytes();
        assert_eq!(bytes.len() % 4, 0);
        let mut raw = Vec::new();
        for chunk in bytes.chunks_exact(4) {
            let a = val(chunk[0]) as u32;
            let b = val(chunk[1]) as u32;
            let c = if chunk[2] == b'=' { 0 } else { val(chunk[2]) as u32 };
            let d = if chunk[3] == b'=' { 0 } else { val(chunk[3]) as u32 };
            let n = (a << 18) | (b << 12) | (c << 6) | d;
            raw.push(((n >> 16) & 0xFF) as u8);
            if chunk[2] != b'=' {
                raw.push(((n >> 8) & 0xFF) as u8);
            }
            if chunk[3] != b'=' {
                raw.push((n & 0xFF) as u8);
            }
        }
        assert_eq!(raw.len() % 2, 0);
        let u16s: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&u16s).expect("valid UTF-16LE")
    }

    #[test]
    fn encoded_command_matches_powershell_utf16le_base64() {
        // "A" in UTF-16LE is [0x41, 0x00] -> Base64 "QQA="
        assert_eq!(encode_ps_encoded_command("A"), "QQA=");
        let script = build_firewall_script();
        let encoded = encode_ps_encoded_command(script);
        assert!(encoded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='));
        assert_eq!(decode_base64_utf16le(&encoded), script);
    }

    #[test]
    fn elevated_task_script_escapes_quotes_in_path_and_username() {
        let exe = Path::new(r"C:\Users\O'Connor\AppData\glide.exe");
        let script = build_elevated_task_script(exe, Some("O'Connor"));
        assert!(script.contains(r"-Execute 'C:\Users\O''Connor\AppData\glide.exe'"));
        assert!(script.contains("-UserId 'O''Connor'"));

        // Unicode smart quotes (e.g. U+2019) are also delimiters in PowerShell
        let smart_exe = Path::new("C:\\Users\\J\u{2019}s\\日本語\\glide.exe");
        let smart_script = build_elevated_task_script(smart_exe, Some("J\u{2019}s"));
        assert!(smart_script.contains("C:\\Users\\J\u{2019}\u{2019}s\\日本語\\glide.exe"));
        assert!(smart_script.contains("-UserId 'J\u{2019}\u{2019}s'"));
        let encoded = encode_ps_encoded_command(&smart_script);
        assert_eq!(decode_base64_utf16le(&encoded), smart_script);

        // Fallback to $env:USERNAME when USERNAME is missing or blank
        let fallback = build_elevated_task_script(exe, Some("   "));
        assert!(fallback.contains("-UserId $env:USERNAME"));
    }

    #[test]
    fn elevated_launcher_uses_encoded_command_and_propagates_exit_code() {
        let script = build_elevated_task_script(
            Path::new(r"C:\Program Files\O'Connor's Glide\glide.exe"),
            Some("O'Connor"),
        );
        let launcher = build_elevated_ps_launcher(&script);
        assert!(launcher.contains("$ErrorActionPreference = 'Stop'"));
        assert!(launcher.contains("-EncodedCommand"));
        assert!(launcher.contains("-Wait -PassThru"));
        assert!(launcher.contains("exit $p.ExitCode"));
        // The launcher must not embed raw unencoded single-quoted payload from the inner script.
        assert!(!launcher.contains("Register-ScheduledTask"));
    }

    #[test]
    fn resolve_receive_dir_handles_default_custom_tilde_and_invalid() {
        // Default points to Downloads/Glide
        let default_dir = resolve_receive_dir(None).expect("default receive dir");
        assert!(
            default_dir.ends_with(Path::new("Downloads").join("Glide"))
                || default_dir.ends_with("Glide"),
            "unexpected default dir: {}",
            default_dir.display()
        );

        // Explicit absolute custom path is trimmed and preserved
        #[cfg(not(windows))]
        {
            let custom = resolve_receive_dir(Some("  /tmp/glide-inbox  ")).expect("custom dir");
            assert_eq!(custom, PathBuf::from("/tmp/glide-inbox"));
        }
        #[cfg(windows)]
        {
            let custom = resolve_receive_dir(Some(r"  C:\GlideInbox  ")).expect("custom dir");
            assert_eq!(custom, PathBuf::from(r"C:\GlideInbox"));
        }

        // Tilde expansion against user home (including repeated leading slashes!)
        if let Some(home) = user_home_dir() {
            assert_eq!(resolve_receive_dir(Some("~")).unwrap(), home);
            assert_eq!(
                resolve_receive_dir(Some("~/Transfers/Glide")).unwrap(),
                home.join("Transfers/Glide")
            );
            assert_eq!(resolve_receive_dir(Some("~/")).unwrap(), home);
            assert_eq!(resolve_receive_dir(Some("~///")).unwrap(), home);
            // ~//etc MUST stay anchored under home and NEVER discard home to become /etc!
            assert_eq!(resolve_receive_dir(Some("~//etc")).unwrap(), home.join("etc"));
            assert_eq!(
                resolve_receive_dir(Some("~///tmp/evil")).unwrap(),
                home.join("tmp/evil")
            );
            assert_eq!(
                resolve_receive_dir(Some(r"~\\Downloads\Glide")).unwrap(),
                home.join(r"Downloads\Glide")
            );
            // Drive-prefixed paths after ~/ MUST be rejected
            assert!(resolve_receive_dir(Some(r"~/C:\Windows")).is_err());
            assert!(resolve_receive_dir(Some("~///C:/Windows")).is_err());
        }

        // Empty, whitespace-only, NUL-containing, and bare relative paths are rejected
        assert!(resolve_receive_dir(Some("")).is_err());
        assert!(resolve_receive_dir(Some("   \t  ")).is_err());
        assert!(resolve_receive_dir(Some("/tmp/bad\0path")).is_err());
        assert!(resolve_receive_dir(Some("Inbox/Glide")).is_err());
        assert!(resolve_receive_dir(Some("./Glide")).is_err());
        assert!(resolve_receive_dir(Some("~otheruser/Downloads")).is_err());
    }

    #[test]
    fn reveal_parent_dir_never_returns_bare_filename_itself() {
        assert_eq!(reveal_parent_dir(Path::new("notes.txt")), Path::new("."));
        assert_eq!(
            reveal_parent_dir(Path::new("/home/user/Downloads/notes.txt")),
            Path::new("/home/user/Downloads")
        );
        assert_eq!(
            reveal_parent_dir(Path::new("subdir/notes.txt")),
            Path::new("subdir")
        );
    }

    #[test]
    fn normalize_windows_shell_path_strips_verbatim_prefixes_and_normalizes_slashes() {
        assert_eq!(
            normalize_windows_shell_path(r"\\?\C:\Users\alice/Downloads/a, b.txt"),
            r"C:\Users\alice\Downloads\a, b.txt"
        );
        assert_eq!(
            normalize_windows_shell_path(r"\\?\UNC\fileserver\share/inbox/report.pdf"),
            r"\\fileserver\share\inbox\report.pdf"
        );
        assert_eq!(
            normalize_windows_shell_path("C:/Users/alice/Downloads"),
            r"C:\Users\alice\Downloads"
        );
    }

    #[test]
    fn open_and_reveal_reject_missing_paths() {
        let missing = std::env::temp_dir().join(format!(
            "glide-nonexistent-path-{}-{}",
            std::process::id(),
            999_999
        ));
        assert!(open_path_in_os(&missing).is_err());
        assert!(reveal_in_file_manager(&missing).is_err());
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

    #[test]
    fn desktop_exec_escaping_handles_reserved_chars_and_rejects_newlines() {
        assert_eq!(
            escape_desktop_exec_arg("/usr/bin/glide").unwrap(),
            "/usr/bin/glide"
        );
        assert_eq!(
            escape_desktop_exec_arg("/home/user/My Apps/glide").unwrap(),
            "\"/home/user/My Apps/glide\""
        );
        assert_eq!(
            escape_desktop_exec_arg(r#"/opt/a"b$c\d`e%f/glide"#).unwrap(),
            r#""/opt/a\\"b\\$c\\\\d\\`e%%f/glide""#
        );
        assert_eq!(escape_desktop_exec_arg("100%").unwrap(), "100%%");
        assert!(escape_desktop_exec_arg("bad\nline").is_err());
        assert!(escape_desktop_exec_arg("bad\rline").is_err());
        assert!(escape_desktop_exec_arg("bad\0nul").is_err());

        let entry = build_linux_desktop_entry(
            "/home/user/My Apps/glide",
            "gui --minimized --auto-start client",
        )
        .unwrap();
        assert!(entry.contains(
            "Exec=\"/home/user/My Apps/glide\" gui --minimized --auto-start client\n"
        ));
        assert!(build_linux_desktop_entry("", "gui").is_err());
        assert!(build_linux_desktop_entry("/usr/bin/glide", "gui\nExec=evil").is_err());
        assert!(build_linux_desktop_entry("/usr/bin/glide", "gui\rExec=evil").is_err());
    }

    #[test]
    fn open_url_validation_rejects_unsafe_and_oversized_urls() {
        assert!(validate_https_url("https://github.com/4our4ace/glide/releases").is_ok());
        assert!(validate_https_url("http://github.com/4our4ace/glide/releases").is_err());
        assert!(validate_https_url("http://127.0.0.1:24802").is_err());
        assert!(validate_https_url("file:///etc/passwd").is_err());
        assert!(validate_https_url("javascript:alert(1)").is_err());
        assert!(validate_https_url("https://").is_err());
        assert!(validate_https_url("https://github.com@evil.example/foo").is_err());
        assert!(validate_https_url("https://example.com/a b").is_err());
        assert!(validate_https_url("https://example.com/a\"b").is_err());
        assert!(validate_https_url("https://example.com/a'b").is_err());
        assert!(validate_https_url("https://example.com/a`b").is_err());
        assert!(validate_https_url("https://example.com/a\nb").is_err());
        assert!(validate_https_url("https://example.com/a\0b").is_err());

        let too_long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(validate_https_url(&too_long).is_err());

        assert!(validate_loopback_url("http://127.0.0.1:24802").is_ok());
        assert!(validate_loopback_url("http://localhost:24802/").is_ok());
        assert!(validate_loopback_url("http://[::1]:24802/").is_ok());
        assert!(validate_loopback_url("http://127.0.0.1:0").is_err());
        assert!(validate_loopback_url("http://127.0.0.1:24802@evil.example").is_err());
        assert!(validate_loopback_url("http://evil.example:24802").is_err());
        assert!(validate_loopback_url("http://127.0.0.1:24802/a\"&calc").is_err());
    }
}
