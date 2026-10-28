//! RGB565 colour, the native pixel format of the display pipeline.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct Color(pub u16);

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color(((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3))
    }

    pub const fn hex(v: u32) -> Self {
        Color::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    pub const fn r(self) -> u8 {
        let r = (self.0 >> 11) as u8 & 0x1F;
        (r << 3) | (r >> 2)
    }

    pub const fn g(self) -> u8 {
        let g = (self.0 >> 5) as u8 & 0x3F;
        (g << 2) | (g >> 4)
    }

    pub const fn b(self) -> u8 {
        let b = self.0 as u8 & 0x1F;
        (b << 3) | (b >> 2)
    }

    /// Mix `self` (alpha 0..=255) over `bg`.
    #[inline]
    pub fn blend(self, bg: Color, alpha: u8) -> Color {
        match alpha {
            0 => bg,
            255 => self,
            _ => {
                // Blend in 565 space on 5/6-bit channels, 5-bit alpha.
                let a = (alpha as u32 + 4) >> 3;
                let fg = self.0 as u32;
                let bgv = bg.0 as u32;
                // Spread G into the upper half so R, G and B can be scaled at once.
                let fg = (fg | (fg << 16)) & 0x07E0_F81F;
                let bgv = (bgv | (bgv << 16)) & 0x07E0_F81F;
                let mixed = ((fg.wrapping_sub(bgv)).wrapping_mul(a) >> 5).wrapping_add(bgv) & 0x07E0_F81F;
                Color((mixed | (mixed >> 16)) as u16)
            }
        }
    }

    /// Linear interpolation, `t` in 0..=255.
    pub fn lerp(self, other: Color, t: u8) -> Color {
        other.blend(self, t)
    }

    pub fn lighten(self, amount: u8) -> Color {
        Color::WHITE.blend(self, amount)
    }

    pub fn darken(self, amount: u8) -> Color {
        Color::BLACK.blend(self, amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_endpoints() {
        let a = Color::rgb(255, 0, 0);
        let b = Color::rgb(0, 0, 255);
        assert_eq!(a.blend(b, 255), a);
        assert_eq!(a.blend(b, 0), b);
        let mid = a.blend(b, 128);
        assert!(mid.r() > 100 && mid.r() < 160, "r={}", mid.r());
        assert!(mid.b() > 100 && mid.b() < 160, "b={}", mid.b());
    }
}
