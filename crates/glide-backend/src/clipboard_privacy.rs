//! Cross-platform password-manager / sensitive clipboard hint detection.
//!
//! Password managers (1Password, Bitwarden, KeePassXC, KeePass, etc.) mark copied
//! secrets with platform-specific clipboard format/MIME hints so clipboard sync tools
//! and clipboard history managers never read, store, or transmit credentials:
//! - Windows: `ExcludeClipboardContentFromMonitorProcessing`,
//!   `CanIncludeInClipboardHistory` (DWORD `0`), and `Clipboard Viewer Ignore`.
//! - Linux (X11 & Wayland): `x-kde-passwordManagerHint`.

/// Standard freedesktop / KDE / password-manager MIME type & X11 target atom name.
pub const PASSWORD_MANAGER_HINT_MIME: &str = "x-kde-passwordManagerHint";

/// Windows clipboard format: presence alone marks content excluded from monitors/sync.
pub const WIN_EXCLUDE_MONITOR_FORMAT: &str = "ExcludeClipboardContentFromMonitorProcessing";

/// Windows clipboard format: DWORD `0` forbids clipboard history / sensitive sync.
pub const WIN_CAN_INCLUDE_HISTORY_FORMAT: &str = "CanIncludeInClipboardHistory";

/// Legacy Windows privacy marker used by KeePass and clipboard managers.
pub const WIN_CLIPBOARD_VIEWER_IGNORE_FORMAT: &str = "Clipboard Viewer Ignore";

/// Snapshot of Windows clipboard privacy markers checked before reading clipboard content.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsPrivacyCheck<'a> {
    /// Whether `ExcludeClipboardContentFromMonitorProcessing` is available on the clipboard.
    pub has_exclude_monitor: bool,
    /// Whether `Clipboard Viewer Ignore` is available on the clipboard.
    pub has_viewer_ignore: bool,
    /// `None` if `CanIncludeInClipboardHistory` is NOT on the clipboard;
    /// `Some(None)` if the format is present with a null/unreadable handle;
    /// `Some(Some(bytes))` if the format is present with raw payload bytes.
    pub can_include_in_history: Option<Option<&'a [u8]>>,
}

/// Returns `true` if Windows privacy clipboard formats indicate the current clipboard
/// content is sensitive (e.g. copied from a password manager) and must NOT be synced
/// or recorded in history.
pub fn is_windows_clipboard_sensitive(check: WindowsPrivacyCheck<'_>) -> bool {
    if check.has_exclude_monitor || check.has_viewer_ignore {
        return true;
    }
    if let Some(raw_opt) = check.can_include_in_history {
        return match raw_opt {
            // Format present with unreadable/empty payload: fail closed as excluded.
            None | Some([]) => true,
            // Standard Win32 DWORD (4-byte little-endian; GlobalSize may round up >= 4).
            Some(bytes) if bytes.len() >= 4 => {
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) == 0
            }
            // Short payload (< 4 bytes): exclude if all zero bytes.
            Some(bytes) => bytes.iter().all(|&b| b == 0),
        };
    }
    false
}

/// Returns `true` if offered Wayland/MIME clipboard types contain a password-manager hint.
pub fn has_sensitive_mime_type<S: AsRef<str>>(mime_types: &[S]) -> bool {
    mime_types
        .iter()
        .any(|m| m.as_ref().eq_ignore_ascii_case(PASSWORD_MANAGER_HINT_MIME))
}

/// Returns `true` if offered X11 `TARGETS` atoms contain the interned `x-kde-passwordManagerHint` atom.
pub fn has_sensitive_x11_target(offered_targets: &[u32], password_manager_hint_atom: u32) -> bool {
    password_manager_hint_atom != 0 && offered_targets.contains(&password_manager_hint_atom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_exclude_monitor_and_viewer_ignore_flag_sensitive() {
        assert!(!is_windows_clipboard_sensitive(WindowsPrivacyCheck::default()));

        assert!(is_windows_clipboard_sensitive(WindowsPrivacyCheck {
            has_exclude_monitor: true,
            ..Default::default()
        }));

        assert!(is_windows_clipboard_sensitive(WindowsPrivacyCheck {
            has_viewer_ignore: true,
            ..Default::default()
        }));
    }

    #[test]
    fn windows_can_include_in_history_handles_zero_nonzero_and_overallocated_dword() {
        // Explicit DWORD 0 -> sensitive
        let zero = 0u32.to_le_bytes();
        assert!(is_windows_clipboard_sensitive(WindowsPrivacyCheck {
            can_include_in_history: Some(Some(&zero)),
            ..Default::default()
        }));

        // Over-allocated GlobalSize buffer (8 bytes) starting with DWORD 0 -> sensitive
        let padded_zero = [0u8, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(is_windows_clipboard_sensitive(WindowsPrivacyCheck {
            can_include_in_history: Some(Some(&padded_zero)),
            ..Default::default()
        }));

        // Explicit DWORD 1 -> allowed (not sensitive)
        let one = 1u32.to_le_bytes();
        assert!(!is_windows_clipboard_sensitive(WindowsPrivacyCheck {
            can_include_in_history: Some(Some(&one)),
            ..Default::default()
        }));

        // Present with NULL/empty payload -> fail closed as sensitive
        assert!(is_windows_clipboard_sensitive(WindowsPrivacyCheck {
            can_include_in_history: Some(None),
            ..Default::default()
        }));
        assert!(is_windows_clipboard_sensitive(WindowsPrivacyCheck {
            can_include_in_history: Some(Some(&[])),
            ..Default::default()
        }));

        // ExcludeMonitor takes precedence even when CanIncludeInClipboardHistory == 1
        assert!(is_windows_clipboard_sensitive(WindowsPrivacyCheck {
            has_exclude_monitor: true,
            can_include_in_history: Some(Some(&one)),
            ..Default::default()
        }));
    }

    #[test]
    fn linux_mime_and_x11_target_hints_detect_password_managers() {
        assert!(!has_sensitive_mime_type(&[
            "text/plain;charset=utf-8",
            "UTF8_STRING",
            "image/png",
        ]));
        assert!(has_sensitive_mime_type(&[
            "text/plain;charset=utf-8",
            "x-kde-passwordManagerHint",
        ]));
        assert!(has_sensitive_mime_type(&["X-KDE-PASSWORDMANAGERHINT"]));

        assert!(!has_sensitive_x11_target(&[31, 42, 99], 512));
        assert!(has_sensitive_x11_target(&[31, 512, 99], 512));
        // Atom 0 (X11 None) must never falsely match
        assert!(!has_sensitive_x11_target(&[0, 31], 0));
    }
}
