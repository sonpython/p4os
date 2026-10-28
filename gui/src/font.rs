//! Anti-aliased bitmap fonts in the P4F1 format produced by
//! `tools/gen-font.py`.

use crate::canvas::Canvas;
use crate::color::Color;
use crate::geom::Point;
use alloc::string::String;
use alloc::vec::Vec;

const HEADER: usize = 12;
const GLYPH: usize = 16;

#[derive(Clone, Copy)]
pub struct Font {
    data: &'static [u8],
    pub size: u16,
    pub line_height: i32,
    pub ascent: i32,
    count: usize,
}

#[derive(Clone, Copy)]
struct Glyph {
    advance: i32,
    w: i32,
    h: i32,
    xoff: i32,
    yoff: i32,
    offset: usize,
}

fn u16_at(d: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([d[i], d[i + 1]])
}

fn u32_at(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

impl Font {
    pub const fn empty() -> Self {
        Self { data: &[], size: 0, line_height: 0, ascent: 0, count: 0 }
    }

    pub fn parse(data: &'static [u8]) -> Self {
        assert!(&data[..4] == b"P4F1", "not a P4F1 font");
        Self {
            data,
            size: u16_at(data, 4),
            line_height: u16_at(data, 6) as i32,
            ascent: u16_at(data, 8) as i16 as i32,
            count: u16_at(data, 10) as usize,
        }
    }

    fn glyph(&self, ch: char) -> Option<Glyph> {
        let cp = ch as u32;
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = HEADER + mid * GLYPH;
            let c = u32_at(self.data, e);
            if c == cp {
                let d = self.data;
                return Some(Glyph {
                    advance: u16_at(d, e + 4) as i32,
                    w: d[e + 6] as i32,
                    h: d[e + 7] as i32,
                    xoff: d[e + 8] as i8 as i32,
                    yoff: d[e + 9] as i8 as i32,
                    offset: HEADER + self.count * GLYPH + u32_at(d, e + 10) as usize,
                });
            }
            if c < cp { lo = mid + 1 } else { hi = mid }
        }
        None
    }

    fn glyph_or_fallback(&self, ch: char) -> Option<Glyph> {
        self.glyph(ch).or_else(|| self.glyph('?'))
    }

    pub fn char_width(&self, ch: char) -> i32 {
        self.glyph_or_fallback(ch).map_or(0, |g| g.advance)
    }

    pub fn measure(&self, text: &str) -> i32 {
        text.chars().map(|c| self.char_width(c)).sum()
    }

    /// Draw `text` with the top of its line box at `at`. Returns the advance.
    pub fn draw(&self, canvas: &mut Canvas, at: Point, text: &str, color: Color) -> i32 {
        let mut x = at.x;
        let baseline = at.y + self.ascent;
        let clip = canvas.clip();
        for ch in text.chars() {
            let Some(g) = self.glyph_or_fallback(ch) else { continue };
            if x > clip.right() {
                break;
            }
            if g.w > 0 && x + g.xoff + g.w >= clip.x {
                let mask = &self.data[g.offset..];
                canvas.draw_mask4(Point::new(x + g.xoff, baseline + g.yoff), mask, g.w, g.h, color);
            }
            x += g.advance;
        }
        x - at.x
    }

    /// Break `text` into lines no wider than `width`, wrapping at spaces and
    /// splitting words that are longer than a whole line.
    pub fn wrap(&self, text: &str, width: i32) -> Vec<String> {
        let mut lines = Vec::new();
        for para in text.split('\n') {
            let mut line = String::new();
            let mut line_w = 0;
            for word in para.split(' ') {
                let word_w = self.measure(word);
                let space_w = if line.is_empty() { 0 } else { self.char_width(' ') };
                if line_w + space_w + word_w <= width {
                    if !line.is_empty() {
                        line.push(' ');
                    }
                    line.push_str(word);
                    line_w += space_w + word_w;
                    continue;
                }
                if !line.is_empty() {
                    lines.push(core::mem::take(&mut line));
                    line_w = 0;
                }
                for ch in word.chars() {
                    let cw = self.char_width(ch);
                    if line_w + cw > width && !line.is_empty() {
                        lines.push(core::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push(ch);
                    line_w += cw;
                }
            }
            lines.push(line);
        }
        lines
    }
}

/// The fonts bundled with the OS.
pub mod builtin {
    use super::Font;

    pub static SANS_14: &[u8] = include_bytes!("../fonts/sans-14.bin");
    pub static SANS_18: &[u8] = include_bytes!("../fonts/sans-18.bin");
    pub static SANS_24: &[u8] = include_bytes!("../fonts/sans-24.bin");
    pub static SANS_32: &[u8] = include_bytes!("../fonts/sans-32.bin");
    pub static SANS_BOLD_24: &[u8] = include_bytes!("../fonts/sans-bold-24.bin");
    pub static SANS_56: &[u8] = include_bytes!("../fonts/sans-56.bin");
    pub static MONO_16: &[u8] = include_bytes!("../fonts/mono-16.bin");

    pub struct Fonts {
        pub small: Font,
        pub body: Font,
        pub large: Font,
        pub title: Font,
        pub bold: Font,
        pub huge: Font,
        pub mono: Font,
    }

    pub fn load() -> Fonts {
        Fonts {
            small: Font::parse(SANS_14),
            body: Font::parse(SANS_18),
            large: Font::parse(SANS_24),
            title: Font::parse(SANS_32),
            bold: Font::parse(SANS_BOLD_24),
            huge: Font::parse(SANS_56),
            mono: Font::parse(MONO_16),
        }
    }
}
