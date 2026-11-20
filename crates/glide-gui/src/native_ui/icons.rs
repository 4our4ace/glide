//! Crisp analytically anti-aliased SVG icons & brand mark via `rune::Icon`,
//! plus the dynamic latency sparkline canvas.

use glide_proto::{Edge, OsKind};
use rune::{canvas, el, icon, Color, El, Icon, Shape};

static GLIDE_MARK: Icon = Icon {
    name: "glide-mark",
    view: 64.0,
    body: r#"<path d="M20 9V46.5L28 39.76V17Z" fill="currentColor" fill-opacity="0.58"/><path d="M32 21V43.88L36 52.5L42.3 49.6L36 36H47Z" fill="currentColor"/>"#,
};

static NAV_DEVICES: Icon = Icon {
    name: "nav-devices",
    view: 24.0,
    body: r#"<rect x="2.5" y="4" width="19" height="13" rx="2.2" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><path d="M8 20.5H16M12 17V20.5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>"#,
};

static NAV_INPUT: Icon = Icon {
    name: "nav-input",
    view: 24.0,
    body: r#"<rect x="6" y="2.5" width="12" height="19" rx="6" fill="none" stroke="currentColor" stroke-width="1.7"/><path d="M12 6.5V10.2" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>"#,
};

static NAV_CLIPBOARD: Icon = Icon {
    name: "nav-clipboard",
    view: 24.0,
    body: r#"<rect x="5" y="5" width="14" height="16" rx="2.2" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><rect x="9" y="2.5" width="6" height="4.2" rx="1.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>"#,
};

static NAV_SYSTEM: Icon = Icon {
    name: "nav-system",
    view: 24.0,
    body: r#"<path d="M3.5 7H20.5M3.5 12H20.5M3.5 17H20.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><circle cx="8.5" cy="7" r="2.1" fill="currentColor"/><circle cx="15.5" cy="12" r="2.1" fill="currentColor"/><circle cx="10.5" cy="17" r="2.1" fill="currentColor"/>"#,
};

static LOCK_CLOSED: Icon = Icon {
    name: "lock-closed",
    view: 24.0,
    body: r#"<rect x="5" y="11" width="14" height="9.5" rx="2" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/><path d="M8 11V7.5A4 4 0 0 1 16 7.5V11" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#,
};

static LOCK_OPEN: Icon = Icon {
    name: "lock-open",
    view: 24.0,
    body: r#"<rect x="5" y="11" width="14" height="9.5" rx="2" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/><path d="M8 11V7.5A4 4 0 0 1 15.6 5.8" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#,
};

static POWER: Icon = Icon {
    name: "power",
    view: 24.0,
    body: r#"<path d="M12 3.2V11.2" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/><path d="M16.6 6.8A7.2 7.2 0 1 1 7.4 6.8" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#,
};

static OS_WINDOWS: Icon = Icon {
    name: "os-windows",
    view: 16.0,
    body: r#"<path d="M1.5 2.9L6.8 2.2V7.3H1.5V2.9ZM7.8 2.1L14.5 1.2V7.3H7.8V2.1ZM1.5 8.7H6.8V13.8L1.5 13.1V8.7ZM7.8 8.7H14.5V14.8L7.8 13.9V8.7Z" fill="currentColor"/>"#,
};

static OS_LINUX: Icon = Icon {
    name: "os-linux",
    view: 16.0,
    body: r#"<rect x="1.5" y="2.5" width="13" height="11" rx="2" fill="none" stroke="currentColor" stroke-width="1.45" stroke-linejoin="round"/><path d="M4.6 6.2L6.9 8L4.6 9.8M8.4 9.8H11.4" fill="none" stroke="currentColor" stroke-width="1.45" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static OS_GENERIC: Icon = Icon {
    name: "os-generic",
    view: 16.0,
    body: r#"<rect x="2" y="2.5" width="12" height="9" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.45"/><path d="M5.4 13.6H10.6" fill="none" stroke="currentColor" stroke-width="1.45" stroke-linecap="round"/>"#,
};

static ARROW_LEFT: Icon = Icon {
    name: "arrow-left",
    view: 16.0,
    body: r#"<path d="M13 8H3M7.2 4L3 8L7.2 12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static ARROW_RIGHT: Icon = Icon {
    name: "arrow-right",
    view: 16.0,
    body: r#"<path d="M3 8H13M8.8 4L13 8L8.8 12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static ARROW_UP: Icon = Icon {
    name: "arrow-up",
    view: 16.0,
    body: r#"<path d="M8 13V3M4 7.2L8 3L12 7.2" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static ARROW_DOWN: Icon = Icon {
    name: "arrow-down",
    view: 16.0,
    body: r#"<path d="M8 3V13M4 8.8L8 13L12 8.8" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// Crisp sleek vector mouse pointer SVG for the active monitor in the screen map.
static CURSOR_POINTER: Icon = Icon {
    name: "cursor-pointer",
    view: 24.0,
    body: r#"<path d="M4.5 3.5L19.5 10.6L12.7 12.7L10.6 19.5L4.5 3.5Z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/>"#,
};

static UPLOAD: Icon = Icon {
    name: "upload",
    view: 24.0,
    body: r#"<path d="M12 15.5V5M7.5 9.5L12 5L16.5 9.5M4.5 16V19H19.5V16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static CHECK: Icon = Icon {
    name: "check",
    view: 24.0,
    body: r#"<path d="M5 12.5L9.8 17.3L19 7.2" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static CLOSE: Icon = Icon {
    name: "close",
    view: 16.0,
    body: r#"<path d="M4 4L12 12M12 4L4 12" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round"/>"#,
};

static TEXT_CLIP: Icon = Icon {
    name: "text-clip",
    view: 24.0,
    body: r#"<path d="M5 7H19M5 12H19M5 17H14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#,
};

static IMAGE_CLIP: Icon = Icon {
    name: "image-clip",
    view: 24.0,
    body: r#"<rect x="3.5" y="4.5" width="17" height="15" rx="2.4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"/><circle cx="9" cy="10" r="1.7" fill="currentColor"/><path d="M6 19L14.5 11.5L20 16.5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static RING_TRACK: Icon = Icon {
    name: "ring-track",
    view: 20.0,
    body: r#"<circle cx="10" cy="10" r="7" fill="none" stroke="currentColor" stroke-width="1.8"/>"#,
};

static RING_ARC: Icon = Icon {
    name: "ring-arc",
    view: 20.0,
    body: r#"<path d="M10 3A7 7 0 0 1 16.8 11.8" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#,
};

/// Glide's split-pointer SVG emblem inside an accent rounded square.
pub fn glide_mark(size: f32, radius: f32, bg: Color) -> El {
    el().size(size, size)
        .radius(radius)
        .fill(bg)
        .center()
        .child(icon(&GLIDE_MARK, size).color(Color::WHITE))
        .a11y_hidden()
}

/// Nav: Devices (monitor).
pub fn nav_devices(color: Color) -> El {
    icon(&NAV_DEVICES, 16.0).color(color).a11y_hidden()
}

/// Nav: Input (mouse).
pub fn nav_input(color: Color) -> El {
    icon(&NAV_INPUT, 16.0).color(color).a11y_hidden()
}

/// Nav: Clipboard & Files.
pub fn nav_clipboard(color: Color) -> El {
    icon(&NAV_CLIPBOARD, 16.0).color(color).a11y_hidden()
}

/// Nav: System (sliders).
pub fn nav_system(color: Color) -> El {
    icon(&NAV_SYSTEM, 16.0).color(color).a11y_hidden()
}

/// Lock icon (padlock).
pub fn lock_icon(locked: bool, color: Color) -> El {
    icon(if locked { &LOCK_CLOSED } else { &LOCK_OPEN }, 13.0)
        .color(color)
        .a11y_hidden()
}

/// Power / Quit icon.
pub fn power_icon(color: Color) -> El {
    icon(&POWER, 13.0).color(color).a11y_hidden()
}

/// OS icon (Windows 4 panes, Linux terminal, or generic display).
pub fn os_icon(os: OsKind, size: f32, color: Color) -> El {
    let glyph = match os {
        OsKind::Windows => &OS_WINDOWS,
        OsKind::LinuxX11 | OsKind::LinuxWayland => &OS_LINUX,
        _ => &OS_GENERIC,
    };
    icon(glyph, size).color(color).a11y_hidden()
}

/// Crisp directional arrow SVG (Inter subset lacks ← and →).
pub fn arrow_icon(edge: Edge, size: f32, color: Color) -> El {
    let glyph = match edge {
        Edge::Left => &ARROW_LEFT,
        Edge::Right => &ARROW_RIGHT,
        Edge::Top => &ARROW_UP,
        Edge::Bottom => &ARROW_DOWN,
    };
    icon(glyph, size).color(color).a11y_hidden()
}

/// Sleek vector pointer SVG shown in the bottom-right corner of the active desk monitor.
pub fn cursor_glyph() -> El {
    icon(&CURSOR_POINTER, 15.0)
        .color(Color::WHITE.alpha(0.95))
        .a11y_hidden()
}

/// Upload icon for the Send Files dropzone.
pub fn upload_icon(color: Color) -> El {
    icon(&UPLOAD, 16.0).color(color).a11y_hidden()
}

/// Checkmark icon for Connected badge.
pub fn check_icon(color: Color) -> El {
    icon(&CHECK, 14.0).color(color).a11y_hidden()
}

/// Small close '×' icon.
pub fn close_icon(size: f32, color: Color) -> El {
    icon(&CLOSE, size).color(color).a11y_hidden()
}

/// Text clipboard entry icon.
pub fn text_clip_icon(color: Color) -> El {
    icon(&TEXT_CLIP, 14.0).color(color).a11y_hidden()
}

/// Image clipboard entry icon.
pub fn image_clip_icon(color: Color) -> El {
    icon(&IMAGE_CLIP, 14.0).color(color).a11y_hidden()
}

/// Transfer direction arrow (up = sent, down = received).
pub fn transfer_icon(sent: bool, color: Color) -> El {
    arrow_icon(if sent { Edge::Top } else { Edge::Bottom }, 14.0, color)
}

/// Subtle ring indicator for waiting/scanning states (static, zero idle redraw cost).
pub fn waiting_ring(color: Color, accent: Color) -> El {
    el().size(18.0, 18.0)
        .child(icon(&RING_TRACK, 18.0).absolute(0.0, 0.0).color(color))
        .child(icon(&RING_ARC, 18.0).absolute(0.0, 0.0).color(accent))
        .a11y_hidden()
}

/// Latency sparkline in the sidebar (60x16).
pub fn sparkline(vals: &[f32], color: Color) -> El {
    let (w, h) = (60.0_f32, 16.0_f32);
    if vals.len() < 2 {
        return el().size(w, h).a11y_hidden();
    }
    let max = vals.iter().copied().fold(2.0_f32, f32::max);
    let denom = (vals.len() - 1) as f32;
    let pts: Vec<[f32; 2]> = vals
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = (i as f32 / denom) * w;
            let y = (h - 1.5) - ((v / max) * (h - 3.0)).clamp(0.0, h - 3.0);
            [x, y]
        })
        .collect();
    let shape = Shape::new().view_box(w, h).stroke(pts, 1.4, color);
    canvas(shape).size(w, h).a11y_hidden()
}
