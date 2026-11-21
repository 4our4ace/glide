//! Step 10: Zero-friction OS integration helpers (Autostart, Windows Firewall,
//! Windows Elevated Task helper, and safe file-drop destination folder).

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Maximum URL length accepted by [`open_url`] and [`open_loopback_url`].
pub const MAX_URL_LEN: usize = 2048;

/// Returns (and creates if needed) the destination folder for received files:
/// `~/Downloads/Glide` on Linux or `%USERPROFILE%\Downloads\Glide` on Windows.
pub fn file_receive_dir() -> Result<PathBuf> {
    let base = if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Downloads")
    } else if let Ok(profile) = std::env::var("USERPROFILE") {
        PathBuf::from(profile).join("Downloads")
    } else {
        PathBuf::from(".")
    };
    let dir = base.join("Glide");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create receive directory {}", dir.display()))?;
    Ok(dir)
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
