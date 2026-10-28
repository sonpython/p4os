//! Helpers for drawing launcher icons from primitives.

use p4os_sdk::{Canvas, Color, Rect};

/// Rounded-square icon background with a vertical gradient.
pub fn tile(c: &mut Canvas, r: Rect, top: Color, bottom: Color) {
    let rad = r.w / 5;
    // Draw the gradient band by band, clipped to the rounded shape by
    // drawing a rounded rect per band.
    for i in 0..r.h {
        let t = (i * 255 / r.h.max(1)) as u8;
        let row = Rect::new(r.x, r.y + i, r.w, 1);
        let old = c.set_clip(row);
        c.fill_round_rect(r, rad, top.lerp(bottom, t));
        c.restore_clip(old);
    }
}
