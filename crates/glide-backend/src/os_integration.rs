//! Step 10: Zero-friction OS integration helpers (Autostart, Windows Firewall,
//! Windows Elevated Task helper, and safe file-drop destination folder).

use anyhow::{anyhow, Context, Result};
use std::path::{Component, Path, PathBuf};

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
        let desktop = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Glide KVM\n\
             Comment=Ultra-low-latency software KVM\n\
             Exec=\"{}\" {}\n\
             Icon=glide\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exe.display(),
            args
        );
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

/// Opens an elevated PowerShell prompt on Windows to allow UDP ports 24800 & 24801
/// from local LAN / direct-link subnets across Private, Domain, and Public/Unidentified profiles.
pub fn configure_firewall() -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let ps_cmd = "Remove-NetFirewallRule -DisplayName 'Glide KVM (QUIC + Discovery)' -ErrorAction SilentlyContinue; \
                      New-NetFirewallRule -DisplayName 'Glide KVM (QUIC + Discovery)' -Direction Inbound -Protocol UDP -LocalPort 24800,24801 -RemoteAddress LocalSubnet -Action Allow -Profile Any";
        let status = Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-NoProfile",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &format!(
                    "Start-Process powershell -Verb RunAs -WindowStyle Hidden -Wait -ArgumentList '-NoProfile -WindowStyle Hidden -Command \"{ps_cmd}\"'"
                ),
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
        Ok("On Ubuntu, if UFW is enabled, run: sudo ufw allow 24800:24801/udp".into())
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
        let ps_cmd = format!(
            "$action = New-ScheduledTaskAction -Execute '{}' -Argument 'gui --minimized'; \
             $trigger = New-ScheduledTaskTrigger -AtLogOn; \
             $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest; \
             Register-ScheduledTask -TaskName 'GlideKVM' -Action $action -Trigger $trigger -Principal $principal -Force",
            exe.display()
        );
        let status = Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-NoProfile",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &format!(
                    "Start-Process powershell -Verb RunAs -WindowStyle Hidden -Wait -ArgumentList '-NoProfile -WindowStyle Hidden -Command \"{ps_cmd}\"'"
                ),
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

#[cfg(test)]
mod tests {
    use super::*;

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
