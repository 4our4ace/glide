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

static CHEVRON_RIGHT: Icon = Icon {
    name: "chevron-right",
    view: 16.0,
    body: r#"<path d="M6 3.5L10.5 8L6 12.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static CHEVRON_DOWN: Icon = Icon {
    name: "chevron-down",
    view: 16.0,
    body: r#"<path d="M3.5 6L8 10.5L12.5 6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
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

static EYE_OFF: Icon = Icon {
    name: "eye-off",
    view: 24.0,
    body: r#"<path d="M10.6 5.1A9.8 9.8 0 0 1 12 5C17 5 20.8 8.3 22 12A13.6 13.6 0 0 1 19.3 16.3M14.1 14.1A3 3 0 0 1 9.9 9.9M6.6 6.6A13.5 13.5 0 0 0 2 12C3.2 15.7 7 19 12 19A9.7 9.7 0 0 0 16.4 18M3 3L21 21" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

// ---------------------------------------------------------------------------
// 16x16 monochrome setting-row icons (uniform stroke weight, crisp at 14px)
// ---------------------------------------------------------------------------

static THEME: Icon = Icon {
    name: "setting-theme",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="5.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M8 2.4A5.6 5.6 0 0 1 8 13.6Z" fill="currentColor"/>"#,
};

static AUTOSTART: Icon = Icon {
    name: "setting-autostart",
    view: 16.0,
    body: r#"<path d="M9.5 2.6H12A1.6 1.6 0 0 1 13.6 4.2V11.8A1.6 1.6 0 0 1 12 13.4H9.5M6.2 5.2L9.2 8L6.2 10.8M2.4 8H9.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static SHIELD_CHECK: Icon = Icon {
    name: "setting-shield-check",
    view: 16.0,
    body: r#"<path d="M8 2.2L13.2 4.2V7.8C13.2 10.9 10.9 13.1 8 14.1C5.1 13.1 2.8 10.9 2.8 7.8V4.2L8 2.2Z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M5.7 8.1L7.3 9.7L10.5 6.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static UPDATE_CHANNEL: Icon = Icon {
    name: "setting-update-channel",
    view: 16.0,
    body: r#"<circle cx="4.5" cy="3.8" r="1.6" fill="none" stroke="currentColor" stroke-width="1.5"/><circle cx="4.5" cy="12.2" r="1.6" fill="none" stroke="currentColor" stroke-width="1.5"/><circle cx="11.8" cy="4.8" r="1.6" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M4.5 5.4V10.6M4.5 10.4C4.5 8.2 11.8 8.8 11.8 6.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

static REFRESH: Icon = Icon {
    name: "setting-refresh",
    view: 16.0,
    body: r#"<path d="M13.2 8A5.2 5.2 0 1 1 12.1 4.8M12.3 2.1V5.0H9.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static DOWNLOAD: Icon = Icon {
    name: "setting-download",
    view: 16.0,
    body: r#"<path d="M8 2.4V10.2M5.0 7.4L8 10.4L11.0 7.4M2.8 11.4V13.2H13.2V11.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static SLIDERS: Icon = Icon {
    name: "setting-sliders",
    view: 16.0,
    body: r#"<path d="M2.4 4H13.6M2.4 8H13.6M2.4 12H13.6" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><circle cx="5.5" cy="4" r="1.55" fill="currentColor"/><circle cx="10.5" cy="8" r="1.55" fill="currentColor"/><circle cx="6.8" cy="12" r="1.55" fill="currentColor"/>"#,
};

static FINGERPRINT: Icon = Icon {
    name: "setting-fingerprint",
    view: 16.0,
    body: r#"<path d="M2.8 11.2V7.6A5.2 5.2 0 0 1 13.2 7.6V10.6M5.4 12.8V7.6A2.6 2.6 0 0 1 10.6 7.6V11.8M8 8.2V13.4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
};

static KEY: Icon = Icon {
    name: "setting-key",
    view: 16.0,
    body: r#"<circle cx="5.0" cy="8" r="2.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M7.6 8H13.6M10.8 8V10.5M13.4 8V10.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static SHIELD: Icon = Icon {
    name: "setting-shield",
    view: 16.0,
    body: r#"<path d="M8 2.2L13.2 4.2V7.8C13.2 10.9 10.9 13.1 8 14.1C5.1 13.1 2.8 10.9 2.8 7.8V4.2L8 2.2Z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>"#,
};

static GAUGE: Icon = Icon {
    name: "setting-gauge",
    view: 16.0,
    body: r#"<path d="M3.4 11.8A5.6 5.6 0 1 1 12.6 11.8M8 9.2L10.7 6.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><circle cx="8" cy="9.4" r="1.2" fill="currentColor"/>"#,
};

static SWAP_HORIZONTAL: Icon = Icon {
    name: "setting-swap-horizontal",
    view: 16.0,
    body: r#"<path d="M2.6 5.3H13.4M11.0 3.1L13.4 5.3L11.0 7.5M13.4 10.7H2.6M5.0 8.5L2.6 10.7L5.0 12.9" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static LOCK_SETTING: Icon = Icon {
    name: "setting-lock",
    view: 16.0,
    body: r#"<rect x="3.2" y="7.4" width="9.6" height="6.2" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M5.3 7.4V5.2A2.7 2.7 0 0 1 10.7 5.2V7.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

static HOME: Icon = Icon {
    name: "setting-home",
    view: 16.0,
    body: r#"<path d="M2.4 7.6L8 2.6L13.6 7.6M4 6.6V13.2H6.6V9.8H9.4V13.2H12V6.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static DIAGNOSTICS: Icon = Icon {
    name: "setting-diagnostics",
    view: 16.0,
    body: r#"<path d="M2 8.2H4.6L6.3 3.6L9.5 12.4L11.2 8.2H14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static CLIPBOARD: Icon = Icon {
    name: "setting-clipboard",
    view: 16.0,
    body: r#"<path d="M5.6 3.5H4.2A1.4 1.4 0 0 0 2.8 4.9V12.6A1.4 1.4 0 0 0 4.2 14H11.8A1.4 1.4 0 0 0 13.2 12.6V4.9A1.4 1.4 0 0 0 11.8 3.5H10.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/><rect x="5.6" y="2" width="4.8" height="2.8" rx="1" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/>"#,
};

static HISTORY: Icon = Icon {
    name: "setting-history",
    view: 16.0,
    body: r#"<path d="M3.4 5.1A5.4 5.4 0 1 1 3.3 10.7M3.4 2.4V5.1H6.1M8 5.2V8.2L10.1 9.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static DRAG_FILES: Icon = Icon {
    name: "setting-drag-files",
    view: 16.0,
    body: r#"<path d="M9.6 5.2V3.6A1.2 1.2 0 0 0 8.4 2.4H4A1.2 1.2 0 0 0 2.8 3.6V12.4A1.2 1.2 0 0 0 4 13.6H8.4A1.2 1.2 0 0 0 9.6 12.4V10.8M6.6 8H13.6M11.2 5.6L13.6 8L11.2 10.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static FOLDER: Icon = Icon {
    name: "setting-folder",
    view: 16.0,
    body: r#"<path d="M2.4 4.2A1.2 1.2 0 0 1 3.6 3H6.3L7.8 4.6H12.4A1.2 1.2 0 0 1 13.6 5.8V11.8A1.2 1.2 0 0 1 12.4 13H3.6A1.2 1.2 0 0 1 2.4 11.8V4.2Z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>"#,
};

static EDGE_DISTANCE: Icon = Icon {
    name: "setting-edge-distance",
    view: 16.0,
    body: r#"<path d="M13 2.8V13.2M2.6 8H10.0M7.2 5.2L10.0 8L7.2 10.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static CORNER_GUARD: Icon = Icon {
    name: "setting-corner-guard",
    view: 16.0,
    body: r#"<path d="M3.0 13.0V4.4A1.4 1.4 0 0 1 4.4 3.0H13.0M3.0 8.2A5.2 5.2 0 0 0 8.2 3.0" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static TIMER: Icon = Icon {
    name: "setting-timer",
    view: 16.0,
    body: r#"<path d="M6.5 2.0H9.5M8 9.0L10.2 6.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><circle cx="8" cy="9.0" r="4.6" fill="none" stroke="currentColor" stroke-width="1.6"/>"#,
};

static FULLSCREEN: Icon = Icon {
    name: "setting-fullscreen",
    view: 16.0,
    body: r#"<path d="M5.6 2.8H3.4A0.6 0.6 0 0 0 2.8 3.4V5.6M10.4 2.8H12.6A0.6 0.6 0 0 1 13.2 3.4V5.6M13.2 10.4V12.6A0.6 0.6 0 0 1 12.6 13.2H10.4M5.6 13.2H3.4A0.6 0.6 0 0 1 2.8 12.6V10.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static KEYBOARD: Icon = Icon {
    name: "setting-keyboard",
    view: 16.0,
    body: r#"<rect x="2.2" y="3.8" width="11.6" height="8.4" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M5.2 6.8H5.3M8 6.8H8.1M10.8 6.8H10.9M5.4 9.6H10.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

static VOLUME: Icon = Icon {
    name: "setting-volume",
    view: 16.0,
    body: r#"<path d="M2.2 6.2H4.5L7.6 3.5V12.5L4.5 9.8H2.2V6.2Z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M10.2 5.9A3.0 3.0 0 0 1 10.2 10.1M12.5 3.9A5.6 5.6 0 0 1 12.5 12.1" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#,
};

static MOUSE: Icon = Icon {
    name: "setting-mouse",
    view: 16.0,
    body: r#"<rect x="4.0" y="2.2" width="8.0" height="11.6" rx="4.0" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M8 4.8V7.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

static SCROLL: Icon = Icon {
    name: "setting-scroll",
    view: 16.0,
    body: r#"<path d="M8 2.6V13.4M5.3 5.2L8 2.6L10.7 5.2M5.3 10.8L8 13.4L10.7 10.8" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static DPI_SCALE: Icon = Icon {
    name: "setting-dpi-scale",
    view: 16.0,
    body: r#"<path d="M9.6 2.8H13.2V6.4M13.2 2.8L9.2 6.8M6.4 13.2H2.8V9.6M2.8 13.2L6.8 9.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static INVERT_SCROLL: Icon = Icon {
    name: "setting-invert-scroll",
    view: 16.0,
    body: r#"<path d="M5.2 13.0V3.0M3.0 5.4L5.2 3.0L7.4 5.4M10.8 3.0V13.0M8.6 10.6L10.8 13.0L13.0 10.6" fill="none" stroke="currentColor" stroke-width="1.55" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static TERMINAL_INPUT: Icon = Icon {
    name: "setting-terminal-input",
    view: 16.0,
    body: r#"<path d="M3.0 4.5L6.8 8L3.0 11.5M8.4 11.5H13.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static NETWORK: Icon = Icon {
    name: "setting-network",
    view: 16.0,
    body: r#"<circle cx="8" cy="8" r="5.6" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M2.6 8H13.4M8 2.4C9.7 4.0 10.5 6.0 10.5 8C10.5 10.0 9.7 12.0 8 13.6C6.3 12.0 5.5 10.0 5.5 8C5.5 6.0 6.3 4.0 8 2.4Z" fill="none" stroke="currentColor" stroke-width="1.4"/>"#,
};

static PORT: Icon = Icon {
    name: "setting-port",
    view: 16.0,
    body: r#"<path d="M6.2 2.8L4.8 13.2M11.2 2.8L9.8 13.2M2.8 5.8H13.4M2.4 10.2H13.0" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

static BROADCAST: Icon = Icon {
    name: "setting-broadcast",
    view: 16.0,
    body: r#"<circle cx="8" cy="11.6" r="1.2" fill="currentColor"/><path d="M5.4 8.8A3.8 3.8 0 0 1 10.6 8.8M3.0 6.4A7.2 7.2 0 0 1 13.0 6.4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

static MONITOR: Icon = Icon {
    name: "setting-monitor",
    view: 16.0,
    body: r#"<rect x="2.2" y="2.8" width="11.6" height="8.0" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M5.4 13.2H10.6M8 10.8V13.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>"#,
};

static POSITION: Icon = Icon {
    name: "setting-position",
    view: 16.0,
    body: r#"<path d="M8 2.4V13.6M2.4 8H13.6M6.0 4.4L8 2.4L10.0 4.4M6.0 11.6L8 13.6L10.0 11.6M4.4 6.0L2.4 8L4.4 10.0M11.6 6.0L13.6 8L11.6 10.0" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"#,
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

/// Nav: Settings (sliders).
pub fn nav_system(color: Color) -> El {
    icon(&NAV_SYSTEM, 16.0).color(color).a11y_hidden()
}

/// Lock icon (padlock).
pub fn lock_icon(locked: bool, size: f32, color: Color) -> El {
    icon(if locked { &LOCK_CLOSED } else { &LOCK_OPEN }, size)
        .color(color)
        .a11y_hidden()
}

/// Power / Quit icon.
pub fn power_icon(size: f32, color: Color) -> El {
    icon(&POWER, size).color(color).a11y_hidden()
}

/// Privacy eye-off icon for hidden clipboard history.
pub fn eye_off_icon(size: f32, color: Color) -> El {
    icon(&EYE_OFF, size).color(color).a11y_hidden()
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

/// Disclosure chevron icon (down when expanded, right when collapsed).
pub fn chevron_icon(expanded: bool, size: f32, color: Color) -> El {
    icon(
        if expanded {
            &CHEVRON_DOWN
        } else {
            &CHEVRON_RIGHT
        },
        size,
    )
    .color(color)
    .a11y_hidden()
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

// ---------------------------------------------------------------------------
// Setting-row icon constructors (14px glyph inside 26x26 badge)
// ---------------------------------------------------------------------------

fn setting_glyph(glyph: &'static Icon, color: Color) -> El {
    icon(glyph, 14.0).color(color).a11y_hidden()
}

pub fn theme_icon(color: Color) -> El {
    setting_glyph(&THEME, color)
}

pub fn autostart_icon(color: Color) -> El {
    setting_glyph(&AUTOSTART, color)
}

pub fn shield_check_icon(color: Color) -> El {
    setting_glyph(&SHIELD_CHECK, color)
}

pub fn update_channel_icon(color: Color) -> El {
    setting_glyph(&UPDATE_CHANNEL, color)
}

pub fn refresh_icon(color: Color) -> El {
    setting_glyph(&REFRESH, color)
}

pub fn download_icon(color: Color) -> El {
    setting_glyph(&DOWNLOAD, color)
}

pub fn sliders_icon(color: Color) -> El {
    setting_glyph(&SLIDERS, color)
}

pub fn fingerprint_icon(color: Color) -> El {
    setting_glyph(&FINGERPRINT, color)
}

pub fn key_icon(color: Color) -> El {
    setting_glyph(&KEY, color)
}

pub fn shield_icon(color: Color) -> El {
    setting_glyph(&SHIELD, color)
}

pub fn gauge_icon(color: Color) -> El {
    setting_glyph(&GAUGE, color)
}

pub fn swap_horizontal_icon(color: Color) -> El {
    setting_glyph(&SWAP_HORIZONTAL, color)
}

pub fn lock_setting_icon(color: Color) -> El {
    setting_glyph(&LOCK_SETTING, color)
}

pub fn home_icon(color: Color) -> El {
    setting_glyph(&HOME, color)
}

pub fn diagnostics_icon(color: Color) -> El {
    icon(&DIAGNOSTICS, 13.5).color(color).a11y_hidden()
}

pub fn clipboard_icon(color: Color) -> El {
    setting_glyph(&CLIPBOARD, color)
}

pub fn history_icon(color: Color) -> El {
    setting_glyph(&HISTORY, color)
}

pub fn drag_files_icon(color: Color) -> El {
    setting_glyph(&DRAG_FILES, color)
}

pub fn folder_icon(color: Color) -> El {
    setting_glyph(&FOLDER, color)
}

pub fn edge_distance_icon(color: Color) -> El {
    setting_glyph(&EDGE_DISTANCE, color)
}

pub fn corner_guard_icon(color: Color) -> El {
    setting_glyph(&CORNER_GUARD, color)
}

pub fn timer_icon(color: Color) -> El {
    setting_glyph(&TIMER, color)
}

pub fn fullscreen_icon(color: Color) -> El {
    setting_glyph(&FULLSCREEN, color)
}

pub fn keyboard_icon(color: Color) -> El {
    setting_glyph(&KEYBOARD, color)
}

pub fn volume_icon(color: Color) -> El {
    setting_glyph(&VOLUME, color)
}

pub fn mouse_icon(color: Color) -> El {
    setting_glyph(&MOUSE, color)
}

pub fn scroll_icon(color: Color) -> El {
    setting_glyph(&SCROLL, color)
}

pub fn dpi_scale_icon(color: Color) -> El {
    setting_glyph(&DPI_SCALE, color)
}

pub fn invert_scroll_icon(color: Color) -> El {
    setting_glyph(&INVERT_SCROLL, color)
}

pub fn terminal_input_icon(color: Color) -> El {
    setting_glyph(&TERMINAL_INPUT, color)
}

pub fn network_icon(color: Color) -> El {
    setting_glyph(&NETWORK, color)
}

pub fn port_icon(color: Color) -> El {
    setting_glyph(&PORT, color)
}

pub fn broadcast_icon(color: Color) -> El {
    setting_glyph(&BROADCAST, color)
}

pub fn monitor_icon(color: Color) -> El {
    setting_glyph(&MONITOR, color)
}

pub fn position_icon(color: Color) -> El {
    setting_glyph(&POSITION, color)
}

