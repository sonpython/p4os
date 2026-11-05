//! YUV 4:2:0 (BT.709, limited range, as YouTube encodes) to RGB565.
//!
//! Table driven: one lookup per plane value and one per output channel, so
//! a pixel costs a handful of loads and adds on a core without SIMD.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use p4os_sdk::Color;

/// Offset that keeps every intermediate channel value a valid table index.
const BIAS: i32 = 512;
const SPAN: usize = 1536;

pub struct YuvToRgb {
    /// 1.164 * (Y - 16).
    y: [i16; 256],
    /// 1.793 * (V - 128).
    rv: [i16; 256],
    /// -0.213 * (U - 128) and -0.533 * (V - 128).
    gu: [i16; 256],
    gv: [i16; 256],
    /// 2.112 * (U - 128).
    bu: [i16; 256],
    /// Clamped channel value already shifted into its RGB565 position.
    r: Vec<u16>,
    g: Vec<u16>,
    b: Vec<u16>,
}

fn scaled(v: i32, k: f32) -> i16 {
    libm::roundf(v as f32 * k) as i16
}

impl YuvToRgb {
    pub fn new() -> Box<Self> {
        let mut t = Box::new(YuvToRgb {
            y: [0; 256],
            rv: [0; 256],
            gu: [0; 256],
            gv: [0; 256],
            bu: [0; 256],
            r: vec![0; SPAN],
            g: vec![0; SPAN],
            b: vec![0; SPAN],
        });
        for i in 0..256 {
            let v = i as i32;
            t.y[i] = scaled(v - 16, 1.164);
            t.rv[i] = scaled(v - 128, 1.793);
            t.gu[i] = scaled(v - 128, -0.213);
            t.gv[i] = scaled(v - 128, -0.533);
            t.bu[i] = scaled(v - 128, 2.112);
        }
        for i in 0..SPAN {
            let c = (i as i32 - BIAS).clamp(0, 255) as u16;
            t.r[i] = (c & 0xF8) << 8;
            t.g[i] = (c & 0xFC) << 3;
            t.b[i] = c >> 3;
        }
        t
    }

    /// Convert a `w` x `h` picture into `out` (`w * h` pixels, row-major).
    /// Odd widths and heights use the chroma sample of the pair they
    /// belong to, as the planes are `ceil(w/2)` x `ceil(h/2)`.
    pub fn convert(&self, y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize, out: &mut [Color]) {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        assert!(y.len() >= w * h && out.len() >= w * h && u.len() >= cw * ch && v.len() >= cw * ch, "plane sizes");
        let (rt, gt, bt) = (&self.r[..SPAN], &self.g[..SPAN], &self.b[..SPAN]);
        for row in 0..h {
            let yrow = &y[row * w..row * w + w];
            let urow = &u[(row / 2) * cw..(row / 2) * cw + cw];
            let vrow = &v[(row / 2) * cw..(row / 2) * cw + cw];
            let orow = &mut out[row * w..row * w + w];
            // Two pixels share one chroma sample: look its offsets up once.
            let pairs = orow.chunks_exact_mut(2).zip(yrow.chunks_exact(2)).zip(urow.iter().zip(vrow.iter()));
            for ((o, yy), (&cu, &cv)) in pairs {
                let ro = self.rv[cv as usize] as i32 + BIAS;
                let go = self.gu[cu as usize] as i32 + self.gv[cv as usize] as i32 + BIAS;
                let bo = self.bu[cu as usize] as i32 + BIAS;
                for k in 0..2 {
                    let l = self.y[yy[k] as usize] as i32;
                    o[k] = Color(rt[(l + ro) as usize] | gt[(l + go) as usize] | bt[(l + bo) as usize]);
                }
            }
            if w % 2 == 1 {
                let (cu, cv) = (urow[cw - 1] as usize, vrow[cw - 1] as usize);
                let l = self.y[yrow[w - 1] as usize] as i32 + BIAS;
                let r = (l + self.rv[cv] as i32) as usize;
                let g = (l + self.gu[cu] as i32 + self.gv[cv] as i32) as usize;
                let b = (l + self.bu[cu] as i32) as usize;
                orow[w - 1] = Color(rt[r] | gt[g] | bt[b]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_reference_colours() {
        let t = YuvToRgb::new();
        let mut out = [Color(0); 4];
        // Black, white and mid grey at limited range.
        for (yv, expect) in [(16u8, (0u8, 0u8, 0u8)), (235, (255, 255, 255)), (126, (128, 128, 128))] {
            t.convert(&[yv; 4], &[128], &[128], 2, 2, &mut out);
            let c = out[0];
            assert!(c.r().abs_diff(expect.0) <= 8 && c.g().abs_diff(expect.1) <= 4 && c.b().abs_diff(expect.2) <= 8, "{yv}: {c:?}");
        }
        // BT.709 pure red: Y 63, Cb 102, Cr 240.
        t.convert(&[63; 4], &[102], &[240], 2, 2, &mut out);
        assert!(out[0].r() > 240 && out[0].g() < 16 && out[0].b() < 16, "{:?}", out[0]);
        // Out-of-gamut values clamp instead of wrapping.
        t.convert(&[255; 4], &[255], &[255], 2, 2, &mut out);
        assert_eq!(out[0].r(), 255);
    }

    #[test]
    fn handles_odd_sizes() {
        let t = YuvToRgb::new();
        let mut out = [Color(0); 9];
        t.convert(&[235; 9], &[128; 4], &[128; 4], 3, 3, &mut out);
        assert!(out.iter().all(|c| c.r() == 255));
    }
}
