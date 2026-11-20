//! Pure-Rust Windows `CF_DIB` / `CF_DIBV5` ↔ `PNG` image converter for universal
//! screenshot clipboard interoperability between Windows 11 and Linux/macOS.

use anyhow::{anyhow, Context, Result};
use std::io::Cursor;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

/// Converts a Windows `CF_DIB` or `CF_DIBV5` memory block into a standard RGBA PNG.
///
/// Supports 24-bit BGR (`BI_RGB`) and 32-bit BGRA (`BI_RGB` / `BI_BITFIELDS`),
/// both bottom-up (`height > 0`) and top-down (`height < 0`).
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    if dib.len() < 40 {
        return Err(anyhow!("CF_DIB buffer too short ({} bytes)", dib.len()));
    }
    let header_size = u32::from_le_bytes(dib[0..4].try_into().unwrap()) as usize;
    if header_size < 40 || dib.len() < header_size {
        return Err(anyhow!("invalid BITMAPINFOHEADER size {header_size}"));
    }

    let width = i32::from_le_bytes(dib[4..8].try_into().unwrap());
    let raw_height = i32::from_le_bytes(dib[8..12].try_into().unwrap());
    let bpp = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32::from_le_bytes(dib[16..20].try_into().unwrap());

    if width <= 0 || raw_height == 0 || width > 16384 || raw_height.unsigned_abs() > 16384 {
        return Err(anyhow!("unsupported DIB dimensions {width}x{raw_height}"));
    }
    let w = width as usize;
    let h = raw_height.unsigned_abs() as usize;
    let bottom_up = raw_height > 0;

    if compression != BI_RGB && compression != BI_BITFIELDS {
        return Err(anyhow!("unsupported DIB compression {compression}"));
    }
    if bpp != 24 && bpp != 32 {
        return Err(anyhow!("unsupported DIB bit depth {bpp}"));
    }

    // Standard BITMAPINFOHEADER (40 bytes) with BI_BITFIELDS has 12 extra mask bytes immediately after header.
    let pixel_offset = if header_size == 40 && compression == BI_BITFIELDS {
        header_size + 12
    } else {
        header_size
    };

    let bytes_per_pixel = (bpp / 8) as usize;
    let row_stride = (w * bytes_per_pixel).div_ceil(4) * 4;
    let required = pixel_offset + row_stride * h;
    if dib.len() < required {
        return Err(anyhow!(
            "CF_DIB pixel data truncated ({} < {required})",
            dib.len()
        ));
    }
    let pixels = &dib[pixel_offset..required];

    let mut rgba = vec![0u8; w * h * 4];
    let mut any_non_zero_alpha = false;

    for dst_y in 0..h {
        let src_y = if bottom_up { h - 1 - dst_y } else { dst_y };
        let src_row = &pixels[src_y * row_stride..(src_y + 1) * row_stride];
        let dst_row = &mut rgba[dst_y * w * 4..(dst_y + 1) * w * 4];

        for x in 0..w {
            let s = x * bytes_per_pixel;
            let d = x * 4;
            dst_row[d] = src_row[s + 2]; // R
            dst_row[d + 1] = src_row[s + 1]; // G
            dst_row[d + 2] = src_row[s]; // B
            let a = if bytes_per_pixel == 4 {
                let av = src_row[s + 3];
                if av != 0 {
                    any_non_zero_alpha = true;
                }
                av
            } else {
                255
            };
            dst_row[d + 3] = a;
        }
    }

    // Many Windows apps (e.g. PrintScreen / GDI BitBlt) leave the 32-bit alpha channel all zeros.
    // Force opaque alpha=255 when every single pixel has alpha == 0.
    if bytes_per_pixel == 4 && !any_non_zero_alpha {
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("PNG write_header failed")?;
        writer
            .write_image_data(&rgba)
            .context("PNG write_image_data failed")?;
    }
    Ok(out)
}

/// Converts a standard PNG into a Windows 32-bit bottom-up BGRA `CF_DIB` memory block.
pub fn png_to_dib(png_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = png::Decoder::new(Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("PNG decode header failed")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .context("PNG decode frame failed")?;
    let raw = &buf[..info.buffer_size()];

    let w = info.width as usize;
    let h = info.height as usize;
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(anyhow!("unsupported PNG dimensions {w}x{h}"));
    }

    let pixel_bytes = w * h * 4;
    let mut dib = vec![0u8; 40 + pixel_bytes];

    // Write 40-byte BITMAPINFOHEADER
    dib[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
    dib[4..8].copy_from_slice(&(w as i32).to_le_bytes()); // biWidth
    dib[8..12].copy_from_slice(&(h as i32).to_le_bytes()); // biHeight (positive = bottom-up)
    dib[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib[14..16].copy_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib[16..20].copy_from_slice(&BI_RGB.to_le_bytes()); // biCompression
    dib[20..24].copy_from_slice(&(pixel_bytes as u32).to_le_bytes()); // biSizeImage

    let dst_pixels = &mut dib[40..];
    for src_y in 0..h {
        let dst_y = h - 1 - src_y;
        for x in 0..w {
            let (r, g, b, a) = match info.color_type {
                png::ColorType::Rgba => {
                    let i = (src_y * w + x) * 4;
                    (raw[i], raw[i + 1], raw[i + 2], raw[i + 3])
                }
                png::ColorType::Rgb => {
                    let i = (src_y * w + x) * 3;
                    (raw[i], raw[i + 1], raw[i + 2], 255)
                }
                png::ColorType::GrayscaleAlpha => {
                    let i = (src_y * w + x) * 2;
                    (raw[i], raw[i], raw[i], raw[i + 1])
                }
                png::ColorType::Grayscale => {
                    let v = raw[src_y * w + x];
                    (v, v, v, 255)
                }
                png::ColorType::Indexed => unreachable!("EXPAND transforms indexed to RGB(A)"),
            };
            let d = (dst_y * w + x) * 4;
            dst_pixels[d] = b;
            dst_pixels[d + 1] = g;
            dst_pixels[d + 2] = r;
            dst_pixels[d + 3] = a;
        }
    }

    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dib_png_roundtrip_preserves_pixels() {
        // Build a 2x2 32-bit bottom-up DIB with distinct BGR colors (alpha=0 like GDI BitBlt)
        let mut dib = vec![0u8; 40 + 2 * 2 * 4];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        // Bottom row (y=1): Blue, Green
        dib[40..44].copy_from_slice(&[255, 0, 0, 0]);
        dib[44..48].copy_from_slice(&[0, 255, 0, 0]);
        // Top row (y=0): Red, White
        dib[48..52].copy_from_slice(&[0, 0, 255, 0]);
        dib[52..56].copy_from_slice(&[255, 255, 255, 0]);

        let png_bytes = dib_to_png(&dib).unwrap();
        assert_eq!(&png_bytes[0..8], b"\x89PNG\r\n\x1a\n");

        let dib2 = png_to_dib(&png_bytes).unwrap();
        // Top-left pixel (src_y=0 -> dst_y=1 -> offset 40 + 8 = 48) must be Red (B=0,G=0,R=255,A=255)
        assert_eq!(&dib2[48..52], &[0, 0, 255, 255]);
        // Bottom-left pixel (src_y=1 -> dst_y=0 -> offset 40) must be Blue (B=255,G=0,R=0,A=255)
        assert_eq!(&dib2[40..44], &[255, 0, 0, 255]);
    }
}
