//! Pure-Rust Windows `CF_DIB` / `CF_DIBV5` ↔ `PNG` image converter for universal
//! screenshot clipboard interoperability between Windows 11 and Linux/macOS.

use anyhow::{anyhow, Context, Result};
use std::io::Cursor;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
/// Maximum uncompressed RGBA pixel buffer (256 MiB = 64 megapixels, larger than dual 8K).
const MAX_UNCOMPRESSED_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct BitMask {
    mask: u32,
    shift: u32,
    max: u32,
}

impl BitMask {
    fn from_mask(mask: u32) -> Option<Self> {
        if mask == 0 {
            return None;
        }
        let shift = mask.trailing_zeros();
        let max = mask >> shift;
        // Reject non-contiguous bitmasks (e.g. 0x00F0_000F).
        if max == 0 || (max & (max + 1)) != 0 {
            return None;
        }
        Some(Self { mask, shift, max })
    }

    #[inline]
    fn extract_u8(self, pixel: u32) -> u8 {
        let val = (pixel & self.mask) >> self.shift;
        if self.max == 255 {
            val as u8
        } else {
            (((val as u64) * 255 + (self.max as u64) / 2) / (self.max as u64)) as u8
        }
    }
}

/// Converts a Windows `CF_DIB` or `CF_DIBV5` memory block into a standard RGBA PNG.
///
/// Supports 24-bit BGR (`BI_RGB`) and 32-bit BGRA/RGBA (`BI_RGB` / `BI_BITFIELDS`),
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

    let rgba_bytes = w
        .checked_mul(h)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n <= MAX_UNCOMPRESSED_BYTES)
        .ok_or_else(|| anyhow!("DIB dimensions {w}x{h} exceed memory limit"))?;

    if compression != BI_RGB && compression != BI_BITFIELDS {
        return Err(anyhow!("unsupported DIB compression {compression}"));
    }
    if bpp != 24 && bpp != 32 {
        return Err(anyhow!("unsupported DIB bit depth {bpp}"));
    }
    if compression == BI_BITFIELDS && bpp != 32 {
        return Err(anyhow!("BI_BITFIELDS is only supported for 32-bit DIBs"));
    }

    // Standard BITMAPINFOHEADER (40 bytes) with BI_BITFIELDS has 12 extra mask bytes immediately after header.
    let pixel_offset = if header_size == 40 && compression == BI_BITFIELDS {
        header_size
            .checked_add(12)
            .ok_or_else(|| anyhow!("DIB header offset overflow"))?
    } else {
        header_size
    };

    // Parse explicit RGB(A) channel masks when BI_BITFIELDS is used.
    let bitfields = if compression == BI_BITFIELDS {
        if dib.len() < 52 || (header_size > 40 && header_size < 52) {
            return Err(anyhow!("BI_BITFIELDS DIB missing channel masks"));
        }
        let r_raw = u32::from_le_bytes(dib[40..44].try_into().unwrap());
        let g_raw = u32::from_le_bytes(dib[44..48].try_into().unwrap());
        let b_raw = u32::from_le_bytes(dib[48..52].try_into().unwrap());
        let a_raw = if header_size >= 56 {
            u32::from_le_bytes(dib[52..56].try_into().unwrap())
        } else {
            0
        };
        let r = BitMask::from_mask(r_raw)
            .ok_or_else(|| anyhow!("invalid BI_BITFIELDS red mask 0x{r_raw:08X}"))?;
        let g = BitMask::from_mask(g_raw)
            .ok_or_else(|| anyhow!("invalid BI_BITFIELDS green mask 0x{g_raw:08X}"))?;
        let b = BitMask::from_mask(b_raw)
            .ok_or_else(|| anyhow!("invalid BI_BITFIELDS blue mask 0x{b_raw:08X}"))?;
        let a = BitMask::from_mask(a_raw);
        Some((r, g, b, a))
    } else {
        None
    };

    let bytes_per_pixel = (bpp / 8) as usize;
    let row_stride = w
        .checked_mul(bytes_per_pixel)
        .map(|n| n.div_ceil(4) * 4)
        .ok_or_else(|| anyhow!("DIB stride overflow"))?;
    let pixel_data_len = row_stride
        .checked_mul(h)
        .ok_or_else(|| anyhow!("DIB pixel buffer size overflow"))?;
    let required = pixel_offset
        .checked_add(pixel_data_len)
        .ok_or_else(|| anyhow!("DIB size overflow"))?;
    if dib.len() < required {
        return Err(anyhow!(
            "CF_DIB pixel data truncated ({} < {required})",
            dib.len()
        ));
    }
    let pixels = &dib[pixel_offset..required];

    let mut rgba = vec![0u8; rgba_bytes];
    let mut any_non_zero_alpha = false;
    let has_alpha_channel = match bitfields {
        Some((_, _, _, a)) => a.is_some(),
        None => bytes_per_pixel == 4,
    };

    for dst_y in 0..h {
        let src_y = if bottom_up { h - 1 - dst_y } else { dst_y };
        let src_row = &pixels[src_y * row_stride..(src_y + 1) * row_stride];
        let dst_row = &mut rgba[dst_y * w * 4..(dst_y + 1) * w * 4];

        if let Some((rm, gm, bm, am)) = bitfields {
            for x in 0..w {
                let s = x * 4;
                let d = x * 4;
                let px = u32::from_le_bytes([
                    src_row[s],
                    src_row[s + 1],
                    src_row[s + 2],
                    src_row[s + 3],
                ]);
                dst_row[d] = rm.extract_u8(px);
                dst_row[d + 1] = gm.extract_u8(px);
                dst_row[d + 2] = bm.extract_u8(px);
                let a = if let Some(am) = am {
                    let av = am.extract_u8(px);
                    if av != 0 {
                        any_non_zero_alpha = true;
                    }
                    av
                } else {
                    255
                };
                dst_row[d + 3] = a;
            }
        } else {
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
    }

    // Many Windows apps (e.g. PrintScreen / GDI BitBlt) leave the 32-bit alpha channel all zeros.
    // Force opaque alpha=255 when every single pixel has alpha == 0.
    if has_alpha_channel && !any_non_zero_alpha {
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

    // Validate dimensions BEFORE allocating the decode buffer so crafted headers cannot OOM.
    let hdr = reader.info();
    let hdr_w = hdr.width as usize;
    let hdr_h = hdr.height as usize;
    if hdr_w == 0 || hdr_h == 0 || hdr_w > 16384 || hdr_h > 16384 {
        return Err(anyhow!("unsupported PNG dimensions {hdr_w}x{hdr_h}"));
    }
    let max_dib_pixels = hdr_w
        .checked_mul(hdr_h)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n <= MAX_UNCOMPRESSED_BYTES)
        .ok_or_else(|| anyhow!("PNG dimensions {hdr_w}x{hdr_h} exceed memory limit"))?;
    let out_buf_size = reader.output_buffer_size();
    if out_buf_size > max_dib_pixels {
        return Err(anyhow!(
            "PNG output buffer size {out_buf_size} exceeds safe limit {max_dib_pixels}"
        ));
    }

    let mut buf = vec![0u8; out_buf_size];
    let info = reader
        .next_frame(&mut buf)
        .context("PNG decode frame failed")?;
    let raw = &buf[..info.buffer_size()];

    let w = info.width as usize;
    let h = info.height as usize;
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(anyhow!("unsupported PNG dimensions {w}x{h}"));
    }

    let pixel_bytes = w
        .checked_mul(h)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n <= MAX_UNCOMPRESSED_BYTES)
        .ok_or_else(|| anyhow!("PNG dimensions {w}x{h} exceed memory limit"))?;
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

    #[test]
    fn dib_bi_bitfields_respects_rgba_channel_masks() {
        // 1x1 top-down 32-bit DIB with BI_BITFIELDS in RGBA byte order (R=0xFF, G=0xFF00, B=0xFF0000)
        let mut dib = vec![0u8; 40 + 12 + 4];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1i32.to_le_bytes());
        dib[8..12].copy_from_slice(&(-1i32).to_le_bytes()); // top-down
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&BI_BITFIELDS.to_le_bytes());
        dib[40..44].copy_from_slice(&0x0000_00FFu32.to_le_bytes()); // R
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes()); // G
        dib[48..52].copy_from_slice(&0x00FF_0000u32.to_le_bytes()); // B
        // Pixel: R=0xAA, G=0xBB, B=0xCC, X=0x00
        dib[52..56].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0x00]);

        let png_bytes = dib_to_png(&dib).unwrap();
        let dib2 = png_to_dib(&png_bytes).unwrap();
        // png_to_dib outputs standard BGRA: [B=0xCC, G=0xBB, R=0xAA, A=0xFF]
        assert_eq!(&dib2[40..44], &[0xCC, 0xBB, 0xAA, 0xFF]);
    }

    #[test]
    fn png_to_dib_rejects_oversized_dimensions_before_allocating() {
        // Build a valid 1x1 PNG and patch IHDR width/height to 100000x100000 (with valid CRC)
        // so read_info() succeeds, proving png_to_dib rejects it before allocating 40 GB.
        let mut valid_png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut valid_png, 1, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header()
                .unwrap()
                .write_image_data(&[1, 2, 3, 255])
                .unwrap();
        }
        // IHDR chunk starts at byte 8: [len:4][type "IHDR":4][width:4][height:4][5 bytes][crc:4]
        assert_eq!(&valid_png[12..16], b"IHDR");
        valid_png[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        valid_png[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        // Recompute PNG IEEE CRC-32 over chunk type + data (bytes 12..29)
        let mut crc: u32 = 0xFFFF_FFFF;
        for &byte in &valid_png[12..29] {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if (crc & 1) != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        valid_png[29..33].copy_from_slice(&(!crc).to_be_bytes());

        let err = png_to_dib(&valid_png).unwrap_err();
        assert!(
            err.to_string().contains("unsupported PNG dimensions"),
            "unexpected error: {err}"
        );
    }
}
