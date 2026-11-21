//! Step 10: Zero-friction OS integration helpers (Autostart, Windows Firewall,
//! Windows Elevated Task helper, and safe file-drop destination folder).

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Resolves a custom or default file-receive directory without modifying the filesystem.
///
/// - `None` resolves to `~/Downloads/Glide` (or `%USERPROFILE%\Downloads\Glide` on Windows).
/// - `Some(raw)` validates that the path is non-empty and free of NUL bytes, and safely expands
///   a leading `~`, `~/`, or `~\` against the user's home directory.
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

    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        let home = user_home_dir()
            .ok_or_else(|| anyhow!("cannot expand '~/': user home directory is unavailable"))?;
        if rest.is_empty() {
            return Ok(home);
        }
        return Ok(home.join(rest));
    }

    Ok(PathBuf::from(trimmed))
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

/// Opens a file or folder in the user's OS default handler / file manager without invoking a shell.
pub fn open_path_in_os(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("path does not exist: {}", path.display());
    }

    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to open {} with xdg-open", path.display()))?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("explorer.exe")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to open {} with explorer.exe", path.display()))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to open {} with open", path.display()))?;
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = path;
        anyhow::bail!("opening local paths is unsupported on this OS")
    }
}

/// Reveals a file in the OS file manager (or opens the folder directly if `path` is a directory).
pub fn reveal_in_file_manager(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("path does not exist: {}", path.display());
    }
    if path.is_dir() {
        return open_path_in_os(path);
    }

    #[cfg(target_os = "linux")]
    {
        let target = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(path);
        Command::new("xdg-open")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to reveal {} with xdg-open", target.display()))?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("explorer.exe")
            .arg("/select,")
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to reveal {} with explorer.exe", path.display()))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg("-R")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to reveal {} with open", path.display()))?;
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = path;
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

        // Explicit custom path is trimmed and preserved
        let custom = resolve_receive_dir(Some("  /tmp/glide-inbox  ")).expect("custom dir");
        assert_eq!(custom, PathBuf::from("/tmp/glide-inbox"));

        // Tilde expansion against user home
        if let Some(home) = user_home_dir() {
            assert_eq!(resolve_receive_dir(Some("~")).unwrap(), home);
            assert_eq!(
                resolve_receive_dir(Some("~/Transfers/Glide")).unwrap(),
                home.join("Transfers/Glide")
            );
            assert_eq!(resolve_receive_dir(Some("~/")).unwrap(), home);
        }

        // Empty, whitespace-only, and NUL-containing paths are rejected
        assert!(resolve_receive_dir(Some("")).is_err());
        assert!(resolve_receive_dir(Some("   \t  ")).is_err());
        assert!(resolve_receive_dir(Some("/tmp/bad\0path")).is_err());
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
