//! Colour tokens, typography and persisted theme preference for the native UI.

use glide_core::config::default_config_dir;
use rune::color::hex;
use rune::Color;
use std::sync::LazyLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePref {
    System,
    Light,
    Dark,
}

impl ThemePref {
    pub fn load() -> Self {
        let path = default_config_dir().join("ui-theme");
        match std::fs::read_to_string(path).ok().as_deref().map(str::trim) {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        }
    }

    pub fn save(self) {
        let dir = default_config_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("ui-theme");
        match self {
            Self::System => {
                let _ = std::fs::remove_file(path);
            }
            Self::Light => {
                let _ = std::fs::write(path, "light\n");
            }
            Self::Dark => {
                let _ = std::fs::write(path, "dark\n");
            }
        }
    }

    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::System => system_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }
}

fn c(code: &str) -> Color {
    hex(code).expect("valid hex colour")
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color,
    pub sidebar: Color,
    pub surface: Color,
    pub canvas: Color,
    pub hover: Color,
    pub press: Color,
    pub nav_active: Color,
    pub seg_bg: Color,
    pub seg_active: Color,
    pub line: Color,
    pub line_2: Color,
    pub text: Color,
    pub text_2: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    pub accent_hover: Color,
    pub accent_soft: Color,
    pub accent_text: Color,
    pub green: Color,
    pub green_soft: Color,
    pub amber: Color,
    pub red: Color,
    pub red_soft: Color,
    pub red_line: Color,
    pub toast_bg: Color,
    pub toast_text: Color,
    pub shadow_sm: Color,
    pub knob_shadow: Color,
}

static DARK_PALETTE: LazyLock<Palette> = LazyLock::new(|| Palette {
    dark: true,
    bg: c("#1c1c1f"),
    sidebar: c("#161618"),
    surface: c("#232326"),
    canvas: c("#19191c"),
    hover: c("#2a2a2e"),
    press: c("#323237"),
    nav_active: c("#2c2c31"),
    seg_bg: c("#2a2a2e"),
    seg_active: c("#3d3d43"),
    line: c("#2e2e33"),
    line_2: c("#3c3c42"),
    text: c("#f4f4f5"),
    text_2: c("#d4d4d8"),
    muted: c("#a1a1aa"),
    faint: c("#6b6b74"),
    accent: c("#3b82f6"),
    accent_hover: c("#2f6fe0"),
    accent_soft: c("#3b82f6").alpha(0.14),
    accent_text: c("#7fb0ff"),
    green: c("#22c55e"),
    green_soft: c("#22c55e").alpha(0.14),
    amber: c("#f59e0b"),
    red: c("#f87171"),
    red_soft: c("#f87171").alpha(0.12),
    red_line: c("#f87171").alpha(0.30),
    toast_bg: c("#f4f4f5"),
    toast_text: c("#18181b"),
    shadow_sm: Color::BLACK.alpha(0.30),
    knob_shadow: Color::BLACK.alpha(0.40),
});

static LIGHT_PALETTE: LazyLock<Palette> = LazyLock::new(|| Palette {
    dark: false,
    bg: c("#ffffff"),
    sidebar: c("#f7f7f8"),
    surface: c("#ffffff"),
    canvas: c("#f4f4f5"),
    hover: c("#f0f0f2"),
    press: c("#e6e6ea"),
    nav_active: c("#e8e8eb"),
    seg_bg: c("#ededf0"),
    seg_active: c("#ffffff"),
    line: c("#e7e7ea"),
    line_2: c("#d6d6db"),
    text: c("#18181b"),
    text_2: c("#3f3f46"),
    muted: c("#71717a"),
    faint: c("#a1a1aa"),
    accent: c("#2563eb"),
    accent_hover: c("#1d4ed8"),
    accent_soft: c("#eef4ff"),
    accent_text: c("#1d4ed8"),
    green: c("#16a34a"),
    green_soft: c("#ecfdf3"),
    amber: c("#d97706"),
    red: c("#dc2626"),
    red_soft: c("#fef2f2"),
    red_line: c("#fecaca"),
    toast_bg: c("#18181b"),
    toast_text: c("#fafafa"),
    shadow_sm: c("#101014").alpha(0.08),
    knob_shadow: c("#101014").alpha(0.22),
});

impl Palette {
    pub fn new(dark: bool, _scale: f32) -> Self {
        if dark {
            *DARK_PALETTE
        } else {
            *LIGHT_PALETTE
        }
    }
}

/// Shared text styles (Inter 400/500/600 and JetBrains Mono 400).
pub mod ty {
    use rune::TextStyle;

    pub const PAGE_TITLE: TextStyle = TextStyle::new(18.0, 600.0).lh(23.0).tracking(-0.25);
    pub const CAPTION: TextStyle = TextStyle::new(12.5, 400.0).lh(17.0);
    pub const BRAND_TITLE: TextStyle = TextStyle::new(14.0, 600.0).lh(17.0).tracking(-0.15);
    pub const BRAND_SUB: TextStyle = TextStyle::new(11.5, 400.0).lh(15.0);
    pub const SECTION: TextStyle = TextStyle::new(12.0, 500.0).lh(16.0);
    pub const BODY: TextStyle = TextStyle::new(13.0, 400.0).lh(18.0);
    pub const BODY_MED: TextStyle = TextStyle::new(13.0, 500.0).lh(18.0);
    pub const BODY_SEMI: TextStyle = TextStyle::new(13.0, 600.0).lh(18.0);
    pub const LABEL: TextStyle = TextStyle::new(12.5, 500.0).lh(17.0);
    pub const SUB: TextStyle = TextStyle::new(11.5, 400.0).lh(15.5);
    pub const SUB_MED: TextStyle = TextStyle::new(11.5, 500.0).lh(15.5);
    pub const BADGE: TextStyle = TextStyle::new(11.0, 500.0).lh(14.0);
    pub const MONO: TextStyle = TextStyle::new(12.0, 400.0).mono().lh(17.0);
    pub const MONO_SM: TextStyle = TextStyle::new(11.5, 400.0).mono().lh(16.5);
    pub const KBD: TextStyle = TextStyle::new(11.0, 400.0).mono().lh(14.0);
}
