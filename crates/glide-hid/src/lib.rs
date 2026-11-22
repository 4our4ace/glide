//! Physical USB HID usage definitions and lossless cross-OS scancode mappings.
//!
//! Glide transmits physical hardware key identities (USB HID Usage Page + Usage ID)
//! rather than OS-translated keysyms. This ensures international layouts (such as
//! German QWERTZ with `AltGr`, dead keys, umlauts, and the ISO `< > |` key) behave
//! identically to plugging a physical USB keyboard into the target machine.

mod tables;

use serde::{Deserialize, Serialize};
use std::fmt;
use tables::{KeyEntry, KEY_TABLE};

/// USB HID Usage Page for Keyboard/Keypad.
pub const PAGE_KEYBOARD: u16 = 0x0007;
/// USB HID Usage Page for Consumer Control (media keys).
pub const PAGE_CONSUMER: u16 = 0x000C;

/// Offset between Linux evdev keycodes and X11/XKB keycodes under Xorg evdev/libinput.
pub const X11_EVDEV_OFFSET: u8 = 8;

const _: () = assert!(KEY_TABLE.len() < u8::MAX as usize);

const EVDEV_TO_IDX: [u8; 256] = {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < KEY_TABLE.len() {
        let code = KEY_TABLE[i].evdev as usize;
        if code < lut.len() {
            lut[code] = (i + 1) as u8;
        }
        i += 1;
    }
    lut
};

const WIN_TO_IDX: [[u8; 256]; 2] = {
    let mut lut = [[0u8; 256]; 2];
    let mut i = 0;
    while i < KEY_TABLE.len() {
        if let Some(w) = KEY_TABLE[i].win {
            let ext = if w.extended { 1 } else { 0 };
            let code = w.code as usize;
            if code < 256 {
                lut[ext][code] = (i + 1) as u8;
            }
        }
        i += 1;
    }
    lut
};

const HID_KB_TO_IDX: [u8; 256] = {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < KEY_TABLE.len() {
        let h = KEY_TABLE[i].hid;
        if h.page() == PAGE_KEYBOARD && (h.id() as usize) < 256 {
            lut[h.id() as usize] = (i + 1) as u8;
        }
        i += 1;
    }
    lut
};

const HID_CONSUMER_TO_IDX: [u8; 256] = {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < KEY_TABLE.len() {
        let h = KEY_TABLE[i].hid;
        if h.page() == PAGE_CONSUMER && (h.id() as usize) < 256 {
            lut[h.id() as usize] = (i + 1) as u8;
        }
        i += 1;
    }
    lut
};

#[inline]
fn lookup_by_hid(usage: HidUsage) -> Option<&'static KeyEntry> {
    let canon = usage.canonicalize();
    let id = canon.id() as usize;
    if id < 256 {
        let idx = match canon.page() {
            PAGE_KEYBOARD => HID_KB_TO_IDX[id],
            PAGE_CONSUMER => HID_CONSUMER_TO_IDX[id],
            _ => 0,
        };
        if idx != 0 {
            return Some(&KEY_TABLE[(idx - 1) as usize]);
        }
        if canon.page() == PAGE_KEYBOARD || canon.page() == PAGE_CONSUMER {
            return None;
        }
    }
    KEY_TABLE.iter().find(|e| e.hid == canon)
}

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
    pub const KEY_L: Self = Self::keyboard(0x0F);
    pub const ESCAPE: Self = Self::keyboard(0x29);
    pub const BACKSPACE: Self = Self::keyboard(0x2A);
    pub const PRINT_SCREEN: Self = Self::keyboard(0x46);
    pub const SCROLL_LOCK: Self = Self::keyboard(0x47);
    pub const PAUSE: Self = Self::keyboard(0x48);
    pub const INSERT: Self = Self::keyboard(0x49);
    pub const HOME: Self = Self::keyboard(0x4A);
    pub const PAGE_UP: Self = Self::keyboard(0x4B);
    pub const DELETE: Self = Self::keyboard(0x4C);
    pub const END: Self = Self::keyboard(0x4D);
    pub const PAGE_DOWN: Self = Self::keyboard(0x4E);
    pub const ARROW_RIGHT: Self = Self::keyboard(0x4F);
    pub const ARROW_LEFT: Self = Self::keyboard(0x50);
    pub const ARROW_DOWN: Self = Self::keyboard(0x51);
    pub const ARROW_UP: Self = Self::keyboard(0x52);
    pub const ISO_BACKSLASH: Self = Self::keyboard(0x64);
    pub const CONTEXT_MENU: Self = Self::keyboard(0x65);
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
    #[inline]
    pub fn from_evdev(evdev: u16) -> Option<Self> {
        if (evdev as usize) < EVDEV_TO_IDX.len() {
            let idx = EVDEV_TO_IDX[evdev as usize];
            return if idx != 0 {
                Some(KEY_TABLE[(idx - 1) as usize].hid)
            } else {
                None
            };
        }
        KEY_TABLE.iter().find(|e| e.evdev == evdev).map(|e| e.hid)
    }

    /// Convert to Linux evdev keycode.
    #[inline]
    pub fn to_evdev(self) -> Option<u16> {
        lookup_by_hid(self).map(|e| e.evdev)
    }

    /// Look up from an X11 keycode (evdev + 8).
    #[inline]
    pub fn from_x11_keycode(keycode: u8) -> Option<Self> {
        let evdev = keycode.checked_sub(X11_EVDEV_OFFSET)? as u16;
        Self::from_evdev(evdev)
    }

    /// Convert to an X11 keycode (evdev + 8), if it fits in X11's 8-bit range.
    #[inline]
    pub fn to_x11_keycode(self) -> Option<u8> {
        let evdev = self.to_evdev()?;
        let x11 = evdev.checked_add(X11_EVDEV_OFFSET as u16)?;
        u8::try_from(x11).ok()
    }

    /// Look up from a Windows Set 1 scan code + E0 extended flag.
    #[inline]
    pub fn from_win_scan(scan: WinScan) -> Option<Self> {
        if (scan.code as usize) < 256 {
            let idx = WIN_TO_IDX[usize::from(scan.extended)][scan.code as usize];
            return if idx != 0 {
                Some(KEY_TABLE[(idx - 1) as usize].hid)
            } else {
                None
            };
        }
        KEY_TABLE
            .iter()
            .find(|e| e.win == Some(scan))
            .map(|e| e.hid)
    }

    /// Convert to a Windows Set 1 scan code + E0 extended flag.
    #[inline]
    pub fn to_win_scan(self) -> Option<WinScan> {
        lookup_by_hid(self).and_then(|e| e.win)
    }

    /// Convert to macOS virtual keycode (for future macOS backend).
    #[inline]
    pub fn to_macos_keycode(self) -> Option<u16> {
        lookup_by_hid(self).and_then(|e| e.macos)
    }

    /// Parse a key by case-insensitive canonical name or forgiving alias
    /// (e.g. `"ScrollLock"`, `"Scroll Lock"`, `"L"`, `"Esc"`, `"AltGr"`, `"LWin"`).
    pub fn from_name(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        let bytes = trimmed.as_bytes();
        if bytes.len() == 1 {
            let b = bytes[0].to_ascii_uppercase();
            if b.is_ascii_uppercase() {
                return Some(Self::keyboard(0x04 + u16::from(b - b'A')));
            }
            if b.is_ascii_digit() {
                return Some(if b == b'0' {
                    Self::keyboard(0x27)
                } else {
                    Self::keyboard(0x1E + u16::from(b - b'1'))
                });
            }
        }

        // Forgiving common aliases (case-insensitive)
        if trimmed.eq_ignore_ascii_case("Esc") {
            return Some(Self::ESCAPE);
        }
        if trimmed.eq_ignore_ascii_case("Del") {
            return Some(Self::DELETE);
        }
        if trimmed.eq_ignore_ascii_case("Ins") {
            return Some(Self::INSERT);
        }
        if trimmed.eq_ignore_ascii_case("PgUp") {
            return Some(Self::PAGE_UP);
        }
        if trimmed.eq_ignore_ascii_case("PgDn") {
            return Some(Self::PAGE_DOWN);
        }
        if trimmed.eq_ignore_ascii_case("Scroll Lock") {
            return Some(Self::SCROLL_LOCK);
        }
        if trimmed.eq_ignore_ascii_case("Print Screen") || trimmed.eq_ignore_ascii_case("PrtSc") {
            return Some(Self::PRINT_SCREEN);
        }
        if trimmed.eq_ignore_ascii_case("Left") {
            return Some(Self::ARROW_LEFT);
        }
        if trimmed.eq_ignore_ascii_case("Right") {
            return Some(Self::ARROW_RIGHT);
        }
        if trimmed.eq_ignore_ascii_case("Up") {
            return Some(Self::ARROW_UP);
        }
        if trimmed.eq_ignore_ascii_case("Down") {
            return Some(Self::ARROW_DOWN);
        }
        if trimmed.eq_ignore_ascii_case("SuperLeft")
            || trimmed.eq_ignore_ascii_case("WinLeft")
            || trimmed.eq_ignore_ascii_case("LWin")
            || trimmed.eq_ignore_ascii_case("LSuper")
        {
            return Some(Self::META_LEFT);
        }
        if trimmed.eq_ignore_ascii_case("SuperRight")
            || trimmed.eq_ignore_ascii_case("WinRight")
            || trimmed.eq_ignore_ascii_case("RWin")
            || trimmed.eq_ignore_ascii_case("RSuper")
        {
            return Some(Self::META_RIGHT);
        }
        if trimmed.eq_ignore_ascii_case("CtrlLeft") || trimmed.eq_ignore_ascii_case("LCtrl") {
            return Some(Self::CONTROL_LEFT);
        }
        if trimmed.eq_ignore_ascii_case("CtrlRight") || trimmed.eq_ignore_ascii_case("RCtrl") {
            return Some(Self::CONTROL_RIGHT);
        }
        if trimmed.eq_ignore_ascii_case("LAlt") {
            return Some(Self::ALT_LEFT);
        }
        if trimmed.eq_ignore_ascii_case("RAlt") || trimmed.eq_ignore_ascii_case("AltGr") {
            return Some(Self::ALT_RIGHT);
        }
        if trimmed.eq_ignore_ascii_case("LShift") {
            return Some(Self::SHIFT_LEFT);
        }
        if trimmed.eq_ignore_ascii_case("RShift") {
            return Some(Self::SHIFT_RIGHT);
        }

        KEY_TABLE
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(trimmed))
            .map(|e| e.hid)
    }

    /// Human-readable canonical key name, if known.
    #[inline]
    pub fn name(self) -> Option<&'static str> {
        lookup_by_hid(self).map(|e| e.name)
    }

    /// Clean, short UI keycap text suitable for displaying inside shortcut badges.
    pub fn keycap_label(self) -> &'static str {
        match self.canonicalize() {
            Self::SCROLL_LOCK => "Scroll Lock",
            Self::ESCAPE => "Esc",
            Self::DELETE => "Del",
            Self::BACKSPACE => "Backspace",
            Self::PRINT_SCREEN => "PrtSc",
            Self::PAGE_UP => "PgUp",
            Self::PAGE_DOWN => "PgDn",
            Self::ARROW_LEFT => "Left",
            Self::ARROW_RIGHT => "Right",
            Self::ARROW_UP => "Up",
            Self::ARROW_DOWN => "Down",
            Self::CONTROL_LEFT => "Left Ctrl",
            Self::CONTROL_RIGHT => "Right Ctrl",
            Self::SHIFT_LEFT => "Left Shift",
            Self::SHIFT_RIGHT => "Right Shift",
            Self::ALT_LEFT => "Left Alt",
            Self::ALT_RIGHT => "Right Alt",
            Self::META_LEFT => "Left Super",
            Self::META_RIGHT => "Right Super",
            other => {
                if let Some(name) = other.name() {
                    if let Some(letter) = name.strip_prefix("Key") {
                        return letter;
                    }
                    if let Some(digit) = name.strip_prefix("Digit") {
                        return digit;
                    }
                    name
                } else {
                    "Unknown"
                }
            }
        }
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

    /// Returns `true` if this usage is one of the 8 physical USB HID modifier keys.
    #[inline]
    pub const fn is_modifier(self) -> bool {
        self.modifier_bit() != 0
    }

    /// Returns `true` if this key may be bound standalone (without Ctrl/Alt/Super)
    /// as a lock or escape shortcut (`F1..F24`, `ScrollLock`, `Pause`, `PrintScreen`,
    /// `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `ContextMenu`).
    #[inline]
    pub const fn is_standalone_shortcut_key(self) -> bool {
        if self.page() != PAGE_KEYBOARD {
            return false;
        }
        matches!(
            self.id(),
            0x3A..=0x45 // F1..=F12
                | 0x46  // PrintScreen
                | 0x47  // ScrollLock
                | 0x48  // Pause
                | 0x49  // Insert
                | 0x4A  // Home
                | 0x4B  // PageUp
                | 0x4D  // End
                | 0x4E  // PageDown
                | 0x65  // ContextMenu
                | 0x68..=0x73 // F13..=F24
        )
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

    pub const fn meta(self) -> bool {
        self.contains(Self::META_LEFT) || self.contains(Self::META_RIGHT)
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

    #[test]
    fn from_name_forgiving_aliases_and_single_chars() {
        for ch in 'A'..='Z' {
            let single = ch.to_string();
            let lower = ch.to_ascii_lowercase().to_string();
            let canonical = format!("Key{ch}");
            let expected = HidUsage::from_name(&canonical).unwrap();
            assert_eq!(HidUsage::from_name(&single), Some(expected));
            assert_eq!(HidUsage::from_name(&lower), Some(expected));
            assert_eq!(HidUsage::from_name(&format!("  {single}  ")), Some(expected));
        }
        for d in '0'..='9' {
            let single = d.to_string();
            let canonical = format!("Digit{d}");
            let expected = HidUsage::from_name(&canonical).unwrap();
            assert_eq!(HidUsage::from_name(&single), Some(expected));
        }

        assert_eq!(HidUsage::from_name("L"), Some(HidUsage::KEY_L));
        assert_eq!(HidUsage::from_name("esc"), Some(HidUsage::ESCAPE));
        assert_eq!(HidUsage::from_name("Del"), Some(HidUsage::DELETE));
        assert_eq!(HidUsage::from_name("Ins"), Some(HidUsage::INSERT));
        assert_eq!(HidUsage::from_name("PgUp"), Some(HidUsage::PAGE_UP));
        assert_eq!(HidUsage::from_name("PgDn"), Some(HidUsage::PAGE_DOWN));
        assert_eq!(HidUsage::from_name("Scroll Lock"), Some(HidUsage::SCROLL_LOCK));
        assert_eq!(HidUsage::from_name("Print Screen"), Some(HidUsage::PRINT_SCREEN));
        assert_eq!(HidUsage::from_name("PrtSc"), Some(HidUsage::PRINT_SCREEN));
        assert_eq!(HidUsage::from_name("Left"), Some(HidUsage::ARROW_LEFT));
        assert_eq!(HidUsage::from_name("Right"), Some(HidUsage::ARROW_RIGHT));
        assert_eq!(HidUsage::from_name("Up"), Some(HidUsage::ARROW_UP));
        assert_eq!(HidUsage::from_name("Down"), Some(HidUsage::ARROW_DOWN));
        assert_eq!(HidUsage::from_name("SuperLeft"), Some(HidUsage::META_LEFT));
        assert_eq!(HidUsage::from_name("WinLeft"), Some(HidUsage::META_LEFT));
        assert_eq!(HidUsage::from_name("LWin"), Some(HidUsage::META_LEFT));
        assert_eq!(HidUsage::from_name("LSuper"), Some(HidUsage::META_LEFT));
        assert_eq!(HidUsage::from_name("SuperRight"), Some(HidUsage::META_RIGHT));
        assert_eq!(HidUsage::from_name("WinRight"), Some(HidUsage::META_RIGHT));
        assert_eq!(HidUsage::from_name("RWin"), Some(HidUsage::META_RIGHT));
        assert_eq!(HidUsage::from_name("RSuper"), Some(HidUsage::META_RIGHT));
        assert_eq!(HidUsage::from_name("LAlt"), Some(HidUsage::ALT_LEFT));
        assert_eq!(HidUsage::from_name("RAlt"), Some(HidUsage::ALT_RIGHT));
        assert_eq!(HidUsage::from_name("AltGr"), Some(HidUsage::ALT_RIGHT));
        assert_eq!(HidUsage::from_name("LShift"), Some(HidUsage::SHIFT_LEFT));
        assert_eq!(HidUsage::from_name("RShift"), Some(HidUsage::SHIFT_RIGHT));
        assert_eq!(HidUsage::from_name("LCtrl"), Some(HidUsage::CONTROL_LEFT));
        assert_eq!(HidUsage::from_name("RCtrl"), Some(HidUsage::CONTROL_RIGHT));
        assert_eq!(HidUsage::from_name("Backspace"), Some(HidUsage::BACKSPACE));
        assert_eq!(HidUsage::from_name("NotAKey"), None);
    }

    #[test]
    fn keycap_labels_are_clean_and_human_readable() {
        assert_eq!(HidUsage::KEY_L.keycap_label(), "L");
        assert_eq!(HidUsage::from_name("KeyA").unwrap().keycap_label(), "A");
        assert_eq!(HidUsage::from_name("KeyZ").unwrap().keycap_label(), "Z");
        assert_eq!(HidUsage::from_name("Digit0").unwrap().keycap_label(), "0");
        assert_eq!(HidUsage::from_name("Digit9").unwrap().keycap_label(), "9");
        assert_eq!(HidUsage::SCROLL_LOCK.keycap_label(), "Scroll Lock");
        assert_eq!(HidUsage::ESCAPE.keycap_label(), "Esc");
        assert_eq!(HidUsage::DELETE.keycap_label(), "Del");
        assert_eq!(HidUsage::BACKSPACE.keycap_label(), "Backspace");
        assert_eq!(HidUsage::PRINT_SCREEN.keycap_label(), "PrtSc");
        assert_eq!(HidUsage::PAGE_UP.keycap_label(), "PgUp");
        assert_eq!(HidUsage::PAGE_DOWN.keycap_label(), "PgDn");
        assert_eq!(HidUsage::ARROW_LEFT.keycap_label(), "Left");
        assert_eq!(HidUsage::ARROW_RIGHT.keycap_label(), "Right");
        assert_eq!(HidUsage::ARROW_UP.keycap_label(), "Up");
        assert_eq!(HidUsage::ARROW_DOWN.keycap_label(), "Down");
        assert_eq!(HidUsage::CONTROL_LEFT.keycap_label(), "Left Ctrl");
        assert_eq!(HidUsage::CONTROL_RIGHT.keycap_label(), "Right Ctrl");
        assert_eq!(HidUsage::from_name("F1").unwrap().keycap_label(), "F1");
        assert_eq!(HidUsage::from_name("F12").unwrap().keycap_label(), "F12");
        assert_eq!(HidUsage::from_name("F24").unwrap().keycap_label(), "F24");
        assert_eq!(HidUsage::PAUSE.keycap_label(), "Pause");
        assert_eq!(HidUsage(0x00FF_00FF).keycap_label(), "Unknown");
    }

    #[test]
    fn classifies_modifiers_and_standalone_shortcut_keys() {
        for m in [
            HidUsage::CONTROL_LEFT,
            HidUsage::CONTROL_RIGHT,
            HidUsage::SHIFT_LEFT,
            HidUsage::SHIFT_RIGHT,
            HidUsage::ALT_LEFT,
            HidUsage::ALT_RIGHT,
            HidUsage::META_LEFT,
            HidUsage::META_RIGHT,
        ] {
            assert!(m.is_modifier());
            assert!(!m.is_standalone_shortcut_key());
        }

        for n in 1..=24 {
            let f = HidUsage::from_name(&format!("F{n}")).unwrap();
            assert!(!f.is_modifier());
            assert!(f.is_standalone_shortcut_key(), "F{n} should be standalone");
        }
        for k in [
            HidUsage::SCROLL_LOCK,
            HidUsage::PAUSE,
            HidUsage::PRINT_SCREEN,
            HidUsage::INSERT,
            HidUsage::HOME,
            HidUsage::END,
            HidUsage::PAGE_UP,
            HidUsage::PAGE_DOWN,
            HidUsage::CONTEXT_MENU,
        ] {
            assert!(!k.is_modifier());
            assert!(k.is_standalone_shortcut_key(), "{k} should be standalone");
        }

        // Ordinary typing / editing / media keys are NOT standalone shortcut keys
        for k in [
            HidUsage::KEY_L,
            HidUsage::ESCAPE,
            HidUsage::DELETE,
            HidUsage::BACKSPACE,
            HidUsage::ARROW_LEFT,
            HidUsage::from_name("Space").unwrap(),
            HidUsage::from_name("AudioVolumeUp").unwrap(),
        ] {
            assert!(!k.is_modifier());
            assert!(!k.is_standalone_shortcut_key(), "{k} must not be standalone");
        }
    }
}
