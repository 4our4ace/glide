//! Pure-Rust anti-aliased Glide app icon renderer (zero external assets).
//!
//! Used identically across:
//! - Ubuntu / Linux top-right StatusNotifierItem system tray (`ksni` ARGB32, symbolic white glyph)
//! - Linux X11 `_NET_WM_ICON` window/dock/Alt+Tab icon
//! - Windows 11 `Shell_NotifyIconW` system tray icon (`HICON`)
//! - Embedded GUI `/favicon.png` & `/favicon.svg`
//!
//! # The mark
//! A classic mouse pointer sliced by a vertical seam — the edge between two screens.
//! The faded sliver is the part still on the old screen; the solid body has already
//! glided across. Canonical vector sources live in `assets/logo/`.
//!
//! The rasterizer evaluates the exact same geometry as the SVGs using analytic signed
//! distance fields with 4x4 supersampling. Vertical/horizontal edges (pointer spine,
//! seam, wing) are snapped to the pixel grid at every size so 16 px and 22 px tray icons
//! stay razor sharp instead of blurry.

/// Canonical full-colour SVG app icon (identical to `assets/logo/glide.svg`).
pub const GLIDE_ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <defs>
    <linearGradient id="glide-g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#3b82f6"/>
      <stop offset="1" stop-color="#06b6d4"/>
    </linearGradient>
  </defs>
  <rect x="2" y="2" width="60" height="60" rx="14" fill="url(#glide-g)"/>
  <path d="M20 9V46.5L28 39.76V17Z" fill="#fff" fill-opacity=".55"/>
  <path d="M32 21V43.88L36 52.5L42.3 49.6L36 36H47Z" fill="#fff"/>
</svg>
"##;

/// Canonical symbolic (white glyph, transparent) SVG (identical to `assets/logo/glide-symbolic.svg`).
pub const GLIDE_SYMBOLIC_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="7.5 4.75 52 52">
  <path d="M20 9V46.5L28 39.76V17Z" fill="#fff" fill-opacity=".55"/>
  <path d="M32 21V43.88L36 52.5L42.3 49.6L36 36H47Z" fill="#fff"/>
</svg>
"##;

/// Pointer silhouette in the 64-unit design space (before the seam cut).
const POINTER: [(f32, f32); 7] = [
    (20.0, 9.0),  // tip
    (20.0, 46.5), // bottom of the spine
    (29.5, 38.5), // notch
    (36.0, 52.5), // tail, outer-left
    (42.3, 49.6), // tail, outer-right
    (36.0, 36.0), // tail, inner
    (47.0, 36.0), // wing
];
/// Vertical spine of the pointer (snapped to the pixel grid).
const SPINE_X: f32 = 20.0;
/// Horizontal underside of the wing (snapped to the pixel grid).
const WING_Y: f32 = 36.0;
/// The seam (screen edge) cutting the pointer: `SEAM_L..SEAM_R`.
const SEAM_L: f32 = 28.0;
const SEAM_W: f32 = 4.0;
/// Opacity of the trailing sliver left on the previous screen.
const GHOST_OPACITY: f32 = 0.55;

/// Badge geometry (64-unit space): 60x60 rounded square inset by 2 with rx = 14.
const BADGE_INSET: f32 = 2.0;
const BADGE_RADIUS_RATIO: f32 = 14.0 / 60.0;
/// Gradient `#3b82f6` (top-left) -> `#06b6d4` (bottom-right).
const GRAD_FROM: [f32; 3] = [59.0, 130.0, 246.0];
const GRAD_TO: [f32; 3] = [6.0, 182.0, 212.0];

/// Supersampling grid per axis (4x4 = 16 samples per pixel).
const SS: usize = 4;

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Coverage of a single supersample given a signed distance in pixels (negative = inside).
/// The filter width equals the sample spacing, so pixel-aligned edges stay perfectly crisp.
fn sample_coverage(d_px: f32) -> f32 {
    clamp01(0.5 - d_px * SS as f32)
}

/// Signed distance to a rounded rectangle centered at (0,0) with half-extents (bx, by) and radius r.
fn sd_rounded_box(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    let qx = px.abs() - bx + r;
    let qy = py.abs() - by + r;
    let out = (qx.max(0.0).hypot(qy.max(0.0))) - r;
    let inside = qx.max(qy).min(0.0);
    out + inside
}

/// Exact signed distance to an arbitrary simple polygon (negative inside).
fn sd_polygon(px: f32, py: f32, poly: &[(f32, f32)]) -> f32 {
    let (x0, y0) = poly[0];
    let mut d = (px - x0) * (px - x0) + (py - y0) * (py - y0);
    let mut sign = 1.0f32;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (vix, viy) = poly[i];
        let (vjx, vjy) = poly[j];
        let (ex, ey) = (vjx - vix, vjy - viy);
        let (wx, wy) = (px - vix, py - viy);
        let t = clamp01((wx * ex + wy * ey) / (ex * ex + ey * ey));
        let (bx, by) = (wx - ex * t, wy - ey * t);
        d = d.min(bx * bx + by * by);
        let c1 = py >= viy;
        let c2 = py < vjy;
        let c3 = ex * wy > ey * wx;
        if (c1 && c2 && c3) || (!c1 && !c2 && !c3) {
            sign = -sign;
        }
        j = i;
    }
    sign * d.sqrt()
}

/// Pixel-space placement of the mark for one render size and view box.
struct MarkLayout {
    /// Pixels per design unit.
    k: f32,
    /// Pixel-space translation (includes grid-fitting hint).
    tx: f32,
    ty: f32,
    /// Seam edges in pixel space (pixel-aligned, at least 1 px wide).
    seam_l: f32,
    seam_r: f32,
    /// Pixel-space bounding box of the pointer (for fast rejection).
    bbox: (f32, f32, f32, f32),
}

impl MarkLayout {
    /// `view` = (min_x, min_y, size) of the square view box in design units.
    fn new(size: u32, view: (f32, f32, f32)) -> Self {
        let k = size as f32 / view.2;
        // Grid-fit: snap the spine and the wing underside to whole pixels.
        let spine = (SPINE_X - view.0) * k;
        let wing = (WING_Y - view.1) * k;
        let tx = -view.0 * k + (spine.round() - spine);
        let ty = -view.1 * k + (wing.round() - wing);
        let seam_l = (SEAM_L * k + tx).round();
        let seam_r = seam_l + (SEAM_W * k).round().max(1.0);
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in &POINTER {
            x0 = x0.min(x * k + tx);
            y0 = y0.min(y * k + ty);
            x1 = x1.max(x * k + tx);
            y1 = y1.max(y * k + ty);
        }
        Self {
            k,
            tx,
            ty,
            seam_l,
            seam_r,
            bbox: (x0 - 1.0, y0 - 1.0, x1 + 1.0, y1 + 1.0),
        }
    }

    /// White-mark alpha at a pixel-space sample position: solid body + faded sliver.
    fn alpha(&self, sx: f32, sy: f32) -> f32 {
        let (x0, y0, x1, y1) = self.bbox;
        if sx < x0 || sx > x1 || sy < y0 || sy > y1 {
            return 0.0;
        }
        let ux = (sx - self.tx) / self.k;
        let uy = (sy - self.ty) / self.k;
        let d_ptr = sd_polygon(ux, uy, &POINTER) * self.k;
        let ghost = sample_coverage(d_ptr.max(sx - self.seam_l));
        let body = sample_coverage(d_ptr.max(self.seam_r - sx));
        body + GHOST_OPACITY * ghost
    }
}

/// Renders the Glide icon as straight RGBA8 (`size * size * 4` bytes).
pub fn render_rgba(size: u32) -> Vec<u8> {
    let size = size.max(1);
    let s = size as usize;
    let sf = size as f32;
    let mut out = vec![0u8; s * s * 4];

    let mark = MarkLayout::new(size, (0.0, 0.0, 64.0));
    // Badge, grid-fitted so its straight sides land on whole pixels.
    let k = sf / 64.0;
    let inset = (BADGE_INSET * k).round();
    let half = sf * 0.5 - inset;
    let radius = 2.0 * half * BADGE_RADIUS_RATIO;
    let span = (2.0 * half).max(1.0);

    let n = (SS * SS) as f32;
    for y in 0..s {
        for x in 0..s {
            let mut acc_a = 0.0f32;
            let mut acc = [0.0f32; 3];
            for j in 0..SS {
                let sy = y as f32 + (j as f32 + 0.5) / SS as f32;
                for i in 0..SS {
                    let sx = x as f32 + (i as f32 + 0.5) / SS as f32;
                    let d_bg = sd_rounded_box(sx - sf * 0.5, sy - sf * 0.5, half, half, radius);
                    let bg = sample_coverage(d_bg);
                    if bg <= 0.0 {
                        continue;
                    }
                    let t = clamp01(((sx - inset) + (sy - inset)) / (2.0 * span));
                    let m = mark.alpha(sx, sy);
                    for c in 0..3 {
                        let base = GRAD_FROM[c] + (GRAD_TO[c] - GRAD_FROM[c]) * t;
                        acc[c] += bg * (base * (1.0 - m) + 255.0 * m);
                    }
                    acc_a += bg;
                }
            }
            if acc_a <= 0.0 {
                continue;
            }
            let idx = (y * s + x) * 4;
            for c in 0..3 {
                out[idx + c] = (acc[c] / acc_a).round().clamp(0.0, 255.0) as u8;
            }
            out[idx + 3] = (acc_a / n * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Renders the symbolic glyph (white mark on transparent) as straight RGBA8.
pub fn render_symbolic_rgba(size: u32) -> Vec<u8> {
    let size = size.max(1);
    let s = size as usize;
    let mut out = vec![0u8; s * s * 4];
    let mark = MarkLayout::new(size, (7.5, 4.75, 52.0));
    let n = (SS * SS) as f32;
    for y in 0..s {
        for x in 0..s {
            let mut a = 0.0f32;
            for j in 0..SS {
                let sy = y as f32 + (j as f32 + 0.5) / SS as f32;
                for i in 0..SS {
                    let sx = x as f32 + (i as f32 + 0.5) / SS as f32;
                    a += mark.alpha(sx, sy);
                }
            }
            let idx = (y * s + x) * 4;
            out[idx] = 255;
            out[idx + 1] = 255;
            out[idx + 2] = 255;
            out[idx + 3] = (a / n * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn rgba_to_argb32_be(rgba: &[u8]) -> Vec<u8> {
    let mut argb = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        argb.push(px[3]); // A
        argb.push(px[0]); // R
        argb.push(px[1]); // G
        argb.push(px[2]); // B
    }
    argb
}

/// Renders ARGB32 big-endian bytes required by freedesktop `StatusNotifierItem` (`ksni::Icon`).
pub fn render_argb32_be(size: u32) -> Vec<u8> {
    rgba_to_argb32_be(&render_rgba(size))
}

/// Renders the white symbolic glyph as ARGB32 big-endian bytes (Linux tray / top bar).
pub fn render_symbolic_argb32_be(size: u32) -> Vec<u8> {
    rgba_to_argb32_be(&render_symbolic_rgba(size))
}

/// Renders `[width, height, 0xAARRGGBB, ...]` for X11 `_NET_WM_ICON`.
pub fn render_x11_net_wm_icon(size: u32) -> Vec<u32> {
    let rgba = render_rgba(size);
    let mut data = Vec::with_capacity(2 + (size as usize) * (size as usize));
    data.push(size);
    data.push(size);
    for px in rgba.chunks_exact(4) {
        let word = ((px[3] as u32) << 24)
            | ((px[0] as u32) << 16)
            | ((px[1] as u32) << 8)
            | (px[2] as u32);
        data.push(word);
    }
    data
}

/// Renders BGRA8 pixels (top-down) for Windows `CreateDIBSection` / `CreateIconIndirect`.
pub fn render_bgra(size: u32) -> Vec<u8> {
    let rgba = render_rgba(size);
    let mut bgra = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        bgra.push(px[2]); // B
        bgra.push(px[1]); // G
        bgra.push(px[0]); // R
        bgra.push(px[3]); // A
    }
    bgra
}

fn encode_png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, size, size);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = encoder.write_header() {
            let _ = writer.write_image_data(rgba);
        }
    }
    buf
}

/// Encodes the Glide icon as a crisp PNG in memory (for `/favicon.png`).
pub fn render_png(size: u32) -> Vec<u8> {
    let size = size.max(1);
    encode_png(size, &render_rgba(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_png_and_argb_icon() {
        let png_bytes = render_png(64);
        assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
        let argb = render_argb32_be(32);
        assert_eq!(argb.len(), 32 * 32 * 4);
        let net_wm = render_x11_net_wm_icon(32);
        assert_eq!(net_wm.len(), 2 + 32 * 32);
        assert_eq!(net_wm[0], 32);
        assert_eq!(net_wm[1], 32);
    }

    #[test]
    fn symbolic_is_white_on_transparent_and_crisp() {
        for size in [16u32, 22, 24, 32, 48] {
            let argb = render_symbolic_argb32_be(size);
            assert_eq!(argb.len(), (size * size * 4) as usize);
            assert!(argb
                .chunks_exact(4)
                .all(|p| p[1] == 255 && p[2] == 255 && p[3] == 255));
            // Corners are transparent, and the solid body reaches full opacity.
            assert_eq!(argb[0], 0);
            assert!(argb.chunks_exact(4).any(|p| p[0] == 255));
        }
    }

    #[test]
    fn embedded_svgs_match_assets() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/logo");
        let color = std::fs::read_to_string(format!("{dir}/glide.svg")).expect("glide.svg");
        let symbolic = std::fs::read_to_string(format!("{dir}/glide-symbolic.svg"))
            .expect("glide-symbolic.svg");
        assert_eq!(color, GLIDE_ICON_SVG);
        assert_eq!(symbolic, GLIDE_SYMBOLIC_SVG);
    }

    /// Dumps preview PNGs: `GLIDE_ICON_DUMP_DIR=/some/dir cargo test -p glide-backend -- --ignored dump`
    #[test]
    #[ignore]
    fn dump_preview_pngs() {
        let Ok(dir) = std::env::var("GLIDE_ICON_DUMP_DIR") else {
            return;
        };
        std::fs::create_dir_all(&dir).expect("create dump dir");
        for size in [16u32, 22, 24, 32, 48, 64, 128, 256] {
            std::fs::write(format!("{dir}/rust_color_{size}.png"), render_png(size))
                .expect("write");
            let sym = encode_png(size, &render_symbolic_rgba(size));
            std::fs::write(format!("{dir}/rust_symbolic_{size}.png"), sym).expect("write");
        }
    }
}
