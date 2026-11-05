//! Snapshots of an app's last frame for the app switcher.
//!
//! A snapshot is the app's screen area box-filtered down by [`SCALE`] in
//! each direction, so it costs 1/9 of the area: at most 160x246 pixels
//! (79 KB) in portrait and 266x140 (74 KB) in landscape, one per running
//! app (6 at most). Its buffer is reserved with at least [`PSRAM_MIN`]
//! bytes because the kernel heap places blocks of that size in PSRAM,
//! keeping snapshots out of the scarce internal SRAM.

use alloc::vec::Vec;
use p4os_sdk::{Color, Rect};

pub const SCALE: i32 = 3;
/// Allocations from this size go to PSRAM (`kernel/src/heap.rs`, `LARGE`).
pub const PSRAM_MIN: usize = 64 * 1024;

pub struct Thumbnail {
    pub w: i32,
    pub h: i32,
    pub pixels: Vec<Color>,
}

impl Thumbnail {
    pub fn bytes(&self) -> usize {
        self.pixels.capacity() * core::mem::size_of::<Color>()
    }
}

/// Downscale `r` of a `stride`-pixel-wide frame buffer, averaging each
/// SCALE x SCALE block. `r` must lie inside the buffer.
pub fn capture(pixels: &[Color], stride: i32, r: Rect) -> Thumbnail {
    let (w, h) = (r.w / SCALE, r.h / SCALE);
    let n = (w.max(0) * h.max(0)) as usize;
    let mut out = Vec::with_capacity(n.max(PSRAM_MIN / core::mem::size_of::<Color>()));
    let area = (SCALE * SCALE) as u32;
    for ty in 0..h {
        for tx in 0..w {
            let (mut rs, mut gs, mut bs) = (0u32, 0u32, 0u32);
            for dy in 0..SCALE {
                let row = ((r.y + ty * SCALE + dy) * stride + r.x + tx * SCALE) as usize;
                for c in &pixels[row..row + SCALE as usize] {
                    rs += c.r() as u32;
                    gs += c.g() as u32;
                    bs += c.b() as u32;
                }
            }
            out.push(Color::rgb((rs / area) as u8, (gs / area) as u8, (bs / area) as u8));
        }
    }
    Thumbnail { w, h, pixels: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_blocks() {
        let (w, h) = (9, 6);
        let mut buf = alloc::vec![Color::BLACK; (w * h) as usize];
        // Left 3x3 block white, the one beside it a third white.
        for y in 0..3 {
            for x in 0..3 {
                buf[(y * w + x) as usize] = Color::WHITE;
            }
            buf[(y * w + 3) as usize] = Color::WHITE;
        }
        let t = capture(&buf, w, Rect::new(0, 0, w, h));
        assert_eq!((t.w, t.h), (3, 2));
        assert_eq!(t.pixels[0], Color::WHITE);
        assert_eq!(t.pixels[1].r(), Color::rgb(85, 85, 85).r());
        assert_eq!(t.pixels[2], Color::BLACK);
        assert_eq!(t.pixels[3], Color::BLACK);
    }

    #[test]
    fn sub_rectangle_and_psram_sized_buffer() {
        let (w, h) = (480, 800);
        let mut buf = alloc::vec![Color::BLACK; (w * h) as usize];
        buf[(36 * w + 3) as usize] = Color::WHITE;
        let t = capture(&buf, w, Rect::new(0, 36, 480, 740));
        assert_eq!((t.w, t.h), (160, 246));
        assert_ne!(t.pixels[1], Color::BLACK);
        assert_eq!(t.pixels[0], Color::BLACK);
        assert!(t.bytes() >= PSRAM_MIN);
    }
}
