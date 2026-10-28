//! Decode JPEG and 24/32-bit BMP into RGB565, scaled to fit a box.

use alloc::vec::Vec;
use p4os_sdk::{Color, Image};

pub fn decode(data: &[u8], max_w: i32, max_h: i32) -> Option<Image> {
    let (w, h, rgb) = if data.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(data)?
    } else if data.starts_with(b"BM") {
        decode_bmp(data)?
    } else {
        return None;
    };
    Some(fit(w, h, &rgb, max_w, max_h))
}

fn decode_jpeg(data: &[u8]) -> Option<(i32, i32, Vec<u8>)> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_max_width(8192)
        .set_max_height(8192);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(data, opts);
    let px = dec.decode().ok()?;
    let (w, h) = dec.dimensions()?;
    Some((w as i32, h as i32, px))
}

fn le32(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(i..i + 4)?.try_into().ok()?))
}

fn decode_bmp(d: &[u8]) -> Option<(i32, i32, Vec<u8>)> {
    let offset = le32(d, 10)? as usize;
    let w = le32(d, 18)? as i32;
    let raw_h = le32(d, 22)? as i32;
    let bpp = u16::from_le_bytes(d.get(28..30)?.try_into().ok()?) as usize;
    if bpp != 24 && bpp != 32 || w <= 0 {
        return None;
    }
    let (h, bottom_up) = if raw_h < 0 { (-raw_h, false) } else { (raw_h, true) };
    let stride = (w as usize * bpp / 8 + 3) & !3;
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h as usize {
        let src_y = if bottom_up { h as usize - 1 - y } else { y };
        let row = d.get(offset + src_y * stride..offset + src_y * stride + w as usize * bpp / 8)?;
        for px in row.chunks(bpp / 8) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    Some((w, h, rgb))
}

/// Downscale (box filter, never upscale) RGB888 into RGB565.
fn fit(w: i32, h: i32, rgb: &[u8], max_w: i32, max_h: i32) -> Image {
    // Integer scale factor keeps the box filter simple and fast.
    let mut s = 1;
    while w / s > max_w || h / s > max_h {
        s += 1;
    }
    let (ow, oh) = (w / s, h / s);
    let mut pixels = Vec::with_capacity((ow * oh) as usize);
    let n = (s * s) as u32;
    for oy in 0..oh {
        for ox in 0..ow {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for dy in 0..s {
                let row = ((oy * s + dy) * w + ox * s) as usize * 3;
                for dx in 0..s as usize {
                    r += rgb[row + dx * 3] as u32;
                    g += rgb[row + dx * 3 + 1] as u32;
                    b += rgb[row + dx * 3 + 2] as u32;
                }
            }
            pixels.push(Color::rgb((r / n) as u8, (g / n) as u8, (b / n) as u8));
        }
    }
    Image { width: ow, height: oh, pixels }
}
