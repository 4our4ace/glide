//! Pure-Rust anti-aliased Glide app icon renderer (zero external assets).
//!
//! Used identically across:
//! - Ubuntu / Linux top-right StatusNotifierItem system tray (`ksni` ARGB32)
//! - Linux X11 `_NET_WM_ICON` window/dock/Alt+Tab icon
//! - Windows 11 `Shell_NotifyIconW` system tray icon (`HICON`)
//! - Embedded GUI `/favicon.png` & `/favicon.svg`

/// Canonical SVG vector icon matching the desktop package & favicon.
pub const GLIDE_ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="#3b82f6"/>
      <stop offset="100%" stop-color="#06b6d4"/>
    </linearGradient>
  </defs>
  <rect x="2" y="2" width="60" height="60" rx="16" fill="url(#g)"/>
  <path d="M44 33H32v-6h18v15c0 4-3 8-8 9-4 1-9 1-14-1-8-3-13-11-12-20 1-10 9-17 19-17 6 0 11 2 14 6l-5 4c-2-3-5-4-9-4-7 0-12 5-12 11 0 7 5 12 12 12 4 0 7-1 9-3v-6z" fill="#ffffff"/>
</svg>"##;

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Signed distance to a rounded rectangle centered at (0,0) with half-extents (bx, by) and radius r.
fn sd_rounded_box(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    let qx = px.abs() - bx + r;
    let qy = py.abs() - by + r;
    let out = (qx.max(0.0).hypot(qy.max(0.0))) - r;
    let inside = qx.max(qy).min(0.0);
    out + inside
}

/// Evaluates whether normalized coordinates `(u, v)` in `[-1, 1]` belong to the crisp geometric "G" glyph.
/// Returns coverage in `[0.0, 1.0]`.
fn glyph_g_coverage(u: f32, v: f32, px_step: f32) -> f32 {
    let r = u.hypot(v);
    let r_outer = 0.56;
    let r_inner = 0.31;

    // Ring SDF
    let d_ring = (r - (r_outer + r_inner) * 0.5).abs() - (r_outer - r_inner) * 0.5;

    // Open cut on upper-right quadrant (angle between -52 deg and +8 deg, with y pointing down so v < 0 is up)
    let angle = v.atan2(u); // -PI..PI, 0 = right, -PI/2 = up
    let in_cut = angle > -0.92 && angle < 0.02;

    let ring_alpha = if in_cut {
        0.0
    } else {
        clamp01(0.5 - d_ring / px_step)
    };

    // Horizontal crossbar of the 'G' (from center x=-0.02 to x=0.56, y in [-0.04, 0.19])
    let d_bar = sd_rounded_box(u - 0.27, v - 0.08, 0.29, 0.115, 0.04);
    let bar_alpha = clamp01(0.5 - d_bar / px_step);

    // Vertical stem on lower-right closing the 'G' cleanly
    let d_stem = sd_rounded_box(u - 0.44, v - 0.24, 0.12, 0.22, 0.04);
    let stem_alpha = clamp01(0.5 - d_stem / px_step);

    ring_alpha.max(bar_alpha).max(stem_alpha)
}

/// Renders the Glide icon as straight RGBA8 (`size * size * 4` bytes).
pub fn render_rgba(size: u32) -> Vec<u8> {
    let s = size.max(16) as usize;
    let mut out = vec![0u8; s * s * 4];
    let inv = 2.0 / (s as f32);
    let aa = inv * 1.15;

    for y in 0..s {
        let v = (y as f32 + 0.5) * inv - 1.0;
        for x in 0..s {
            let u = (x as f32 + 0.5) * inv - 1.0;

            // Outer rounded square badge
            let d_bg = sd_rounded_box(u, v, 0.92, 0.92, 0.44);
            let bg_alpha = clamp01(0.5 - d_bg / aa);
            if bg_alpha <= 0.001 {
                continue;
            }

            // Diagonal blue (#2563eb) -> cyan (#06b6d4) gradient
            let t = clamp01((u + v + 1.8) / 3.6);
            let mut r = 37.0 + (6.0 - 37.0) * t;
            let mut g = 99.0 + (182.0 - 99.0) * t;
            let mut b = 235.0 + (212.0 - 235.0) * t;

            // Subtle top glossy highlight inside badge
            let gloss = clamp01((-v - 0.15) * 0.45) * 18.0;
            r = (r + gloss).min(255.0);
            g = (g + gloss).min(255.0);
            b = (b + gloss).min(255.0);

            // Crisp white "G" glyph composited on top
            let ga = glyph_g_coverage(u, v, aa);
            if ga > 0.0 {
                r = r * (1.0 - ga) + 255.0 * ga;
                g = g * (1.0 - ga) + 255.0 * ga;
                b = b * (1.0 - ga) + 255.0 * ga;
            }

            let idx = (y * s + x) * 4;
            out[idx] = r.round().clamp(0.0, 255.0) as u8;
            out[idx + 1] = g.round().clamp(0.0, 255.0) as u8;
            out[idx + 2] = b.round().clamp(0.0, 255.0) as u8;
            out[idx + 3] = (bg_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Renders ARGB32 big-endian bytes required by freedesktop `StatusNotifierItem` (`ksni::Icon`).
pub fn render_argb32_be(size: u32) -> Vec<u8> {
    let rgba = render_rgba(size);
    let mut argb = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        argb.push(px[3]); // A
        argb.push(px[0]); // R
        argb.push(px[1]); // G
        argb.push(px[2]); // B
    }
    argb
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

/// Encodes the Glide icon as a crisp PNG in memory (for `/favicon.png`).
pub fn render_png(size: u32) -> Vec<u8> {
    let rgba = render_rgba(size);
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, size, size);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = encoder.write_header() {
            let _ = writer.write_image_data(&rgba);
        }
    }
    buf
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
}
