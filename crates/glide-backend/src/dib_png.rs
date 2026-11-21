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

    if bytes_per_pixel == 4 {
        let mut alpha_or = 0u8;
        for dst_y in 0..h {
            let src_y = if bottom_up { h - 1 - dst_y } else { dst_y };
            let src_row = &pixels[src_y * row_stride..src_y * row_stride + w * 4];
            let dst_row = &mut rgba[dst_y * w * 4..(dst_y + 1) * w * 4];
            for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
                d[0] = s[2]; // R
                d[1] = s[1]; // G
                d[2] = s[0]; // B
                d[3] = s[3]; // A
                alpha_or |= s[3];
            }
        }
        // Many Windows apps (e.g. PrintScreen / GDI BitBlt) leave the 32-bit alpha channel all zeros.
        // Force opaque alpha=255 when every single pixel has alpha == 0.
        if alpha_or == 0 {
            for px in rgba.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
    } else {
        for dst_y in 0..h {
            let src_y = if bottom_up { h - 1 - dst_y } else { dst_y };
            let src_row = &pixels[src_y * row_stride..src_y * row_stride + w * 3];
            let dst_row = &mut rgba[dst_y * w * 4..(dst_y + 1) * w * 4];
            for (s, d) in src_row.chunks_exact(3).zip(dst_row.chunks_exact_mut(4)) {
                d[0] = s[2]; // R
                d[1] = s[1]; // G
                d[2] = s[0]; // B
                d[3] = 255;
            }
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // Optimize for LAN clipboard latency (3-5x faster 4K screenshot encoding).
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::FilterType::Sub);
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
    match info.color_type {
        png::ColorType::Rgba => {
            for src_y in 0..h {
                let dst_y = h - 1 - src_y;
                let src_row = &raw[src_y * w * 4..(src_y + 1) * w * 4];
                let dst_row = &mut dst_pixels[dst_y * w * 4..(dst_y + 1) * w * 4];
                for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = s[3];
                }
            }
        }
        png::ColorType::Rgb => {
            for src_y in 0..h {
                let dst_y = h - 1 - src_y;
                let src_row = &raw[src_y * w * 3..(src_y + 1) * w * 3];
                let dst_row = &mut dst_pixels[dst_y * w * 4..(dst_y + 1) * w * 4];
                for (s, d) in src_row.chunks_exact(3).zip(dst_row.chunks_exact_mut(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = 255;
                }
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for src_y in 0..h {
                let dst_y = h - 1 - src_y;
                let src_row = &raw[src_y * w * 2..(src_y + 1) * w * 2];
                let dst_row = &mut dst_pixels[dst_y * w * 4..(dst_y + 1) * w * 4];
                for (s, d) in src_row.chunks_exact(2).zip(dst_row.chunks_exact_mut(4)) {
                    d[0] = s[0];
                    d[1] = s[0];
                    d[2] = s[0];
                    d[3] = s[1];
                }
            }
        }
        png::ColorType::Grayscale => {
            for src_y in 0..h {
                let dst_y = h - 1 - src_y;
                let src_row = &raw[src_y * w..(src_y + 1) * w];
                let dst_row = &mut dst_pixels[dst_y * w * 4..(dst_y + 1) * w * 4];
                for (&v, d) in src_row.iter().zip(dst_row.chunks_exact_mut(4)) {
                    d[0] = v;
                    d[1] = v;
                    d[2] = v;
                    d[3] = 255;
                }
            }
        }
        png::ColorType::Indexed => unreachable!("EXPAND transforms indexed to RGB(A)"),
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

    #[test]
    fn dib24_roundtrip_handles_row_padding() {
        // 1x2 24-bit bottom-up DIB: each row is 3 bytes + 1 padding byte = 4-byte stride.
        let mut dib = vec![0u8; 40 + 2 * 4];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        // Bottom row: BGR = (10, 20, 30), pad = 0xAA
        dib[40..44].copy_from_slice(&[10, 20, 30, 0xAA]);
        // Top row: BGR = (40, 50, 60), pad = 0xBB
        dib[44..48].copy_from_slice(&[40, 50, 60, 0xBB]);

        let png_bytes = dib_to_png(&dib).unwrap();
        let dib2 = png_to_dib(&png_bytes).unwrap();
        assert_eq!(&dib2[40..44], &[10, 20, 30, 255]);
        assert_eq!(&dib2[44..48], &[40, 50, 60, 255]);
    }
}
