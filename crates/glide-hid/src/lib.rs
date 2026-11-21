//! Physical USB HID usage definitions and lossless cross-OS scancode mappings.
//!
//! Glide transmits physical hardware key identities (USB HID Usage Page + Usage ID)
//! rather than OS-translated keysyms. This ensures international layouts (such as
//! German QWERTZ with `AltGr`, dead keys, umlauts, and the ISO `< > |` key) behave
//! identically to plugging a physical USB keyboard into the target machine.

mod tables;

use serde::{Deserialize, Serialize};
use std::fmt;
use tables::KEY_TABLE;

/// USB HID Usage Page for Keyboard/Keypad.
pub const PAGE_KEYBOARD: u16 = 0x0007;
/// USB HID Usage Page for Consumer Control (media keys).
pub const PAGE_CONSUMER: u16 = 0x000C;

/// Offset between Linux evdev keycodes and X11/XKB keycodes under Xorg evdev/libinput.
pub const X11_EVDEV_OFFSET: u8 = 8;

/// A canonical USB HID usage packed as `(usage_page << 16) | usage_id`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HidUsage(pub u32);

impl HidUsage {
    /// USB HID Usage Page for Keyboard/Keypad (`0x07`).
    pub const PAGE_KEYBOARD: u16 = PAGE_KEYBOARD;
    /// USB HID Usage Page for Consumer Control / Media keys (`0x0C`).
    pub const PAGE_CONSUMER: u16 = PAGE_CONSUMER;

    pub const fn new(page: u16, id: u16) -> Self {
        Self(((page as u32) << 16) | (id as u32))
    }

    pub const fn keyboard(id: u16) -> Self {
        Self::new(PAGE_KEYBOARD, id)
    }

    pub const fn consumer(id: u16) -> Self {
        Self::new(PAGE_CONSUMER, id)
    }

    pub const fn page(self) -> u16 {
        (self.0 >> 16) as u16
    }

    pub const fn id(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Returns `true` if this usage belongs to USB HID Consumer Control page (`0x0C`:
    /// volume, mute, play/pause, next/prev track, stop).
    #[inline]
    pub const fn is_media_key(self) -> bool {
        self.page() == Self::PAGE_CONSUMER
    }

    // Well-known keys used across crates
    pub const ESCAPE: Self = Self::keyboard(0x29);
    pub const SCROLL_LOCK: Self = Self::keyboard(0x47);
    pub const PAUSE: Self = Self::keyboard(0x48);
    pub const ISO_BACKSLASH: Self = Self::keyboard(0x64);
    pub const CONTROL_LEFT: Self = Self::keyboard(0xE0);
    pub const SHIFT_LEFT: Self = Self::keyboard(0xE1);
    pub const ALT_LEFT: Self = Self::keyboard(0xE2);
    pub const META_LEFT: Self = Self::keyboard(0xE3);
    pub const CONTROL_RIGHT: Self = Self::keyboard(0xE4);
    pub const SHIFT_RIGHT: Self = Self::keyboard(0xE5);
    pub const ALT_RIGHT: Self = Self::keyboard(0xE6);
    pub const META_RIGHT: Self = Self::keyboard(0xE7);

    /// Normalize ISO Non-US `#/~` (0x32) to Backslash (0x31), since Windows Set 1
    /// (`0x2B`) and Linux evdev (`KEY_BACKSLASH = 43`) both unify them.
    pub const fn canonicalize(self) -> Self {
        if self.0 == Self::keyboard(0x32).0 {
            Self::keyboard(0x31)
        } else {
            self
        }
    }

    /// Look up by Linux evdev keycode (from `<linux/input-event-codes.h>`).
    pub fn from_evdev(evdev: u16) -> Option<Self> {
        KEY_TABLE.iter().find(|e| e.evdev == evdev).map(|e| e.hid)
    }

    /// Convert to Linux evdev keycode.
    pub fn to_evdev(self) -> Option<u16> {
        let canon = self.canonicalize();
        KEY_TABLE.iter().find(|e| e.hid == canon).map(|e| e.evdev)
    }

    /// Look up from an X11 keycode (evdev + 8).
    pub fn from_x11_keycode(keycode: u8) -> Option<Self> {
        let evdev = keycode.checked_sub(X11_EVDEV_OFFSET)? as u16;
        Self::from_evdev(evdev)
    }

    /// Convert to an X11 keycode (evdev + 8), if it fits in X11's 8-bit range.
    pub fn to_x11_keycode(self) -> Option<u8> {
        let evdev = self.to_evdev()?;
        let x11 = evdev.checked_add(X11_EVDEV_OFFSET as u16)?;
        u8::try_from(x11).ok()
    }

    /// Look up from a Windows Set 1 scan code + E0 extended flag.
    pub fn from_win_scan(scan: WinScan) -> Option<Self> {
        KEY_TABLE
            .iter()
            .find(|e| e.win == Some(scan))
            .map(|e| e.hid)
    }

    /// Convert to a Windows Set 1 scan code + E0 extended flag.
    pub fn to_win_scan(self) -> Option<WinScan> {
        let canon = self.canonicalize();
        KEY_TABLE.iter().find(|e| e.hid == canon).and_then(|e| e.win)
    }

    /// Convert to macOS virtual keycode (for future macOS backend).
    pub fn to_macos_keycode(self) -> Option<u16> {
        let canon = self.canonicalize();
        KEY_TABLE
            .iter()
            .find(|e| e.hid == canon)
            .and_then(|e| e.macos)
    }

    /// Parse a key by case-insensitive canonical name (e.g. `"ScrollLock"`, `"AltRight"`).
    pub fn from_name(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        // Helpful common aliases
        if trimmed.eq_ignore_ascii_case("AltGr") {
            return Some(Self::ALT_RIGHT);
        }
        if trimmed.eq_ignore_ascii_case("CtrlLeft") || trimmed.eq_ignore_ascii_case("LCtrl") {
            return Some(Self::CONTROL_LEFT);
        }
        if trimmed.eq_ignore_ascii_case("CtrlRight") || trimmed.eq_ignore_ascii_case("RCtrl") {
            return Some(Self::CONTROL_RIGHT);
        }
        KEY_TABLE
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(trimmed))
            .map(|e| e.hid)
    }

    /// Human-readable canonical key name, if known.
    pub fn name(self) -> Option<&'static str> {
        let canon = self.canonicalize();
        KEY_TABLE.iter().find(|e| e.hid == canon).map(|e| e.name)
    }

    /// Returns the bit in [`ModifierMask`] corresponding to this key, if it is a modifier.
    pub const fn modifier_bit(self) -> u8 {
        match self.0 {
            x if x == Self::CONTROL_LEFT.0 => ModifierMask::CONTROL_LEFT,
            x if x == Self::SHIFT_LEFT.0 => ModifierMask::SHIFT_LEFT,
            x if x == Self::ALT_LEFT.0 => ModifierMask::ALT_LEFT,
            x if x == Self::META_LEFT.0 => ModifierMask::META_LEFT,
            x if x == Self::CONTROL_RIGHT.0 => ModifierMask::CONTROL_RIGHT,
            x if x == Self::SHIFT_RIGHT.0 => ModifierMask::SHIFT_RIGHT,
            x if x == Self::ALT_RIGHT.0 => ModifierMask::ALT_RIGHT,
            x if x == Self::META_RIGHT.0 => ModifierMask::META_RIGHT,
            _ => 0,
        }
    }
}

impl fmt::Debug for HidUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.name() {
            write!(
                f,
                "HidUsage({name}, 0x{:04X}:0x{:04X})",
                self.page(),
                self.id()
            )
        } else {
            write!(f, "HidUsage(0x{:04X}:0x{:04X})", self.page(), self.id())
        }
    }
}

impl fmt::Display for HidUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.name() {
            f.write_str(name)
        } else {
            write!(f, "HID(0x{:04X}:0x{:04X})", self.page(), self.id())
        }
    }
}

/// Windows Set 1 keyboard scan code plus E0 extended flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WinScan {
    pub code: u16,
    pub extended: bool,
}

/// Detects the synthetic fake `VK_LCONTROL` (`scanCode == 0x21D`) that Windows
/// injects immediately before `VK_RMENU` whenever `AltGr` is pressed on German
/// and other European keyboard layouts.
///
/// Forwarding this synthetic key to Linux/macOS breaks `AltGr` combinations
/// (e.g. `AltGr+Q` becomes `Ctrl+AltGr+Q` instead of `@`). Filtering it on the
/// Windows server ensures only physical `AltRight` (`0xE6`) crosses the wire.
#[inline]
pub const fn is_windows_fake_altgr_lctrl(vk_code: u32, scan_code: u32) -> bool {
    const VK_LCONTROL: u32 = 0xA2;
    vk_code == VK_LCONTROL && (scan_code == 0x21D || (scan_code & 0x200) != 0)
}

/// Bitmask of the 8 physical USB HID modifier keys, matching USB HID boot report order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifierMask(pub u8);

impl ModifierMask {
    pub const CONTROL_LEFT: u8 = 1 << 0;
    pub const SHIFT_LEFT: u8 = 1 << 1;
    pub const ALT_LEFT: u8 = 1 << 2;
    pub const META_LEFT: u8 = 1 << 3;
    pub const CONTROL_RIGHT: u8 = 1 << 4;
    pub const SHIFT_RIGHT: u8 = 1 << 5;
    pub const ALT_RIGHT: u8 = 1 << 6;
    pub const META_RIGHT: u8 = 1 << 7;

    pub const EMPTY: Self = Self(0);

    pub fn update(&mut self, key: HidUsage, pressed: bool) {
        let bit = key.modifier_bit();
        if bit != 0 {
            if pressed {
                self.0 |= bit;
            } else {
                self.0 &= !bit;
            }
        }
    }

    pub const fn contains(self, bit: u8) -> bool {
        (self.0 & bit) != 0
    }

    pub const fn ctrl(self) -> bool {
        self.contains(Self::CONTROL_LEFT) || self.contains(Self::CONTROL_RIGHT)
    }

    pub const fn shift(self) -> bool {
        self.contains(Self::SHIFT_LEFT) || self.contains(Self::SHIFT_RIGHT)
    }

    pub const fn alt(self) -> bool {
        self.contains(Self::ALT_LEFT) || self.contains(Self::ALT_RIGHT)
    }
}

/// Physical mouse buttons transmitted over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum MouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    Back = 4,    // XButton1 / X11 button 8
    Forward = 5, // XButton2 / X11 button 9
}

impl MouseButton {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Left),
            2 => Some(Self::Right),
            3 => Some(Self::Middle),
            4 => Some(Self::Back),
            5 => Some(Self::Forward),
            _ => None,
        }
    }

    /// Convert to X11 button number (1=Left, 2=Middle, 3=Right, 8=Back, 9=Forward).
    pub const fn to_x11_button(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
            Self::Back => 8,
            Self::Forward => 9,
        }
    }

    /// Convert from X11 button number.
    pub const fn from_x11_button(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Left),
            2 => Some(Self::Middle),
            3 => Some(Self::Right),
            8 => Some(Self::Back),
            9 => Some(Self::Forward),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_is_strictly_bijective() {
        let mut hids = HashSet::new();
        let mut evdevs = HashSet::new();
        let mut wins = HashSet::new();
        let mut names = HashSet::new();

        for entry in KEY_TABLE {
            assert!(hids.insert(entry.hid), "Duplicate HID: {:?}", entry.hid);
            assert!(
                evdevs.insert(entry.evdev),
                "Duplicate evdev {} for {}",
                entry.evdev,
                entry.name
            );
            if let Some(win) = entry.win {
                assert!(
                    wins.insert(win),
                    "Duplicate WinScan {:?} for {}",
                    win,
                    entry.name
                );
            }
            assert!(names.insert(entry.name), "Duplicate name {}", entry.name);

            // Round-trip checks
            assert_eq!(HidUsage::from_evdev(entry.evdev), Some(entry.hid));
            assert_eq!(entry.hid.to_evdev(), Some(entry.evdev));
            if let Some(win) = entry.win {
                assert_eq!(HidUsage::from_win_scan(win), Some(entry.hid));
                assert_eq!(entry.hid.to_win_scan(), Some(win));
            } else {
                assert_eq!(entry.hid.to_win_scan(), None);
            }
        }
    }

    #[test]
    fn german_iso_keys_roundtrip_win_and_x11() {
        // AltGr: Win E0 38 <-> HID 0xE6 <-> evdev 100 <-> X11 108
        let altgr_win = WinScan {
            code: 0x38,
            extended: true,
        };
        let hid = HidUsage::from_win_scan(altgr_win).unwrap();
        assert_eq!(hid, HidUsage::ALT_RIGHT);
        assert_eq!(hid.to_x11_keycode(), Some(108));
        assert_eq!(HidUsage::from_x11_keycode(108), Some(HidUsage::ALT_RIGHT));
        assert_eq!(HidUsage::ALT_RIGHT.to_win_scan(), Some(altgr_win));

        // ISO < > | key: Win 0x56 <-> HID 0x64 <-> evdev 86 (KEY_102ND) <-> X11 94
        let iso_win = WinScan {
            code: 0x56,
            extended: false,
        };
        let iso = HidUsage::from_win_scan(iso_win).unwrap();
        assert_eq!(iso, HidUsage::ISO_BACKSLASH);
        assert_eq!(iso.to_x11_keycode(), Some(94));
        assert_eq!(
            HidUsage::from_x11_keycode(94),
            Some(HidUsage::ISO_BACKSLASH)
        );

        // German #/' key (Non-US Hash 0x32 canonicalizes to 0x31 -> Win 0x2B / X11 51)
        assert_eq!(
            HidUsage::keyboard(0x32).to_win_scan(),
            Some(WinScan {
                code: 0x2B,
                extended: false
            })
        );
        assert_eq!(HidUsage::keyboard(0x32).to_x11_keycode(), Some(51));

        // Right Shift must NOT have extended flag on Windows
        let rshift = HidUsage::SHIFT_RIGHT.to_win_scan().unwrap();
        assert_eq!(
            rshift,
            WinScan {
                code: 0x36,
                extended: false
            }
        );
        assert_eq!(HidUsage::SHIFT_RIGHT.to_x11_keycode(), Some(62));
    }

    #[test]
    fn filters_windows_fake_altgr_lctrl_only() {
        // Fake LCtrl injected by Windows German layout on AltGr press
        assert!(is_windows_fake_altgr_lctrl(0xA2, 0x21D));
        // Real physical Left Ctrl
        assert!(!is_windows_fake_altgr_lctrl(0xA2, 0x1D));
        // Real physical Right Alt
        assert!(!is_windows_fake_altgr_lctrl(0xA5, 0x38));
    }

    #[test]
    fn classifies_consumer_media_and_brightness_keys() {
        for name in [
            "AudioVolumeMute",
            "AudioVolumeDown",
            "AudioVolumeUp",
            "MediaPlayPause",
            "MediaTrackNext",
            "MediaTrackPrev",
            "MediaStop",
            "BrightnessDown",
            "BrightnessUp",
        ] {
            let u = HidUsage::from_name(name).expect("known media key");
            assert!(u.is_media_key(), "{name} should be classified as media key");
        }
        // Brightness Decrement (0x0070) / Increment (0x006F) roundtrip with evdev 224/225 & X11 232/233
        assert_eq!(HidUsage::from_evdev(224), Some(HidUsage::consumer(0x0070)));
        assert_eq!(HidUsage::from_evdev(225), Some(HidUsage::consumer(0x006F)));
        assert_eq!(HidUsage::from_x11_keycode(232), Some(HidUsage::consumer(0x0070)));
        assert_eq!(HidUsage::from_x11_keycode(233), Some(HidUsage::consumer(0x006F)));
        assert!(HidUsage::consumer(0x006F).is_media_key());
        assert!(HidUsage::consumer(0x0070).is_media_key());

        // Standard keyboard usages are not media keys
        assert!(!HidUsage::ESCAPE.is_media_key());
        assert!(!HidUsage::ALT_RIGHT.is_media_key());
        assert!(!HidUsage::ISO_BACKSLASH.is_media_key());
        assert!(!HidUsage::from_name("KeyA").unwrap().is_media_key());
    }
}
