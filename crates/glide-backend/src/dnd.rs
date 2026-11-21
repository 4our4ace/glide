//! Platform-agnostic cross-edge file drag-and-drop abstraction, `file://` URI
//! percent-encoding/decoding (`text/uri-list` per RFC 2483 / RFC 3986), and
//! edge-strip placement geometry shared across Windows, Linux X11, and Linux Wayland.

use crate::geometry::DesktopGeometry;
use anyhow::Result;
use glide_proto::Edge;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Outcome of delivering received cross-edge files to the receiving desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropDeliveryOutcome {
    /// Files were delivered directly to an application window via native OS drag-and-drop
    /// (Windows OLE `DoDragDrop` or Linux X11 XDND v5).
    DroppedIntoTarget,
    /// Files were saved into `Downloads/Glide/` and published to the OS clipboard as
    /// `text/uri-list` (Linux) or `CF_HDROP` (Windows) so the user can immediately `Ctrl+V`.
    ClipboardFallback,
}

/// Platform-agnostic cross-edge file drag-and-drop backend.
///
/// Implementations manage two halves without blocking normal pointer input:
/// 1. **Drag-out:** Invisible 1px edge-strip drop target window(s) placed along active
///    outer edges, shown ONLY while a local left-button drag is armed so normal clicks,
///    scrollbars, and window resizing are never obstructed.
/// 2. **Drag-in:** Native OS drop handoff (`DoDragDrop` on Windows, XDND v5 on Linux X11,
///    or `wl-clipboard-rs` `text/uri-list` fallback on Wayland) after files are saved to
///    `Downloads/Glide/`.
pub trait DndBackend: Send + Sync {
    /// Enables or disables cross-edge file drag-and-drop at runtime.
    fn set_enabled(&self, enabled: bool);

    /// Updates active outer edges and dead-corner margin used to position edge strips.
    fn set_active_edges(&self, edges: &[Edge], dead_corner_px: i32);

    /// Arms or disarms the invisible edge-strip drop targets.
    /// Callers MUST only arm while the left mouse button is held during a potential local drag.
    fn set_drag_armed(&self, armed: bool);

    /// Returns and consumes any local file paths staged on `edge` by a local drag-out.
    fn take_staged_files(&self, edge: Edge) -> Option<Vec<PathBuf>>;

    /// Cleanly cancels the local OS drag operation (e.g. Explorer `DoDragDrop` or X11 XDND grab)
    /// and hides edge strips so Glide can seamlessly transition control to the peer.
    fn cancel_local_drag(&self);

    /// Optional live cursor notification while an incoming cross-edge drag moves over this screen.
    fn on_incoming_drag_move(&self, _x: i32, _y: i32) {}

    /// Cancels any live hover state for an incoming cross-edge drag.
    fn on_incoming_drag_cancel(&self) {}

    /// Hands off received files (already saved on disk in `Downloads/Glide/`) to the target
    /// application under the cursor, falling back cleanly to the clipboard if needed.
    fn deliver_drop(&self, paths: Vec<PathBuf>) -> Result<DropDeliveryOutcome>;
}

/// Safe no-op backend used on unsupported platforms (e.g. macOS) or when DnD initialization fails.
#[derive(Debug, Default)]
pub struct NoopDndBackend;

impl DndBackend for NoopDndBackend {
    fn set_enabled(&self, _enabled: bool) {}
    fn set_active_edges(&self, _edges: &[Edge], _dead_corner_px: i32) {}
    fn set_drag_armed(&self, _armed: bool) {}
    fn take_staged_files(&self, _edge: Edge) -> Option<Vec<PathBuf>> {
        None
    }
    fn cancel_local_drag(&self) {}
    fn deliver_drop(&self, _paths: Vec<PathBuf>) -> Result<DropDeliveryOutcome> {
        Ok(DropDeliveryOutcome::ClipboardFallback)
    }
}

/// Physical/virtual-desktop rectangle for an invisible 1px edge-strip drop target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeStripRect {
    pub edge: Edge,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Computes invisible edge-strip rectangles along every monitor touching `active_edges`,
/// inset by `dead_corner_px` so window close buttons (`X`) and taskbar corners are never covered.
pub fn compute_edge_strip_rects(
    geometry: &DesktopGeometry,
    active_edges: &[Edge],
    dead_corner_px: i32,
    thickness_px: i32,
) -> Vec<EdgeStripRect> {
    if geometry.monitors.is_empty() || active_edges.is_empty() {
        return Vec::new();
    }
    let (min_x, min_y, max_x, max_y) = geometry.bounds();
    const TOL: i32 = 4;
    let dead = dead_corner_px.max(0);
    let thick = thickness_px.max(1);
    let mut out = Vec::new();

    for &edge in active_edges {
        for m in &geometry.monitors {
            let on_edge = match edge {
                Edge::Left => (m.left() - min_x).abs() <= TOL,
                Edge::Right => (m.right() - max_x).abs() <= TOL,
                Edge::Top => (m.top() - min_y).abs() <= TOL,
                Edge::Bottom => (m.bottom() - max_y).abs() <= TOL,
            };
            if !on_edge {
                continue;
            }
            let rect = match edge {
                Edge::Left => {
                    let d = dead.min(m.height / 4);
                    EdgeStripRect {
                        edge,
                        x: m.left(),
                        y: m.top() + d,
                        width: thick.min(m.width.max(1)),
                        height: (m.height - 2 * d).max(1),
                    }
                }
                Edge::Right => {
                    let d = dead.min(m.height / 4);
                    let w = thick.min(m.width.max(1));
                    EdgeStripRect {
                        edge,
                        x: m.right() - w,
                        y: m.top() + d,
                        width: w,
                        height: (m.height - 2 * d).max(1),
                    }
                }
                Edge::Top => {
                    let d = dead.min(m.width / 4);
                    EdgeStripRect {
                        edge,
                        x: m.left() + d,
                        y: m.top(),
                        width: (m.width - 2 * d).max(1),
                        height: thick.min(m.height.max(1)),
                    }
                }
                Edge::Bottom => {
                    let d = dead.min(m.width / 4);
                    let h = thick.min(m.height.max(1));
                    EdgeStripRect {
                        edge,
                        x: m.left() + d,
                        y: m.bottom() - h,
                        width: (m.width - 2 * d).max(1),
                        height: h,
                    }
                }
            };
            if rect.width > 0 && rect.height > 0 {
                out.push(rect);
            }
        }
    }
    out
}

/// Default headline shown in the cursor-adjacent fallback hint popup when a target app rejects a drop.
pub const FALLBACK_HINT_TITLE: &str = "This app didn't allow the drop";
/// Default detail line shown in the cursor-adjacent fallback hint popup.
pub const FALLBACK_HINT_SUBTITLE: &str = "Saved to Downloads/Glide (Ctrl+V to paste)";
/// Combined single-line fallback message (used in desktop notifications on Wayland).
pub const FALLBACK_HINT_SINGLE_LINE: &str =
    "This app didn't allow the drop — saved to Downloads/Glide (Ctrl+V to paste)";

/// Computes a safe screen position `(x, y)` for a cursor-adjacent hint popup of size
/// `(width, height)` starting at `(cursor_x + 14, cursor_y + 18)`, flipping or clamping inside
/// the monitor containing the cursor so the card never clips off-screen.
pub fn clamp_hint_popup_pos(
    geometry: &DesktopGeometry,
    cursor_x: i32,
    cursor_y: i32,
    width: i32,
    height: i32,
) -> (i32, i32) {
    const OFFSET_X: i32 = 14;
    const OFFSET_Y: i32 = 18;
    const MARGIN: i32 = 8;

    let (left, top, right, bottom) = geometry
        .monitors
        .iter()
        .find(|m| m.contains(cursor_x, cursor_y))
        .or_else(|| geometry.primary())
        .map(|m| (m.left(), m.top(), m.right(), m.bottom()))
        .unwrap_or_else(|| geometry.bounds());

    let mut x = cursor_x + OFFSET_X;
    if x + width > right - MARGIN {
        x = cursor_x - width - OFFSET_X;
    }
    let min_x = left + MARGIN;
    let max_x = (right - width - MARGIN).max(min_x);
    x = x.clamp(min_x, max_x);

    let mut y = cursor_y + OFFSET_Y;
    if y + height > bottom - MARGIN {
        y = cursor_y - height - OFFSET_X;
    }
    let min_y = top + MARGIN;
    let max_y = (bottom - height - MARGIN).max(min_y);
    y = y.clamp(min_y, max_y);

    (x, y)
}

// ---------------------------------------------------------------------------
// RFC 3986 / RFC 2483 `file://` URI encoding & `text/uri-list` parsing
// ---------------------------------------------------------------------------

fn is_uri_path_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/')
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_encode_path_bytes(bytes: &[u8], preserve_drive_colon: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() + 16);
    for (idx, &b) in bytes.iter().enumerate() {
        let is_drive_colon = preserve_drive_colon
            && idx == 2
            && b == b':'
            && bytes.first() == Some(&b'/')
            && bytes.get(1).is_some_and(u8::is_ascii_alphabetic);
        if is_uri_path_unreserved(b) || is_drive_colon {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

fn percent_decode_bytes(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_val(bytes[i + 1])?;
            let lo = hex_val(bytes[i + 2])?;
            let decoded = (hi << 4) | lo;
            if decoded == 0 {
                // Reject embedded NUL bytes for OS path safety.
                return None;
            }
            out.push(decoded);
            i += 3;
        } else {
            if bytes[i] == 0 {
                return None;
            }
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Encodes an absolute filesystem path string (Unix `/...` or Windows `C:\...`) as a `file://` URI.
pub fn encode_file_uri_str(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    // Check for Windows drive letter path (e.g. `C:\Users\...` or `C:/Users/...`)
    let bytes = raw.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        let normalized = format!("/{}", raw.replace('\\', "/"));
        let encoded = percent_encode_path_bytes(normalized.as_bytes(), true);
        return Some(format!("file://{encoded}"));
    }

    if !raw.starts_with('/') {
        return None;
    }
    let encoded = percent_encode_path_bytes(raw.as_bytes(), false);
    Some(format!("file://{encoded}"))
}

/// Encodes an absolute local [`Path`] into an RFC 3986 `file:///...` URI.
pub fn path_to_file_uri(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if !path.is_absolute() {
            return None;
        }
        let raw = path.as_os_str().as_bytes();
        if raw.is_empty() || raw.contains(&0) {
            return None;
        }
        let encoded = percent_encode_path_bytes(raw, false);
        Some(format!("file://{encoded}"))
    }
    #[cfg(not(unix))]
    {
        if !path.is_absolute() {
            return None;
        }
        encode_file_uri_str(path.to_str()?)
    }
}

/// Decodes a single `file://` URI into a local [`PathBuf`].
///
/// Rejects non-`file://` schemes (`http://`, `https://`, `ftp://`, etc.), non-local hosts,
/// malformed percent escapes, and NUL bytes.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let trimmed = uri.trim();
    if trimmed.len() < 8 {
        return None;
    }
    let (scheme, rest) = trimmed.split_at(7);
    if !scheme.eq_ignore_ascii_case("file://") {
        return None;
    }

    // Strip optional query/fragment if present.
    let rest = rest.split(['?', '#']).next()?;

    // Extract authority before the first `/`. Only empty authority (`file:///...`) or
    // `localhost` (`file://localhost/...`) is accepted for security.
    let slash_idx = rest.find('/')?;
    let authority = &rest[..slash_idx];
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let raw_path = &rest[slash_idx..];
    let decoded = percent_decode_bytes(raw_path)?;
    if decoded.is_empty() {
        return None;
    }

    // Handle Windows drive URI form `/C:/Users/...` when running on Windows.
    #[cfg(windows)]
    {
        let s = String::from_utf8(decoded).ok()?;
        let b = s.as_bytes();
        let win_path = if b.len() >= 4
            && b[0] == b'/'
            && b[1].is_ascii_alphabetic()
            && b[2] == b':'
            && b[3] == b'/'
        {
            s[1..].replace('/', "\\")
        } else {
            s.replace('/', "\\")
        };
        Some(PathBuf::from(win_path))
    }

    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(OsString::from_vec(decoded)))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let s = String::from_utf8(decoded).ok()?;
        Some(PathBuf::from(s))
    }
}

/// Formats local paths into an RFC 2483 `text/uri-list` payload (`\r\n`-terminated).
pub fn format_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        if let Some(uri) = path_to_file_uri(path) {
            out.push_str(&uri);
            out.push_str("\r\n");
        }
    }
    out
}

/// Formats local paths for `x-special/gnome-copied-files` (`copy\nfile:///...`),
/// understood by Nautilus, Nemo, Caja, Thunar, and GTK file managers on Ctrl+V.
pub fn format_gnome_copied_files(paths: &[PathBuf]) -> String {
    let uris: Vec<String> = paths.iter().filter_map(|p| path_to_file_uri(p)).collect();
    if uris.is_empty() {
        return String::new();
    }
    format!("copy\n{}", uris.join("\n"))
}

/// Parses an RFC 2483 `text/uri-list` payload into unique local [`PathBuf`] entries.
///
/// Ignores blank lines, `#` comment lines, non-`file://` schemes, and non-local hosts.
pub fn parse_uri_list(raw: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(raw);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(path) = file_uri_to_path(trimmed) {
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::MonitorRect;

    #[test]
    fn file_uri_roundtrips_spaces_german_umlauts_and_special_chars() {
        let paths = vec![
            PathBuf::from("/home/max/Dokumente/Büro Grüße äöüß.pdf"),
            PathBuf::from("/tmp/my folder/100% ready #1 (final).png"),
            PathBuf::from("/home/max/euro_€_notes.txt"),
        ];

        let uri_list = format_uri_list(&paths);
        assert!(uri_list.contains("file:///home/max/Dokumente/B%C3%BCro%20Gr%C3%BC%C3%9Fe%20%C3%A4%C3%B6%C3%BC%C3%9F.pdf\r\n"));
        assert!(uri_list.contains("file:///tmp/my%20folder/100%25%20ready%20%231%20%28final%29.png\r\n"));

        let parsed = parse_uri_list(uri_list.as_bytes());
        assert_eq!(parsed, paths);
    }

    #[test]
    fn uri_list_handles_comments_localhost_and_rejects_non_file_schemes() {
        let raw = concat!(
            "# comment line to ignore\r\n",
            "\r\n",
            "https://example.com/not-a-file.png\r\n",
            "http://localhost/index.html\r\n",
            "ftp://files.local/archive.zip\r\n",
            "mailto:user@example.com\r\n",
            "/bare/path/without/scheme.txt\r\n",
            "file://remote-host.local/share/evil.exe\r\n",
            "file:///tmp/bad%00nul.txt\r\n",
            "file:///tmp/bad%ZZhex.txt\r\n",
            "FILE://localhost/home/user/valid%20file.txt\r\n",
            "file:///home/user/second%20%C3%A4%C3%B6%C3%BC%C3%9F.md\n",
            "file:///home/user/valid%20file.txt\n", // duplicate should be deduped
        );

        let parsed = parse_uri_list(raw.as_bytes());
        assert_eq!(
            parsed,
            vec![
                PathBuf::from("/home/user/valid file.txt"),
                PathBuf::from("/home/user/second äöüß.md"),
            ]
        );
    }

    #[test]
    fn encode_windows_drive_uri_preserves_drive_colon() {
        let uri = encode_file_uri_str(r"C:\Users\Max\Büro\hello world.txt").unwrap();
        assert_eq!(
            uri,
            "file:///C:/Users/Max/B%C3%BCro/hello%20world.txt"
        );
        assert!(encode_file_uri_str("relative/path.txt").is_none());
    }

    #[test]
    fn gnome_copied_files_formats_copy_header() {
        let paths = vec![
            PathBuf::from("/tmp/a b.txt"),
            PathBuf::from("/tmp/über.png"),
        ];
        assert_eq!(
            format_gnome_copied_files(&paths),
            "copy\nfile:///tmp/a%20b.txt\nfile:///tmp/%C3%BCber.png"
        );
    }

    #[test]
    fn edge_strip_rects_respect_dead_corners_and_multi_monitor() {
        let geom = DesktopGeometry::new(vec![
            MonitorRect {
                name: "Left-1080p".into(),
                x: -1920,
                y: 180,
                width: 1920,
                height: 1080,
                scale: 1.0,
                primary: false,
            },
            MonitorRect {
                name: "Main-1440p".into(),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 1.5,
                primary: true,
            },
        ]);

        let strips = compute_edge_strip_rects(&geom, &[Edge::Right, Edge::Left], 24, 1);
        assert_eq!(strips.len(), 2);

        let right = strips.iter().find(|s| s.edge == Edge::Right).unwrap();
        assert_eq!((right.x, right.y, right.width, right.height), (2559, 24, 1, 1392));

        let left = strips.iter().find(|s| s.edge == Edge::Left).unwrap();
        assert_eq!((left.x, left.y, left.width, left.height), (-1920, 204, 1, 1032));
    }

    #[test]
    fn hint_popup_clamping_respects_monitor_bounds_and_flips_near_edges() {
        let geom = DesktopGeometry::new(vec![
            MonitorRect {
                name: "Left".into(),
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                primary: false,
            },
            MonitorRect {
                name: "Main".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                primary: true,
            },
        ]);

        // Middle of monitor: offset by (+14, +18)
        assert_eq!(clamp_hint_popup_pos(&geom, 400, 300, 320, 56), (414, 318));

        // Near bottom-right corner of Main monitor: flips left & above cursor, stays in bounds
        let (x, y) = clamp_hint_popup_pos(&geom, 1900, 1060, 320, 56);
        assert!(x >= 8 && x + 320 <= 1912, "x={x} clipped out of Main monitor");
        assert!(y >= 8 && y + 56 <= 1072, "y={y} clipped out of Main monitor");

        // Negative-coordinate secondary monitor: stays inside [-1920, 0)
        let (lx, ly) = clamp_hint_popup_pos(&geom, -100, 500, 320, 56);
        assert!(lx >= -1912 && lx + 320 <= -8, "lx={lx} clipped out of Left monitor");
        assert_eq!(ly, 518);
    }
}
