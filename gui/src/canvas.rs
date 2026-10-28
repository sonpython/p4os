//! Drawing onto an RGB565 pixel buffer.
//!
//! All coordinates are absolute screen pixels. Every primitive is clipped to
//! `clip`, so widgets can draw freely inside the area they were given.

use crate::color::Color;
use crate::geom::{Point, Rect};
use libm::sqrtf;

pub struct Canvas<'a> {
    buf: &'a mut [Color],
    width: i32,
    height: i32,
    clip: Rect,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [Color], width: i32, height: i32) -> Self {
        assert!(buf.len() >= (width * height) as usize);
        Self { buf, width, height, clip: Rect::new(0, 0, width, height) }
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Restrict drawing to `r` (intersected with the current clip) and return
    /// the previous clip so callers can restore it.
    pub fn set_clip(&mut self, r: Rect) -> Rect {
        let old = self.clip;
        self.clip = r.intersect(&self.bounds());
        old
    }

    pub fn restore_clip(&mut self, old: Rect) {
        self.clip = old;
    }

    pub fn pixels(&self) -> &[Color] {
        self.buf
    }

    #[inline]
    fn idx(&self, x: i32, y: i32) -> usize {
        (y * self.width + x) as usize
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32) -> Color {
        self.buf[self.idx(x, y)]
    }

    /// Blend one pixel with coverage `alpha`, honouring the clip.
    #[inline]
    pub fn blend_pixel(&mut self, x: i32, y: i32, c: Color, alpha: u8) {
        if alpha == 0 || !self.clip.contains(Point::new(x, y)) {
            return;
        }
        let i = self.idx(x, y);
        self.buf[i] = c.blend(self.buf[i], alpha);
    }

    pub fn fill(&mut self, c: Color) {
        let r = self.clip;
        self.fill_rect(r, c);
    }

    pub fn fill_rect(&mut self, r: Rect, c: Color) {
        let r = r.intersect(&self.clip);
        if r.is_empty() {
            return;
        }
        for y in r.y..r.bottom() {
            let start = self.idx(r.x, y);
            self.buf[start..start + r.w as usize].fill(c);
        }
    }

    pub fn fill_rect_alpha(&mut self, r: Rect, c: Color, alpha: u8) {
        let r = r.intersect(&self.clip);
        for y in r.y..r.bottom() {
            let start = self.idx(r.x, y);
            for p in &mut self.buf[start..start + r.w as usize] {
                *p = c.blend(*p, alpha);
            }
        }
    }

    /// Vertical gradient from `top` to `bottom`.
    pub fn fill_gradient(&mut self, r: Rect, top: Color, bottom: Color) {
        let h = r.h.max(1);
        for row in 0..r.h {
            let t = (row * 255 / h) as u8;
            self.fill_rect(Rect::new(r.x, r.y + row, r.w, 1), top.lerp(bottom, t));
        }
    }

    pub fn stroke_rect(&mut self, r: Rect, width: i32, c: Color) {
        self.fill_rect(Rect::new(r.x, r.y, r.w, width), c);
        self.fill_rect(Rect::new(r.x, r.bottom() - width, r.w, width), c);
        self.fill_rect(Rect::new(r.x, r.y + width, width, r.h - 2 * width), c);
        self.fill_rect(Rect::new(r.right() - width, r.y + width, width, r.h - 2 * width), c);
    }

    /// Filled rectangle with anti-aliased rounded corners.
    pub fn fill_round_rect(&mut self, r: Rect, radius: i32, c: Color) {
        let rad = radius.min(r.w / 2).min(r.h / 2).max(0);
        if rad == 0 {
            self.fill_rect(r, c);
            return;
        }
        // Straight middle band and the two side bands between the corners.
        self.fill_rect(Rect::new(r.x + rad, r.y, r.w - 2 * rad, r.h), c);
        self.fill_rect(Rect::new(r.x, r.y + rad, rad, r.h - 2 * rad), c);
        self.fill_rect(Rect::new(r.right() - rad, r.y + rad, rad, r.h - 2 * rad), c);
        let radf = rad as f32;
        for dy in 0..rad {
            for dx in 0..rad {
                // Distance from the corner circle centre to this pixel centre.
                let fx = radf - dx as f32 - 0.5;
                let fy = radf - dy as f32 - 0.5;
                let cov = coverage(radf - sqrtf(fx * fx + fy * fy));
                if cov == 0 {
                    continue;
                }
                self.blend_pixel(r.x + dx, r.y + dy, c, cov);
                self.blend_pixel(r.right() - 1 - dx, r.y + dy, c, cov);
                self.blend_pixel(r.x + dx, r.bottom() - 1 - dy, c, cov);
                self.blend_pixel(r.right() - 1 - dx, r.bottom() - 1 - dy, c, cov);
            }
        }
    }

    /// Rounded rectangle outline of the given stroke width.
    pub fn stroke_round_rect(&mut self, r: Rect, radius: i32, width: i32, c: Color) {
        let rad = radius.min(r.w / 2).min(r.h / 2).max(0) as f32;
        let w = width as f32;
        let clipped = r.intersect(&self.clip);
        for y in clipped.y..clipped.bottom() {
            for x in clipped.x..clipped.right() {
                let d = round_rect_sdf(r, rad, x as f32 + 0.5, y as f32 + 0.5);
                // Inside the band [-w, 0] of the signed distance field.
                let cov = coverage(-d).min(coverage(d + w));
                self.blend_pixel(x, y, c, cov);
            }
        }
    }

    pub fn fill_circle(&mut self, center: Point, radius: f32, c: Color) {
        let r = radius.ceil_i32() + 1;
        let bb = Rect::new(center.x - r, center.y - r, 2 * r + 1, 2 * r + 1).intersect(&self.clip);
        for y in bb.y..bb.bottom() {
            for x in bb.x..bb.right() {
                let dx = x as f32 + 0.5 - center.x as f32;
                let dy = y as f32 + 0.5 - center.y as f32;
                let cov = coverage(radius - sqrtf(dx * dx + dy * dy));
                self.blend_pixel(x, y, c, cov);
            }
        }
    }

    pub fn stroke_circle(&mut self, center: Point, radius: f32, width: f32, c: Color) {
        let r = (radius + width).ceil_i32() + 1;
        let bb = Rect::new(center.x - r, center.y - r, 2 * r + 1, 2 * r + 1).intersect(&self.clip);
        for y in bb.y..bb.bottom() {
            for x in bb.x..bb.right() {
                let dx = x as f32 + 0.5 - center.x as f32;
                let dy = y as f32 + 0.5 - center.y as f32;
                let d = sqrtf(dx * dx + dy * dy);
                let cov = coverage(radius + width / 2.0 - d).min(coverage(d - (radius - width / 2.0)));
                self.blend_pixel(x, y, c, cov);
            }
        }
    }

    /// Anti-aliased line with round caps.
    pub fn line(&mut self, a: Point, b: Point, width: f32, c: Color) {
        let half = width / 2.0;
        let pad = half.ceil_i32() + 1;
        let bb = Rect::new(
            a.x.min(b.x) - pad,
            a.y.min(b.y) - pad,
            (a.x - b.x).abs() + 2 * pad + 1,
            (a.y - b.y).abs() + 2 * pad + 1,
        )
        .intersect(&self.clip);
        let (ax, ay, bx, by) = (a.x as f32, a.y as f32, b.x as f32, b.y as f32);
        let (vx, vy) = (bx - ax, by - ay);
        let len2 = (vx * vx + vy * vy).max(1e-6);
        for y in bb.y..bb.bottom() {
            for x in bb.x..bb.right() {
                let px = x as f32 + 0.5 - ax;
                let py = y as f32 + 0.5 - ay;
                let t = ((px * vx + py * vy) / len2).clamp(0.0, 1.0);
                let dx = px - vx * t;
                let dy = py - vy * t;
                let cov = coverage(half - sqrtf(dx * dx + dy * dy));
                self.blend_pixel(x, y, c, cov);
            }
        }
    }

    /// Filled triangle, anti-aliased by 4x4 supersampling along edges.
    pub fn fill_triangle(&mut self, p: [Point; 3], c: Color) {
        let minx = p.iter().map(|q| q.x).min().unwrap();
        let maxx = p.iter().map(|q| q.x).max().unwrap();
        let miny = p.iter().map(|q| q.y).min().unwrap();
        let maxy = p.iter().map(|q| q.y).max().unwrap();
        let bb = Rect::new(minx, miny, maxx - minx + 1, maxy - miny + 1).intersect(&self.clip);
        let f = |q: Point| (q.x as f32, q.y as f32);
        let (p0, p1, p2) = (f(p[0]), f(p[1]), f(p[2]));
        let area = edge(p0, p1, p2);
        if area == 0.0 {
            return;
        }
        for y in bb.y..bb.bottom() {
            for x in bb.x..bb.right() {
                let mut hits = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let s = (x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0);
                        let w0 = edge(p1, p2, s) / area;
                        let w1 = edge(p2, p0, s) / area;
                        let w2 = edge(p0, p1, s) / area;
                        if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 {
                            hits += 1;
                        }
                    }
                }
                self.blend_pixel(x, y, c, (hits * 255 / 16) as u8);
            }
        }
    }

    /// Copy an RGB565 image with its top-left corner at `at`.
    pub fn blit(&mut self, at: Point, img: &[Color], w: i32, h: i32) {
        let dst = Rect::new(at.x, at.y, w, h).intersect(&self.clip);
        for y in dst.y..dst.bottom() {
            let sy = y - at.y;
            let sx = dst.x - at.x;
            let src = &img[(sy * w + sx) as usize..(sy * w + sx + dst.w) as usize];
            let start = self.idx(dst.x, y);
            self.buf[start..start + dst.w as usize].copy_from_slice(src);
        }
    }

    /// Scale an image into `dst` with nearest-neighbour sampling.
    pub fn blit_scaled(&mut self, dst: Rect, img: &[Color], w: i32, h: i32) {
        let vis = dst.intersect(&self.clip);
        if vis.is_empty() || w == 0 || h == 0 {
            return;
        }
        for y in vis.y..vis.bottom() {
            let sy = ((y - dst.y) as i64 * h as i64 / dst.h as i64) as i32;
            let row = &img[(sy * w) as usize..((sy + 1) * w) as usize];
            let start = self.idx(vis.x, y);
            for (i, p) in self.buf[start..start + vis.w as usize].iter_mut().enumerate() {
                let sx = ((vis.x + i as i32 - dst.x) as i64 * w as i64 / dst.w as i64) as usize;
                *p = row[sx];
            }
        }
    }

    /// Draw a 4-bit alpha mask (two pixels per byte, high nibble first) in `c`.
    pub fn draw_mask4(&mut self, at: Point, mask: &[u8], w: i32, h: i32, c: Color) {
        let stride = ((w + 1) / 2) as usize;
        let vis = Rect::new(at.x, at.y, w, h).intersect(&self.clip);
        for y in vis.y..vis.bottom() {
            let row = &mask[(y - at.y) as usize * stride..];
            let base = self.idx(0, y);
            for x in vis.x..vis.right() {
                let mx = (x - at.x) as usize;
                let nib = if mx % 2 == 0 { row[mx / 2] >> 4 } else { row[mx / 2] & 0xF };
                if nib != 0 {
                    let i = base + x as usize;
                    self.buf[i] = c.blend(self.buf[i], nib * 17);
                }
            }
        }
    }
}

/// Signed distance to a rounded rectangle (negative inside).
fn round_rect_sdf(r: Rect, rad: f32, x: f32, y: f32) -> f32 {
    let cx = r.x as f32 + r.w as f32 / 2.0;
    let cy = r.y as f32 + r.h as f32 / 2.0;
    let qx = (x - cx).abs() - (r.w as f32 / 2.0 - rad);
    let qy = (y - cy).abs() - (r.h as f32 / 2.0 - rad);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    sqrtf(ox * ox + oy * oy) + qx.max(qy).min(0.0) - rad
}

fn edge(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Map a signed distance (positive = inside, in pixels) to 0..=255 coverage.
#[inline]
fn coverage(d: f32) -> u8 {
    if d >= 0.5 {
        255
    } else if d <= -0.5 {
        0
    } else {
        ((d + 0.5) * 255.0) as u8
    }
}

trait CeilI32 {
    fn ceil_i32(self) -> i32;
}

impl CeilI32 for f32 {
    fn ceil_i32(self) -> i32 {
        let t = self as i32;
        if (t as f32) < self { t + 1 } else { t }
    }
}
