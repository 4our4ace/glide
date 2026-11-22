//! Customizable, disableable global capture shortcuts and lock-free hotkey matching.
//!
//! Shared across all OS capture backends (Windows, Linux X11, Linux Wayland), `glide-core`,
//! and `glide-gui`. Re-exports [`HidUsage`] and [`ModifierMask`] so downstream UI crates can
//! inspect/format physical keys without adding a direct dependency on `glide-hid`.

use anyhow::{bail, Result};
pub use glide_hid::{HidUsage, ModifierMask};
use glide_proto::Edge;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// Modifier set for a global shortcut chord.
///
/// [`Self::ALT`] intentionally matches **only** physical Left Alt ([`ModifierMask::ALT_LEFT`]),
/// NEVER Right Alt / `AltGr` ([`ModifierMask::ALT_RIGHT`]), so international keyboard layouts
/// (e.g. German QWERTZ `@`, `€`, `{`, `[`, `\`, `~`) never collide with Glide shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct HotkeyMods(pub u8);

impl HotkeyMods {
    pub const EMPTY: Self = Self(0);
    pub const CTRL: Self = Self(1 << 0);
    pub const ALT: Self = Self(1 << 1);
    pub const SHIFT: Self = Self(1 << 2);
    pub const SUPER: Self = Self(1 << 3);

    /// Default modifier prefix for direct screen-switching (`Ctrl+Alt+Arrow`).
    pub const DEFAULT_SWITCH: Self = Self(Self::CTRL.0 | Self::ALT.0);

    const ALL_BITS: u8 = Self::CTRL.0 | Self::ALT.0 | Self::SHIFT.0 | Self::SUPER.0;

    #[inline]
    pub const fn bits(self) -> u8 {
        self.0 & Self::ALL_BITS
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.bits() == 0
    }

    #[inline]
    pub const fn ctrl(self) -> bool {
        (self.0 & Self::CTRL.0) != 0
    }

    #[inline]
    pub const fn alt(self) -> bool {
        (self.0 & Self::ALT.0) != 0
    }

    #[inline]
    pub const fn shift(self) -> bool {
        (self.0 & Self::SHIFT.0) != 0
    }

    #[inline]
    pub const fn super_key(self) -> bool {
        (self.0 & Self::SUPER.0) != 0
    }

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self((self.0 | other.0) & Self::ALL_BITS)
    }

    /// Returns `true` if at least one command modifier (`Ctrl`, `Alt`, or `Super`/`Win`) is set.
    #[inline]
    pub const fn has_command_mod(self) -> bool {
        (self.0 & (Self::CTRL.0 | Self::ALT.0 | Self::SUPER.0)) != 0
    }

    /// Converts a held physical [`ModifierMask`] into [`HotkeyMods`], returning `None` if
    /// `AltRight` (`AltGr`) is held.
    #[inline]
    pub const fn from_modifier_mask(mask: ModifierMask) -> Option<Self> {
        if mask.contains(ModifierMask::ALT_RIGHT) {
            return None;
        }
        let mut bits = 0u8;
        if mask.ctrl() {
            bits |= Self::CTRL.0;
        }
        if mask.contains(ModifierMask::ALT_LEFT) {
            bits |= Self::ALT.0;
        }
        if mask.shift() {
            bits |= Self::SHIFT.0;
        }
        if mask.meta() {
            bits |= Self::SUPER.0;
        }
        Some(Self(bits))
    }

    /// Returns `true` iff the held physical modifier mask matches `self` **exactly**.
    ///
    /// Always returns `false` when `AltRight` (`AltGr`) is held.
    #[inline]
    pub const fn matches_exact(self, mask: ModifierMask) -> bool {
        if mask.contains(ModifierMask::ALT_RIGHT) {
            return false;
        }
        mask.ctrl() == self.ctrl()
            && mask.contains(ModifierMask::ALT_LEFT) == self.alt()
            && mask.shift() == self.shift()
            && mask.meta() == self.super_key()
    }

    /// Validates that this modifier set is safe to use for `Switch` (`<Mods>+Arrow`),
    /// rejecting empty or bare-`Shift` prefixes (which would hijack text selection).
    pub const fn validate_for_switch(self) -> std::result::Result<(), &'static str> {
        if self.has_command_mod() {
            Ok(())
        } else if self.is_empty() {
            Err("Screen-switch shortcut requires at least one modifier (Ctrl, Alt, or Super/Win)")
        } else {
            Err("Shift+Arrow is used for text selection; include Ctrl, Alt, or Super/Win")
        }
    }

    /// Parses a modifier combination string such as `"Ctrl+Alt"`, `"Super+Shift"`, or
    /// `"Ctrl+Alt+Arrow"` / `"Ctrl+Alt+Arrows"`.
    ///
    /// Returns `Ok(None)` for `"none"`, `""`, `"off"`, or `"disabled"` (case-insensitive).
    pub fn parse(s: &str) -> Result<Option<Self>> {
        let trimmed = s.trim();
        if is_disabled_keyword(trimmed) {
            return Ok(None);
        }

        let mut tokens: Vec<&str> = Vec::new();
        for part in trimmed.split('+') {
            let tok = part.trim();
            if tok.is_empty() {
                bail!("empty modifier token in shortcut '{s}'");
            }
            tokens.push(tok);
        }

        if tokens
            .last()
            .is_some_and(|t| t.eq_ignore_ascii_case("Arrow") || t.eq_ignore_ascii_case("Arrows"))
        {
            tokens.pop();
        }

        if tokens.is_empty() {
            bail!("shortcut '{s}' requires at least one modifier");
        }

        let mut mods = Self::EMPTY;
        for tok in tokens {
            let Some(m) = parse_mod_token(tok) else {
                bail!("unknown modifier '{tok}' in shortcut '{s}'");
            };
            mods |= m;
        }
        Ok(Some(mods))
    }

    /// Formats the modifier set in canonical config order (`Ctrl+Alt+Shift+Super`).
    pub fn format_canonical(self) -> String {
        let mut parts = Vec::with_capacity(4);
        if self.ctrl() {
            parts.push("Ctrl");
        }
        if self.alt() {
            parts.push("Alt");
        }
        if self.shift() {
            parts.push("Shift");
        }
        if self.super_key() {
            parts.push("Super");
        }
        parts.join("+")
    }

    /// Returns platform-appropriate short keycap labels (`"Win"` on Windows, `"Super"` on Linux).
    pub fn keycap_labels(self, is_windows: bool) -> Vec<&'static str> {
        let mut labels = Vec::with_capacity(4);
        if self.ctrl() {
            labels.push("Ctrl");
        }
        if self.alt() {
            labels.push("Alt");
        }
        if self.shift() {
            labels.push("Shift");
        }
        if self.super_key() {
            labels.push(if is_windows { "Win" } else { "Super" });
        }
        labels
    }
}

impl BitOr for HotkeyMods {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for HotkeyMods {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 = (self.0 | rhs.0) & Self::ALL_BITS;
    }
}

impl fmt::Display for HotkeyMods {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_canonical())
    }
}

/// A full keyboard shortcut chord consisting of optional [`HotkeyMods`] plus a trigger [`HidUsage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HotkeyChord {
    pub mods: HotkeyMods,
    pub key: HidUsage,
}

impl HotkeyChord {
    /// Default shortcut to lock/unlock the cursor on the current screen (`Ctrl+Alt+L`).
    pub const DEFAULT_LOCK: Self = Self {
        mods: HotkeyMods::DEFAULT_SWITCH,
        key: HidUsage::KEY_L,
    };

    /// Default emergency shortcut to return control to the local server desktop (`ScrollLock`).
    pub const DEFAULT_ESCAPE: Self = Self {
        mods: HotkeyMods::EMPTY,
        key: HidUsage::SCROLL_LOCK,
    };

    #[inline]
    pub const fn new(mods: HotkeyMods, key: HidUsage) -> Self {
        Self {
            mods: HotkeyMods(mods.bits()),
            key: key.canonicalize(),
        }
    }

    /// Packs this chord into a non-zero `u64` (`((mods << 32) | key.raw())`) for lock-free atomics.
    #[inline]
    pub const fn pack(self) -> u64 {
        let key_raw = self.key.canonicalize().raw();
        if key_raw == 0 {
            return 0;
        }
        ((self.mods.bits() as u64) << 32) | (key_raw as u64)
    }

    /// Unpacks a chord produced by [`Self::pack`], returning `None` for `0` (disabled).
    #[inline]
    pub const fn unpack(raw: u64) -> Option<Self> {
        let key_raw = (raw & 0xFFFF_FFFF) as u32;
        if key_raw == 0 {
            return None;
        }
        let mods = HotkeyMods(((raw >> 32) as u8) & HotkeyMods::ALL_BITS);
        Some(Self {
            mods,
            key: HidUsage(key_raw),
        })
    }

    /// Parses a chord string such as `"ScrollLock"`, `"Ctrl+Alt+L"`, `"Ctrl+Alt+Escape"`,
    /// or `"F12"`.
    ///
    /// Returns `Ok(None)` for `"none"`, `""`, `"off"`, or `"disabled"` (case-insensitive).
    /// Legacy single-key bindings (including bare modifiers like `"ControlRight"`) parse
    /// cleanly so existing `glide.toml` files remain backwards-compatible; call
    /// [`Self::validate_for_lock_or_escape`] when validating interactive user input.
    pub fn parse(s: &str) -> Result<Option<Self>> {
        let trimmed = s.trim();
        if is_disabled_keyword(trimmed) {
            return Ok(None);
        }

        let mut tokens: Vec<&str> = Vec::new();
        for part in trimmed.split('+') {
            let tok = part.trim();
            if tok.is_empty() {
                bail!("empty key token in shortcut '{s}'");
            }
            tokens.push(tok);
        }

        let Some((&key_tok, mod_toks)) = tokens.split_last() else {
            return Ok(None);
        };

        let mut mods = HotkeyMods::EMPTY;
        for &tok in mod_toks {
            let Some(m) = parse_mod_token(tok) else {
                bail!("unknown modifier '{tok}' in shortcut '{s}'");
            };
            mods |= m;
        }

        let Some(key) = HidUsage::from_name(key_tok) else {
            bail!("unknown key '{key_tok}' in shortcut '{s}'");
        };

        Ok(Some(Self::new(mods, key)))
    }

    /// Formats the chord in canonical config notation (e.g. `"ScrollLock"`, `"Ctrl+Alt+L"`,
    /// `"Ctrl+Alt+Escape"`).
    pub fn format_canonical(self) -> String {
        let key = self.key.canonicalize();
        let key_str = if let Some(name) = key.name() {
            if let Some(letter) = name.strip_prefix("Key") {
                letter.to_owned()
            } else if let Some(digit) = name.strip_prefix("Digit") {
                digit.to_owned()
            } else {
                name.to_owned()
            }
        } else {
            key.to_string()
        };

        if self.mods.is_empty() {
            key_str
        } else {
            format!("{}+{key_str}", self.mods.format_canonical())
        }
    }

    /// Returns platform-appropriate short keycap strings for displaying in UI shortcut pills.
    pub fn keycap_labels(self, is_windows: bool) -> Vec<String> {
        let mut out: Vec<String> = self
            .mods
            .keycap_labels(is_windows)
            .into_iter()
            .map(String::from)
            .collect();
        let key = self.key.canonicalize();
        let key_label = if is_windows && key == HidUsage::META_LEFT {
            "Left Win"
        } else if is_windows && key == HidUsage::META_RIGHT {
            "Right Win"
        } else {
            key.keycap_label()
        };
        out.push(key_label.to_owned());
        out
    }

    /// Returns a human-readable explanation if this chord is reserved by the OS kernel.
    pub const fn reserved_reason(self) -> Option<&'static str> {
        let mods = self.mods.bits();
        let key = self.key.canonicalize().raw();
        if mods == HotkeyMods::SUPER.0 && key == HidUsage::KEY_L.raw() {
            return Some("Win+L is reserved by Windows to lock your PC");
        }
        if mods == (HotkeyMods::CTRL.0 | HotkeyMods::ALT.0) && key == HidUsage::DELETE.raw() {
            return Some("Ctrl+Alt+Delete is reserved by the OS");
        }
        None
    }

    /// Validates that this chord is safe to bind for Lock or Escape in UI / interactive setup.
    pub const fn validate_for_lock_or_escape(self) -> std::result::Result<(), &'static str> {
        if let Some(reason) = self.reserved_reason() {
            return Err(reason);
        }
        if self.key.is_modifier() {
            return Err("Shortcut requires a non-modifier key");
        }
        if self.mods.has_command_mod()
            || (self.mods.is_empty() && self.key.is_standalone_shortcut_key())
        {
            Ok(())
        } else if self.mods.is_empty() {
            Err("Include Ctrl, Alt, or Super/Win (or use a standalone function/navigation key)")
        } else {
            Err("Shift alone cannot be used; include Ctrl, Alt, or Super/Win")
        }
    }
}

impl fmt::Display for HotkeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_canonical())
    }
}

/// Complete set of runtime-configurable global capture shortcuts.
///
/// Any field set to `None` disables that specific shortcut independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaptureHotkeys {
    /// Modifiers held with an Arrow key to jump directly to a neighbor screen (`None` = disabled).
    pub switch_mods: Option<HotkeyMods>,
    /// Shortcut to toggle cursor lock to the current screen (`None` = disabled).
    pub lock: Option<HotkeyChord>,
    /// Emergency shortcut to return control to the local server desktop (`None` = disabled).
    pub escape: Option<HotkeyChord>,
}

impl Default for CaptureHotkeys {
    fn default() -> Self {
        Self {
            switch_mods: Some(HotkeyMods::DEFAULT_SWITCH),
            lock: Some(HotkeyChord::DEFAULT_LOCK),
            escape: Some(HotkeyChord::DEFAULT_ESCAPE),
        }
    }
}

impl CaptureHotkeys {
    /// Returns a configuration with every global shortcut disabled.
    #[inline]
    pub const fn disabled() -> Self {
        Self {
            switch_mods: None,
            lock: None,
            escape: None,
        }
    }

    /// Returns `true` when all three shortcuts are disabled.
    #[inline]
    pub const fn is_all_disabled(self) -> bool {
        self.switch_mods.is_none() && self.lock.is_none() && self.escape.is_none()
    }
}

/// Maps physical USB HID arrow keys to screen [`Edge`] directions.
#[inline]
pub const fn arrow_to_edge(usage: HidUsage) -> Option<Edge> {
    match usage.raw() {
        x if x == HidUsage::ARROW_LEFT.raw() => Some(Edge::Left),
        x if x == HidUsage::ARROW_RIGHT.raw() => Some(Edge::Right),
        x if x == HidUsage::ARROW_UP.raw() => Some(Edge::Top),
        x if x == HidUsage::ARROW_DOWN.raw() => Some(Edge::Bottom),
        _ => None,
    }
}

/// Action triggered by a matched global capture shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    Escape,
    Switch(Edge),
    ToggleLock,
}

/// Result of evaluating a physical key transition against active capture shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyMatch {
    /// Shortcut triggered on fresh key press — swallow locally & do not forward to remote.
    Trigger(HotkeyAction),
    /// Typematic repeat or release of an already-triggered shortcut key — swallow without re-firing.
    SuppressOnly,
    /// Not a shortcut — handle normally.
    PassThrough,
}

/// Lock-free atomic container for live-updated [`CaptureHotkeys`] + temporary UI recording suspend.
///
/// Safe to read directly inside Windows `WH_KEYBOARD_LL` hooks and X11/Wayland capture loops
/// with zero mutex contention or allocations.
#[derive(Debug)]
pub struct AtomicHotkeys {
    switch_mods: AtomicU8,
    lock_chord: AtomicU64,
    escape_chord: AtomicU64,
    suspended: AtomicBool,
}

impl Default for AtomicHotkeys {
    fn default() -> Self {
        Self::new(CaptureHotkeys::default())
    }
}

impl AtomicHotkeys {
    pub fn new(hotkeys: CaptureHotkeys) -> Self {
        Self {
            switch_mods: AtomicU8::new(hotkeys.switch_mods.map_or(0, HotkeyMods::bits)),
            lock_chord: AtomicU64::new(hotkeys.lock.map_or(0, HotkeyChord::pack)),
            escape_chord: AtomicU64::new(hotkeys.escape.map_or(0, HotkeyChord::pack)),
            suspended: AtomicBool::new(false),
        }
    }

    /// Loads the currently effective shortcuts (returns [`CaptureHotkeys::disabled()`] while suspended).
    #[inline]
    pub fn load(&self) -> CaptureHotkeys {
        if self.suspended.load(Ordering::Acquire) {
            return CaptureHotkeys::disabled();
        }
        let sm = self.switch_mods.load(Ordering::Acquire) & HotkeyMods::ALL_BITS;
        CaptureHotkeys {
            switch_mods: (sm != 0).then_some(HotkeyMods(sm)),
            lock: HotkeyChord::unpack(self.lock_chord.load(Ordering::Acquire)),
            escape: HotkeyChord::unpack(self.escape_chord.load(Ordering::Acquire)),
        }
    }

    /// Updates the stored shortcuts atomically without changing the suspended state.
    pub fn store(&self, hotkeys: CaptureHotkeys) {
        self.switch_mods.store(
            hotkeys.switch_mods.map_or(0, HotkeyMods::bits),
            Ordering::Release,
        );
        self.lock_chord
            .store(hotkeys.lock.map_or(0, HotkeyChord::pack), Ordering::Release);
        self.escape_chord.store(
            hotkeys.escape.map_or(0, HotkeyChord::pack),
            Ordering::Release,
        );
    }

    /// Temporarily suspends or resumes shortcut interception (e.g. while recording a shortcut in UI).
    #[inline]
    pub fn set_suspended(&self, suspended: bool) {
        self.suspended.store(suspended, Ordering::Release);
    }

    #[inline]
    pub fn is_suspended(&self) -> bool {
        self.suspended.load(Ordering::Acquire)
    }
}

/// Lock-free state machine that evaluates shortcuts and suppresses typematic repeats and matching
/// key-up releases of triggered shortcut keys so they never leak to local or remote apps.
#[derive(Debug, Default)]
pub struct AtomicHotkeySuppressor(AtomicU32);

impl AtomicHotkeySuppressor {
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Clears any tracked suppressed key (call on OS suspend/resume or session lock/unlock).
    #[inline]
    pub fn clear(&self) {
        self.0.store(0, Ordering::Release);
    }

    /// Evaluates a physical key press/release using `prev_mask` (the modifier mask **before**
    /// applying `usage`).
    #[inline]
    pub fn evaluate(
        &self,
        hotkeys: CaptureHotkeys,
        remote: bool,
        prev_mask: ModifierMask,
        usage: HidUsage,
        pressed: bool,
    ) -> HotkeyMatch {
        let canon = usage.canonicalize();
        let raw = canon.raw();
        let active = self.0.load(Ordering::Acquire);

        if active != 0 && active == raw {
            if pressed {
                return HotkeyMatch::SuppressOnly;
            }
            self.0.store(0, Ordering::Release);
            return HotkeyMatch::SuppressOnly;
        }

        if !pressed || hotkeys.is_all_disabled() {
            return HotkeyMatch::PassThrough;
        }

        // 1. Escape hotkey (only fires while controlling a remote client).
        if remote {
            if let Some(esc) = hotkeys.escape {
                let esc_key = esc.key.canonicalize();
                if canon == esc_key {
                    let matched = if esc.mods.is_empty() {
                        if esc_key == HidUsage::SCROLL_LOCK || esc_key == HidUsage::PAUSE {
                            // Emergency panic escape: always fires regardless of held modifiers.
                            true
                        } else {
                            prev_mask == ModifierMask::EMPTY
                        }
                    } else {
                        esc.mods.matches_exact(prev_mask)
                    };
                    if matched {
                        self.0.store(raw, Ordering::Release);
                        return HotkeyMatch::Trigger(HotkeyAction::Escape);
                    }
                }
            }
        }

        // 2. Direct screen switch (`switch_mods + Arrow`).
        if let Some(sm) = hotkeys.switch_mods {
            if !sm.is_empty() {
                if let Some(edge) = arrow_to_edge(canon) {
                    if sm.matches_exact(prev_mask) {
                        self.0.store(raw, Ordering::Release);
                        return HotkeyMatch::Trigger(HotkeyAction::Switch(edge));
                    }
                }
            }
        }

        // 3. Cursor lock toggle.
        if let Some(lock) = hotkeys.lock {
            if canon == lock.key.canonicalize() && lock.mods.matches_exact(prev_mask) {
                self.0.store(raw, Ordering::Release);
                return HotkeyMatch::Trigger(HotkeyAction::ToggleLock);
            }
        }

        HotkeyMatch::PassThrough
    }
}

fn is_disabled_keyword(trimmed: &str) -> bool {
    trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("none")
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.eq_ignore_ascii_case("disabled")
}

fn parse_mod_token(tok: &str) -> Option<HotkeyMods> {
    if tok.eq_ignore_ascii_case("Ctrl") || tok.eq_ignore_ascii_case("Control") {
        Some(HotkeyMods::CTRL)
    } else if tok.eq_ignore_ascii_case("Alt")
        || tok.eq_ignore_ascii_case("Option")
        || tok.eq_ignore_ascii_case("Opt")
    {
        Some(HotkeyMods::ALT)
    } else if tok.eq_ignore_ascii_case("Shift") {
        Some(HotkeyMods::SHIFT)
    } else if tok.eq_ignore_ascii_case("Super")
        || tok.eq_ignore_ascii_case("Win")
        || tok.eq_ignore_ascii_case("Windows")
        || tok.eq_ignore_ascii_case("Meta")
        || tok.eq_ignore_ascii_case("Cmd")
        || tok.eq_ignore_ascii_case("Command")
    {
        Some(HotkeyMods::SUPER)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_mods_parse_format_and_keycaps() {
        assert_eq!(HotkeyMods::parse("").unwrap(), None);
        assert_eq!(HotkeyMods::parse("  ").unwrap(), None);
        assert_eq!(HotkeyMods::parse("none").unwrap(), None);
        assert_eq!(HotkeyMods::parse("OFF").unwrap(), None);
        assert_eq!(HotkeyMods::parse("Disabled").unwrap(), None);

        assert_eq!(
            HotkeyMods::parse("Ctrl+Alt").unwrap(),
            Some(HotkeyMods::DEFAULT_SWITCH)
        );
        assert_eq!(
            HotkeyMods::parse("Control + Option + Arrow").unwrap(),
            Some(HotkeyMods::DEFAULT_SWITCH)
        );
        assert_eq!(
            HotkeyMods::parse("alt + ctrl + arrows").unwrap(),
            Some(HotkeyMods::DEFAULT_SWITCH)
        );
        assert_eq!(
            HotkeyMods::parse("Win+Shift+Opt+Ctrl").unwrap(),
            Some(HotkeyMods::CTRL | HotkeyMods::ALT | HotkeyMods::SHIFT | HotkeyMods::SUPER)
        );
        assert_eq!(
            HotkeyMods::parse("Cmd+Meta").unwrap(),
            Some(HotkeyMods::SUPER)
        );
        assert_eq!(
            HotkeyMods::parse("Command+Shift").unwrap(),
            Some(HotkeyMods::SUPER | HotkeyMods::SHIFT)
        );

        // Invalid syntax
        assert!(HotkeyMods::parse("Arrow").is_err());
        assert!(HotkeyMods::parse("Ctrl++Alt").is_err());
        assert!(HotkeyMods::parse("Ctrl+Alt+L").is_err());
        assert!(HotkeyMods::parse("Bogus").is_err());

        // Canonical format & keycaps
        assert_eq!(HotkeyMods::DEFAULT_SWITCH.format_canonical(), "Ctrl+Alt");
        let all = HotkeyMods::CTRL | HotkeyMods::ALT | HotkeyMods::SHIFT | HotkeyMods::SUPER;
        assert_eq!(all.format_canonical(), "Ctrl+Alt+Shift+Super");
        assert_eq!(all.keycap_labels(true), vec!["Ctrl", "Alt", "Shift", "Win"]);
        assert_eq!(
            all.keycap_labels(false),
            vec!["Ctrl", "Alt", "Shift", "Super"]
        );

        // Switch validation
        assert!(HotkeyMods::DEFAULT_SWITCH.validate_for_switch().is_ok());
        assert!(HotkeyMods::SUPER.validate_for_switch().is_ok());
        assert!(HotkeyMods::SHIFT.validate_for_switch().is_err());
        assert!(HotkeyMods::EMPTY.validate_for_switch().is_err());
    }

    #[test]
    fn hotkey_mods_exact_matching_and_altgr_rejection() {
        let ctrl_alt = HotkeyMods::DEFAULT_SWITCH;

        // Left/Right Ctrl + Left Alt match exactly
        assert!(ctrl_alt.matches_exact(ModifierMask(
            ModifierMask::CONTROL_LEFT | ModifierMask::ALT_LEFT
        )));
        assert!(ctrl_alt.matches_exact(ModifierMask(
            ModifierMask::CONTROL_RIGHT | ModifierMask::ALT_LEFT
        )));

        // Extra Shift or Super breaks exact match
        assert!(!ctrl_alt.matches_exact(ModifierMask(
            ModifierMask::CONTROL_LEFT | ModifierMask::ALT_LEFT | ModifierMask::SHIFT_LEFT
        )));
        assert!(!ctrl_alt.matches_exact(ModifierMask(
            ModifierMask::CONTROL_LEFT | ModifierMask::ALT_LEFT | ModifierMask::META_LEFT
        )));

        // AltRight (AltGr) is NEVER accepted — even with Ctrl or when matching EMPTY/ALT
        assert!(!ctrl_alt.matches_exact(ModifierMask(
            ModifierMask::CONTROL_LEFT | ModifierMask::ALT_RIGHT
        )));
        assert!(!ctrl_alt.matches_exact(ModifierMask(
            ModifierMask::CONTROL_LEFT | ModifierMask::ALT_LEFT | ModifierMask::ALT_RIGHT
        )));
        assert!(!HotkeyMods::ALT.matches_exact(ModifierMask(ModifierMask::ALT_RIGHT)));
        assert!(!HotkeyMods::EMPTY.matches_exact(ModifierMask(ModifierMask::ALT_RIGHT)));
        assert_eq!(
            HotkeyMods::from_modifier_mask(ModifierMask(ModifierMask::ALT_RIGHT)),
            None
        );
    }

    #[test]
    fn hotkey_chord_parse_format_pack_and_validation() {
        assert_eq!(HotkeyChord::parse("").unwrap(), None);
        assert_eq!(HotkeyChord::parse("none").unwrap(), None);
        assert_eq!(HotkeyChord::parse("off").unwrap(), None);
        assert_eq!(HotkeyChord::parse("disabled").unwrap(), None);

        let esc = HotkeyChord::parse("ScrollLock").unwrap().unwrap();
        assert_eq!(esc, HotkeyChord::DEFAULT_ESCAPE);
        assert_eq!(esc.format_canonical(), "ScrollLock");
        assert_eq!(esc.keycap_labels(true), vec!["Scroll Lock"]);
        assert_eq!(HotkeyChord::unpack(esc.pack()), Some(esc));
        assert!(esc.validate_for_lock_or_escape().is_ok());

        let esc_spaced = HotkeyChord::parse("Scroll Lock").unwrap().unwrap();
        assert_eq!(esc_spaced, HotkeyChord::DEFAULT_ESCAPE);

        let lock = HotkeyChord::parse("Ctrl+Alt+L").unwrap().unwrap();
        assert_eq!(lock, HotkeyChord::DEFAULT_LOCK);
        assert_eq!(lock.format_canonical(), "Ctrl+Alt+L");
        assert_eq!(lock.keycap_labels(true), vec!["Ctrl", "Alt", "L"]);
        assert_eq!(HotkeyChord::unpack(lock.pack()), Some(lock));
        assert!(lock.validate_for_lock_or_escape().is_ok());

        let ctrl_alt_esc = HotkeyChord::parse("Ctrl+Alt+Escape").unwrap().unwrap();
        assert_eq!(ctrl_alt_esc.mods, HotkeyMods::DEFAULT_SWITCH);
        assert_eq!(ctrl_alt_esc.key, HidUsage::ESCAPE);
        assert_eq!(ctrl_alt_esc.format_canonical(), "Ctrl+Alt+Escape");
        assert_eq!(ctrl_alt_esc.keycap_labels(false), vec!["Ctrl", "Alt", "Esc"]);
        assert!(ctrl_alt_esc.validate_for_lock_or_escape().is_ok());

        // Super keycap adapts to OS
        let super_f12 = HotkeyChord::parse("Super+F12").unwrap().unwrap();
        assert_eq!(super_f12.keycap_labels(true), vec!["Win", "F12"]);
        assert_eq!(super_f12.keycap_labels(false), vec!["Super", "F12"]);

        // Pack/unpack zero -> None
        assert_eq!(HotkeyChord::unpack(0), None);

        // Reserved OS chords
        let win_l = HotkeyChord::parse("Win+L").unwrap().unwrap();
        assert_eq!(
            win_l.reserved_reason(),
            Some("Win+L is reserved by Windows to lock your PC")
        );
        assert_eq!(
            win_l.validate_for_lock_or_escape(),
            Err("Win+L is reserved by Windows to lock your PC")
        );

        let cad = HotkeyChord::parse("Ctrl+Alt+Delete").unwrap().unwrap();
        assert_eq!(
            cad.reserved_reason(),
            Some("Ctrl+Alt+Delete is reserved by the OS")
        );
        assert_eq!(
            cad.validate_for_lock_or_escape(),
            Err("Ctrl+Alt+Delete is reserved by the OS")
        );

        // Ctrl+Super+L is NOT bare Win+L, so it is allowed
        let ctrl_win_l = HotkeyChord::parse("Ctrl+Super+L").unwrap().unwrap();
        assert_eq!(ctrl_win_l.reserved_reason(), None);
        assert!(ctrl_win_l.validate_for_lock_or_escape().is_ok());

        // Standalone vs non-standalone vs modifier validation
        assert!(HotkeyChord::parse("F12")
            .unwrap()
            .unwrap()
            .validate_for_lock_or_escape()
            .is_ok());
        assert!(HotkeyChord::parse("Pause")
            .unwrap()
            .unwrap()
            .validate_for_lock_or_escape()
            .is_ok());
        assert!(HotkeyChord::parse("L")
            .unwrap()
            .unwrap()
            .validate_for_lock_or_escape()
            .is_err());
        assert!(HotkeyChord::parse("Shift+L")
            .unwrap()
            .unwrap()
            .validate_for_lock_or_escape()
            .is_err());
        assert!(HotkeyChord::parse("AltGr")
            .unwrap()
            .unwrap()
            .validate_for_lock_or_escape()
            .is_err());
        assert!(HotkeyChord::parse("Ctrl+Alt+AltRight")
            .unwrap()
            .unwrap()
            .validate_for_lock_or_escape()
            .is_err());

        // Legacy bare modifier escape_key (e.g. ControlRight in glide.toml) still parses!
        let legacy_rctrl = HotkeyChord::parse("ControlRight").unwrap().unwrap();
        assert_eq!(legacy_rctrl.mods, HotkeyMods::EMPTY);
        assert_eq!(legacy_rctrl.key, HidUsage::CONTROL_RIGHT);
        assert!(legacy_rctrl.validate_for_lock_or_escape().is_err());

        // Invalid syntax
        assert!(HotkeyChord::parse("Ctrl+").is_err());
        assert!(HotkeyChord::parse("+L").is_err());
        assert!(HotkeyChord::parse("Ctrl+NotAKey").is_err());
        assert!(HotkeyChord::parse("NotAMod+L").is_err());
    }

    #[test]
    fn atomic_hotkeys_store_load_and_suspend() {
        let atom = AtomicHotkeys::new(CaptureHotkeys::default());
        assert!(!atom.is_suspended());
        assert_eq!(atom.load(), CaptureHotkeys::default());

        let custom = CaptureHotkeys {
            switch_mods: Some(HotkeyMods::SUPER | HotkeyMods::SHIFT),
            lock: None,
            escape: Some(HotkeyChord::new(HotkeyMods::CTRL | HotkeyMods::ALT, HidUsage::ESCAPE)),
        };
        atom.store(custom);
        assert_eq!(atom.load(), custom);

        atom.set_suspended(true);
        assert!(atom.is_suspended());
        assert_eq!(atom.load(), CaptureHotkeys::disabled());

        // Storing while suspended preserves suspended state until resumed
        let defaults = CaptureHotkeys::default();
        atom.store(defaults);
        assert_eq!(atom.load(), CaptureHotkeys::disabled());

        atom.set_suspended(false);
        assert!(!atom.is_suspended());
        assert_eq!(atom.load(), defaults);
    }

    #[test]
    fn suppressor_handles_trigger_repeat_release_panic_escape_and_suspend() {
        let sup = AtomicHotkeySuppressor::new();
        let hk = CaptureHotkeys::default();
        let ctrl_alt = ModifierMask(ModifierMask::CONTROL_LEFT | ModifierMask::ALT_LEFT);

        // 1. Switch hotkey press -> Trigger, repeat -> SuppressOnly, release -> SuppressOnly
        assert_eq!(
            sup.evaluate(hk, false, ctrl_alt, HidUsage::ARROW_RIGHT, true),
            HotkeyMatch::Trigger(HotkeyAction::Switch(Edge::Right))
        );
        assert_eq!(
            sup.evaluate(hk, false, ctrl_alt, HidUsage::ARROW_RIGHT, true),
            HotkeyMatch::SuppressOnly
        );
        assert_eq!(
            sup.evaluate(hk, false, ctrl_alt, HidUsage::ARROW_RIGHT, false),
            HotkeyMatch::SuppressOnly
        );
        // Subsequent normal arrow press without modifiers passes through
        assert_eq!(
            sup.evaluate(hk, false, ModifierMask::EMPTY, HidUsage::ARROW_RIGHT, true),
            HotkeyMatch::PassThrough
        );

        // 2. Lock hotkey with AltGr rejected, with Left Alt triggered
        let ctrl_altgr = ModifierMask(ModifierMask::CONTROL_LEFT | ModifierMask::ALT_RIGHT);
        assert_eq!(
            sup.evaluate(hk, true, ctrl_altgr, HidUsage::KEY_L, true),
            HotkeyMatch::PassThrough
        );
        assert_eq!(
            sup.evaluate(hk, true, ctrl_alt, HidUsage::KEY_L, true),
            HotkeyMatch::Trigger(HotkeyAction::ToggleLock)
        );
        assert_eq!(
            sup.evaluate(hk, true, ctrl_alt, HidUsage::KEY_L, false),
            HotkeyMatch::SuppressOnly
        );

        // 3. Bare ScrollLock emergency escape fires in remote mode even with modifiers held,
        //    and does NOT fire in local mode.
        assert_eq!(
            sup.evaluate(hk, false, ModifierMask::EMPTY, HidUsage::SCROLL_LOCK, true),
            HotkeyMatch::PassThrough
        );
        assert_eq!(
            sup.evaluate(hk, true, ctrl_alt, HidUsage::SCROLL_LOCK, true),
            HotkeyMatch::Trigger(HotkeyAction::Escape)
        );
        // Repeat & release suppressed even after remote flips to false!
        assert_eq!(
            sup.evaluate(hk, false, ctrl_alt, HidUsage::SCROLL_LOCK, true),
            HotkeyMatch::SuppressOnly
        );
        assert_eq!(
            sup.evaluate(hk, false, ctrl_alt, HidUsage::SCROLL_LOCK, false),
            HotkeyMatch::SuppressOnly
        );

        // 4. Legacy bare ControlRight escape fires when prev_mask == EMPTY (before applying usage)
        let legacy_hk = CaptureHotkeys {
            escape: Some(HotkeyChord::new(HotkeyMods::EMPTY, HidUsage::CONTROL_RIGHT)),
            ..CaptureHotkeys::disabled()
        };
        assert_eq!(
            sup.evaluate(
                legacy_hk,
                true,
                ModifierMask(ModifierMask::SHIFT_LEFT),
                HidUsage::CONTROL_RIGHT,
                true
            ),
            HotkeyMatch::PassThrough
        );
        assert_eq!(
            sup.evaluate(legacy_hk, true, ModifierMask::EMPTY, HidUsage::CONTROL_RIGHT, true),
            HotkeyMatch::Trigger(HotkeyAction::Escape)
        );
        assert_eq!(
            sup.evaluate(legacy_hk, false, ModifierMask::EMPTY, HidUsage::CONTROL_RIGHT, false),
            HotkeyMatch::SuppressOnly
        );

        // 5. While suspended (all None): does not trigger new hotkeys,
        //    but still cleanly releases an already-suppressed key-up.
        assert_eq!(
            sup.evaluate(hk, false, ctrl_alt, HidUsage::KEY_L, true),
            HotkeyMatch::Trigger(HotkeyAction::ToggleLock)
        );
        assert_eq!(
            sup.evaluate(CaptureHotkeys::disabled(), false, ctrl_alt, HidUsage::ARROW_LEFT, true),
            HotkeyMatch::PassThrough
        );
        assert_eq!(
            sup.evaluate(CaptureHotkeys::disabled(), false, ctrl_alt, HidUsage::KEY_L, false),
            HotkeyMatch::SuppressOnly
        );
        assert_eq!(
            sup.evaluate(CaptureHotkeys::disabled(), false, ctrl_alt, HidUsage::KEY_L, true),
            HotkeyMatch::PassThrough
        );

        // 6. Modded escape chord (Ctrl+Alt+Escape): requires exact Ctrl+LeftAlt in remote mode,
        //    rejects extra Shift / AltGr, and never triggers in local mode.
        let modded_esc = CaptureHotkeys {
            escape: Some(HotkeyChord::new(HotkeyMods::DEFAULT_SWITCH, HidUsage::ESCAPE)),
            ..CaptureHotkeys::disabled()
        };
        assert_eq!(
            sup.evaluate(modded_esc, false, ctrl_alt, HidUsage::ESCAPE, true),
            HotkeyMatch::PassThrough
        );
        assert_eq!(
            sup.evaluate(
                modded_esc,
                true,
                ModifierMask(
                    ModifierMask::CONTROL_LEFT | ModifierMask::ALT_LEFT | ModifierMask::SHIFT_LEFT
                ),
                HidUsage::ESCAPE,
                true
            ),
            HotkeyMatch::PassThrough
        );
        assert_eq!(
            sup.evaluate(modded_esc, true, ctrl_altgr, HidUsage::ESCAPE, true),
            HotkeyMatch::PassThrough
        );
        assert_eq!(
            sup.evaluate(modded_esc, true, ctrl_alt, HidUsage::ESCAPE, true),
            HotkeyMatch::Trigger(HotkeyAction::Escape)
        );
        // clear() resets active suppressed key (e.g. on suspend/resume)
        sup.clear();
        assert_eq!(
            sup.evaluate(modded_esc, false, ModifierMask::EMPTY, HidUsage::ESCAPE, false),
            HotkeyMatch::PassThrough
        );

        // 7. Bare Pause panic escape also fires regardless of held modifiers in remote mode
        let pause_esc = CaptureHotkeys {
            escape: Some(HotkeyChord::new(HotkeyMods::EMPTY, HidUsage::PAUSE)),
            ..CaptureHotkeys::disabled()
        };
        assert_eq!(
            sup.evaluate(pause_esc, true, ctrl_alt, HidUsage::PAUSE, true),
            HotkeyMatch::Trigger(HotkeyAction::Escape)
        );
        assert_eq!(
            sup.evaluate(pause_esc, false, ctrl_alt, HidUsage::PAUSE, false),
            HotkeyMatch::SuppressOnly
        );
    }

    #[test]
    fn arrow_to_edge_and_mock_capture_control_hotkeys() {
        assert_eq!(arrow_to_edge(HidUsage::ARROW_LEFT), Some(Edge::Left));
        assert_eq!(arrow_to_edge(HidUsage::ARROW_RIGHT), Some(Edge::Right));
        assert_eq!(arrow_to_edge(HidUsage::ARROW_UP), Some(Edge::Top));
        assert_eq!(arrow_to_edge(HidUsage::ARROW_DOWN), Some(Edge::Bottom));
        assert_eq!(arrow_to_edge(HidUsage::KEY_L), None);

        let (handle, _tx, state) =
            crate::mock::create_mock_capture(crate::geometry::DesktopGeometry::fallback());
        assert_eq!(*state.hotkeys.lock(), CaptureHotkeys::default());
        assert!(!state.hotkeys_suspended.load(Ordering::SeqCst));

        let custom = CaptureHotkeys {
            switch_mods: None,
            lock: Some(HotkeyChord::new(HotkeyMods::SUPER | HotkeyMods::SHIFT, HidUsage::KEY_L)),
            escape: None,
        };
        handle.control.set_hotkeys(custom);
        handle.control.set_hotkeys_suspended(true);
        assert_eq!(*state.hotkeys.lock(), custom);
        assert!(state.hotkeys_suspended.load(Ordering::SeqCst));
    }
}
