//! Vector icons and sparkline shapes rendered directly by Rune's GPU canvas.

use glide_proto::{Edge, OsKind};
use rune::{canvas, el, Color, El, LineCap, Path, Shape};

/// Rounded rectangle closed polyline in arbitrary viewBox coordinates.
fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w * 0.5).min(h * 0.5);
    Path::new()
        .move_to([x + r, y])
        .line_to([x + w - r, y])
        .quad_to([x + w, y], [x + w, y + r])
        .line_to([x + w, y + h - r])
        .quad_to([x + w, y + h], [x + w - r, y + h])
        .line_to([x + r, y + h])
        .quad_to([x, y + h], [x, y + h - r])
        .line_to([x, y + r])
        .quad_to([x, y], [x + r, y])
        .close()
}

/// Circle approximated cleanly with 4 quadratic/cubic curves (or 16-segment polyline).
fn circle_pts(cx: f32, cy: f32, r: f32, segs: usize) -> Vec<[f32; 2]> {
    (0..segs)
        .map(|i| {
            let a = (i as f32 / segs as f32) * std::f32::consts::TAU;
            [cx + r * a.cos(), cy + r * a.sin()]
        })
        .collect()
}

/// Glide's split-pointer emblem inside an accent rounded square.
pub fn glide_mark(size: f32, radius: f32, bg: Color) -> El {
    // Matches `glide_backend::icon::GLIDE_ICON_SVG` geometry in 64x64 space.
    let left_half = vec![[20.0, 9.0], [20.0, 46.5], [28.0, 39.76], [28.0, 17.0]];
    let right_half = vec![
        [32.0, 21.0],
        [32.0, 43.88],
        [36.0, 52.5],
        [42.3, 49.6],
        [36.0, 36.0],
        [47.0, 36.0],
    ];
    let shape = Shape::new()
        .view_box(64.0, 64.0)
        .fill(left_half, Color::WHITE.alpha(0.58))
        .fill(right_half, Color::WHITE);

    el().size(size, size)
        .radius(radius)
        .fill(bg)
        .center()
        .child(canvas(shape).size(size, size))
        .a11y_hidden()
}

/// Nav: Devices (monitor).
pub fn nav_devices(color: Color) -> El {
    let s = Shape::new()
        .view_box(24.0, 24.0)
        .stroke_path(
            &rounded_rect(2.5, 4.0, 19.0, 13.0, 2.2),
            1.6,
            LineCap::Round,
            color,
        )
        .line([8.0, 20.5], [16.0, 20.5], 1.6, LineCap::Round, color)
        .line([12.0, 17.0], [12.0, 20.5], 1.6, LineCap::Round, color);
    canvas(s).size(16.0, 16.0).a11y_hidden()
}

/// Nav: Input (mouse).
pub fn nav_input(color: Color) -> El {
    let s = Shape::new()
        .view_box(24.0, 24.0)
        .stroke_path(
            &rounded_rect(6.0, 2.5, 12.0, 19.0, 6.0),
            1.6,
            LineCap::Round,
            color,
        )
        .line([12.0, 6.5], [12.0, 10.2], 1.6, LineCap::Round, color);
    canvas(s).size(16.0, 16.0).a11y_hidden()
}

/// Nav: Clipboard & Files.
pub fn nav_clipboard(color: Color) -> El {
    let s = Shape::new()
        .view_box(24.0, 24.0)
        .stroke_path(
            &rounded_rect(5.0, 5.0, 14.0, 16.0, 2.2),
            1.6,
            LineCap::Round,
            color,
        )
        .stroke_path(
            &rounded_rect(9.0, 2.5, 6.0, 4.2, 1.2),
            1.5,
            LineCap::Round,
            color,
        );
    canvas(s).size(16.0, 16.0).a11y_hidden()
}

/// Nav: System (sliders).
pub fn nav_system(color: Color) -> El {
    let mut s = Shape::new()
        .view_box(24.0, 24.0)
        .line([3.5, 7.0], [20.5, 7.0], 1.55, LineCap::Round, color)
        .line([3.5, 12.0], [20.5, 12.0], 1.55, LineCap::Round, color)
        .line([3.5, 17.0], [20.5, 17.0], 1.55, LineCap::Round, color);
    for (cx, cy) in [(8.5, 7.0), (15.5, 12.0), (10.5, 17.0)] {
        s = s.fill(circle_pts(cx, cy, 2.1, 12), color);
    }
    canvas(s).size(16.0, 16.0).a11y_hidden()
}

/// Lock icon (padlock).
pub fn lock_icon(locked: bool, color: Color) -> El {
    let shackle = if locked {
        Path::new()
            .move_to([8.0, 11.0])
            .line_to([8.0, 7.5])
            .quad_to([8.0, 4.0], [12.0, 4.0])
            .quad_to([16.0, 4.0], [16.0, 7.5])
            .line_to([16.0, 11.0])
    } else {
        Path::new()
            .move_to([8.0, 11.0])
            .line_to([8.0, 7.5])
            .quad_to([8.0, 4.0], [12.0, 4.0])
            .quad_to([15.5, 4.0], [16.0, 6.5])
    };
    let s = Shape::new()
        .view_box(24.0, 24.0)
        .stroke_path(
            &rounded_rect(5.0, 11.0, 14.0, 9.5, 2.0),
            1.7,
            LineCap::Round,
            color,
        )
        .stroke_path(&shackle, 1.7, LineCap::Round, color);
    canvas(s).size(13.0, 13.0).a11y_hidden()
}

/// Power / Quit icon.
pub fn power_icon(color: Color) -> El {
    let arc: Vec<[f32; 2]> = (0..=16)
        .map(|i| {
            // 280-degree open arc leaving the top gap for the power stem
            let deg = -50.0 + (i as f32 / 16.0) * 280.0;
            let rad = deg.to_radians();
            [12.0 + 7.2 * rad.cos(), 13.0 + 7.2 * rad.sin()]
        })
        .collect();
    let s = Shape::new()
        .view_box(24.0, 24.0)
        .line([12.0, 3.2], [12.0, 11.0], 1.7, LineCap::Round, color)
        .stroke(arc, 1.7, color);
    canvas(s).size(13.0, 13.0).a11y_hidden()
}

/// OS icon (14x14 by default): Windows 4 panes, Linux terminal, or generic display.
pub fn os_icon(os: OsKind, size: f32, color: Color) -> El {
    let s = match os {
        OsKind::Windows => Shape::new()
            .view_box(16.0, 16.0)
            .fill(vec![[1.6, 2.8], [6.8, 2.1], [6.8, 7.3], [1.6, 7.3]], color)
            .fill(
                vec![[7.8, 2.0], [14.4, 1.1], [14.4, 7.3], [7.8, 7.3]],
                color,
            )
            .fill(
                vec![[1.6, 8.5], [6.8, 8.5], [6.8, 13.7], [1.6, 13.0]],
                color,
            )
            .fill(
                vec![[7.8, 8.5], [14.4, 8.5], [14.4, 14.7], [7.8, 13.8]],
                color,
            ),
        OsKind::LinuxX11 | OsKind::LinuxWayland => Shape::new()
            .view_box(16.0, 16.0)
            .stroke_path(
                &rounded_rect(1.6, 2.6, 12.8, 10.8, 2.0),
                1.4,
                LineCap::Round,
                color,
            )
            .stroke(vec![[4.6, 6.2], [6.9, 8.0], [4.6, 9.8]], 1.4, color)
            .line([8.4, 9.8], [11.4, 9.8], 1.4, LineCap::Round, color),
        _ => Shape::new()
            .view_box(16.0, 16.0)
            .stroke_path(
                &rounded_rect(2.0, 2.6, 12.0, 8.8, 1.6),
                1.4,
                LineCap::Round,
                color,
            )
            .line([5.4, 13.6], [10.6, 13.6], 1.4, LineCap::Round, color),
    };
    canvas(s).size(size, size).a11y_hidden()
}

/// Crisp directional arrow drawn as vector strokes (Inter subset lacks ← and →).
pub fn arrow_icon(edge: Edge, size: f32, color: Color) -> El {
    let (shaft, head) = match edge {
        Edge::Left => (
            ([13.0, 8.0], [3.0, 8.0]),
            vec![[7.2, 4.0], [3.0, 8.0], [7.2, 12.0]],
        ),
        Edge::Right => (
            ([3.0, 8.0], [13.0, 8.0]),
            vec![[8.8, 4.0], [13.0, 8.0], [8.8, 12.0]],
        ),
        Edge::Top => (
            ([8.0, 13.0], [8.0, 3.0]),
            vec![[4.0, 7.2], [8.0, 3.0], [12.0, 7.2]],
        ),
        Edge::Bottom => (
            ([8.0, 3.0], [8.0, 13.0]),
            vec![[4.0, 8.8], [8.0, 13.0], [12.0, 8.8]],
        ),
    };
    let s = Shape::new()
        .view_box(16.0, 16.0)
        .line(shaft.0, shaft.1, 1.45, LineCap::Round, color)
        .stroke(head, 1.45, color);
    canvas(s).size(size, size).a11y_hidden()
}

/// Pointer glyph shown in the bottom-right corner of the active desk monitor.
pub fn cursor_glyph() -> El {
    let outline = rune::color::hex("#1e3a8a").unwrap_or(Color::BLACK);
    let pts = vec![
        [1.2, 1.2],
        [1.2, 11.0],
        [4.0, 8.4],
        [5.9, 12.2],
        [7.5, 11.4],
        [5.6, 7.6],
        [9.4, 7.6],
    ];
    let s = Shape::new()
        .view_box(11.0, 13.5)
        .fill(pts.clone(), Color::WHITE)
        .stroke_with(pts, true, 0.9, LineCap::Round, outline);
    canvas(s).size(11.0, 13.5).a11y_hidden()
}

/// Upload icon for the Send Files dropzone.
pub fn upload_icon(color: Color) -> El {
    let s = Shape::new()
        .view_box(24.0, 24.0)
        .line([12.0, 15.5], [12.0, 5.0], 1.75, LineCap::Round, color)
        .stroke(vec![[7.5, 9.5], [12.0, 5.0], [16.5, 9.5]], 1.75, color)
        .stroke(
            vec![[4.5, 16.0], [4.5, 19.0], [19.5, 19.0], [19.5, 16.0]],
            1.75,
            color,
        );
    canvas(s).size(16.0, 16.0).a11y_hidden()
}

/// Checkmark icon for Connected badge.
pub fn check_icon(color: Color) -> El {
    let s = Shape::new().view_box(24.0, 24.0).stroke(
        vec![[5.0, 12.5], [9.8, 17.3], [19.0, 7.2]],
        2.2,
        color,
    );
    canvas(s).size(14.0, 14.0).a11y_hidden()
}

/// Small close '×' icon.
pub fn close_icon(size: f32, color: Color) -> El {
    let s = Shape::new()
        .view_box(16.0, 16.0)
        .line([4.0, 4.0], [12.0, 12.0], 1.7, LineCap::Round, color)
        .line([12.0, 4.0], [4.0, 12.0], 1.7, LineCap::Round, color);
    canvas(s).size(size, size).a11y_hidden()
}

/// Text clipboard entry icon.
pub fn text_clip_icon(color: Color) -> El {
    let s = Shape::new()
        .view_box(24.0, 24.0)
        .line([5.0, 7.0], [19.0, 7.0], 1.7, LineCap::Round, color)
        .line([5.0, 12.0], [19.0, 12.0], 1.7, LineCap::Round, color)
        .line([5.0, 17.0], [14.0, 17.0], 1.7, LineCap::Round, color);
    canvas(s).size(14.0, 14.0).a11y_hidden()
}

/// Image clipboard entry icon.
pub fn image_clip_icon(color: Color) -> El {
    let s = Shape::new()
        .view_box(24.0, 24.0)
        .stroke_path(
            &rounded_rect(3.5, 4.5, 17.0, 15.0, 2.4),
            1.6,
            LineCap::Round,
            color,
        )
        .fill(circle_pts(9.0, 10.0, 1.7, 10), color)
        .stroke(vec![[6.0, 19.0], [14.5, 11.5], [20.0, 16.5]], 1.6, color);
    canvas(s).size(14.0, 14.0).a11y_hidden()
}

/// Transfer direction arrow (up = sent, down = received).
pub fn transfer_icon(sent: bool, color: Color) -> El {
    arrow_icon(if sent { Edge::Top } else { Edge::Bottom }, 14.0, color)
}

/// Subtle ring indicator for waiting/scanning states (static, zero idle redraw cost).
pub fn waiting_ring(color: Color, accent: Color) -> El {
    let full = circle_pts(10.0, 10.0, 7.0, 24);
    let arc: Vec<[f32; 2]> = (0..=8)
        .map(|i| {
            let deg = -90.0 + (i as f32 / 8.0) * 105.0;
            let rad = deg.to_radians();
            [10.0 + 7.0 * rad.cos(), 10.0 + 7.0 * rad.sin()]
        })
        .collect();
    let s = Shape::new()
        .view_box(20.0, 20.0)
        .stroke_with(full, true, 1.8, LineCap::Round, color)
        .stroke(arc, 1.8, accent);
    canvas(s).size(18.0, 18.0).a11y_hidden()
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
