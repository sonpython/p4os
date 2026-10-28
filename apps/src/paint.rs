//! Finger painting with a colour palette, brush sizes, eraser, undo and
//! saving to `/sd/Paint/drawing-N.bmp` (24-bit BMP).
//!
//! The drawing lives in an RGB565 buffer owned by the app; strokes are drawn
//! into it with a `Canvas` over that buffer and the buffer is blitted to the
//! screen every frame.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, DirEntry, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "paint", name: "Paint", icon, create };

const SAVE_DIR: &str = "/sd/Paint";
const PAPER: Color = Color::WHITE;
const UNDO_DEPTH: usize = 4;

const PALETTE: [u32; 10] = [
    0x1e1e1e, 0x8a8f98, 0xe53935, 0xfb8c00, 0xfdd835, 0x43a047, 0x00acc1, 0x1e88e5, 0x8e24aa, 0xec407a,
];
const SIZES: [f32; 4] = [3.0, 7.0, 14.0, 28.0];

fn create() -> Box<dyn App> {
    Box::new(Paint::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0xfdfbf7), Color::hex(0xd9d4ca));
    let s = r.w as f32 / 100.0;
    let at = |x: f32, y: f32| Point::new(r.x + (x * s) as i32, r.y + (y * s) as i32);
    // Three brush strokes.
    c.line(at(22.0, 70.0), at(45.0, 30.0), 10.0 * s, Color::hex(0xe53935));
    c.line(at(45.0, 30.0), at(60.0, 62.0), 10.0 * s, Color::hex(0xe53935));
    c.line(at(40.0, 76.0), at(78.0, 40.0), 10.0 * s, Color::hex(0x1e88e5));
    c.fill_circle(at(72.0, 72.0), 9.0 * s, Color::hex(0xfdd835));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Brush,
    Eraser,
}

struct Paint {
    pixels: Vec<Color>,
    /// Size of `pixels` (follows the drawing area).
    w: i32,
    h: i32,
    undo: VecDeque<Vec<Color>>,
    color: usize,
    size: usize,
    tool: Tool,
    /// Last pointer position of the stroke in progress, in buffer coordinates.
    last: Option<Point>,
    message: String,
}

impl Default for Paint {
    fn default() -> Self {
        Paint {
            pixels: Vec::new(),
            w: 0,
            h: 0,
            undo: VecDeque::new(),
            color: 0,
            size: 1,
            tool: Tool::Brush,
            last: None,
            message: String::new(),
        }
    }
}

/// Toolbar and canvas regions: two toolbar rows above the canvas in
/// portrait, a three-column tool panel left of it in landscape.
struct Layout {
    colors: [Rect; 10],
    /// Brush sizes followed by the eraser.
    tools: [Rect; 5],
    /// Undo, Clear, Save.
    actions: [Rect; 3],
    canvas: Rect,
    status: Rect,
}

impl Layout {
    fn new(area: Rect) -> Self {
        let pad = 12;
        let inner = area.inset(pad);
        if area.h >= area.w {
            let row_a = Rect::new(inner.x, inner.y, inner.w, 52);
            let row_b = Rect::new(inner.x, row_a.bottom() + 10, inner.w, 56);
            let canvas = Rect::new(inner.x, row_b.bottom() + 12, inner.w, inner.bottom() - row_b.bottom() - 12 - 36);
            Layout {
                colors: core::array::from_fn(|i| row_a.grid_cell(10, 1, 6, i as i32, 0)),
                tools: core::array::from_fn(|i| row_b.grid_cell(8, 1, 8, i as i32, 0)),
                actions: core::array::from_fn(|i| row_b.grid_cell(8, 1, 8, 5 + i as i32, 0)),
                canvas,
                status: Rect::new(inner.x, canvas.bottom() + 4, inner.w, 32),
            }
        } else {
            // Rows: 4 of colours, 2 of tools, 1 of actions.
            let panel = Rect::new(inner.x, inner.y, 180, inner.h.min(7 * 62));
            let cell = |i: usize, row0: i32| panel.grid_cell(3, 7, 6, i as i32 % 3, row0 + i as i32 / 3);
            let canvas_x = panel.right() + pad;
            let canvas = Rect::new(canvas_x, inner.y, inner.right() - canvas_x, inner.h - 36);
            Layout {
                colors: core::array::from_fn(|i| cell(i, 0)),
                tools: core::array::from_fn(|i| cell(i, 4)),
                actions: core::array::from_fn(|i| cell(i, 6)),
                canvas,
                status: Rect::new(canvas.x, canvas.bottom() + 4, canvas.w, 32),
            }
        }
    }
}

impl Paint {
    /// Make the buffer match the drawing area, keeping the overlapping part
    /// of the old drawing. Undo snapshots of another size are dropped.
    fn fit(&mut self, w: i32, h: i32) {
        if (w, h) == (self.w, self.h) {
            return;
        }
        let mut px = Vec::new();
        if px.try_reserve_exact((w.max(0) * h.max(0)) as usize).is_err() {
            self.message = String::from("Not enough memory for the canvas");
            return;
        }
        px.resize((w * h) as usize, PAPER);
        for y in 0..h.min(self.h) {
            let n = w.min(self.w) as usize;
            let (src, dst) = ((y * self.w) as usize, (y * w) as usize);
            px[dst..dst + n].copy_from_slice(&self.pixels[src..src + n]);
        }
        self.pixels = px;
        self.w = w;
        self.h = h;
        self.undo.clear();
        self.last = None;
    }

    fn snapshot(&mut self) {
        if self.undo.len() >= UNDO_DEPTH {
            self.undo.pop_front();
        }
        let mut copy = Vec::new();
        if copy.try_reserve_exact(self.pixels.len()).is_ok() {
            copy.extend_from_slice(&self.pixels);
            self.undo.push_back(copy);
        }
    }

    fn stroke_color(&self) -> Color {
        match self.tool {
            Tool::Brush => Color::hex(PALETTE[self.color]),
            Tool::Eraser => PAPER,
        }
    }

    /// Draw the segment from the previous pointer position to `p`.
    fn stroke_to(&mut self, p: Point) {
        let from = self.last.unwrap_or(p);
        let (color, width) = (self.stroke_color(), SIZES[self.size]);
        let mut c = Canvas::new(&mut self.pixels, self.w, self.h);
        c.line(from, p, width, color);
        self.last = Some(p);
    }

    fn handle_pointer(&mut self, ui: &Ui, canvas_r: Rect) {
        let i = ui.input;
        let local = Point::new(i.pos.x - canvas_r.x, i.pos.y - canvas_r.y);
        if i.pressed && canvas_r.contains(i.pos) && !self.pixels.is_empty() {
            self.snapshot();
            self.last = None;
            self.stroke_to(local);
            self.message.clear();
        } else if self.last.is_some() && (i.down || i.released) {
            self.stroke_to(local);
        }
        if !i.down {
            self.last = None;
        }
    }

    fn save(&mut self, sys: &mut dyn System) {
        let entries = match sys.fs().list(SAVE_DIR) {
            Ok(e) => e,
            Err(e) => {
                self.message = format!("Cannot save: {SAVE_DIR} is not available ({e:?})");
                return;
            }
        };
        let path = format!("{SAVE_DIR}/drawing-{}.bmp", next_drawing_number(&entries));
        let bmp = encode_bmp(&self.pixels, self.w, self.h);
        self.message = match sys.fs().write(&path, &bmp) {
            Ok(()) => format!("Saved {path}"),
            Err(e) => format!("Cannot write {path} ({e:?})"),
        };
    }
}

/// One more than the highest N among existing `drawing-N.bmp` files.
pub fn next_drawing_number(entries: &[DirEntry]) -> u32 {
    entries
        .iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| e.name.strip_prefix("drawing-")?.strip_suffix(".bmp")?.parse::<u32>().ok())
        .max()
        .map_or(1, |n| n + 1)
}

/// Encode RGB565 pixels as an uncompressed 24-bit bottom-up BMP.
pub fn encode_bmp(pixels: &[Color], w: i32, h: i32) -> Vec<u8> {
    let (w, h) = (w.max(0) as u32, h.max(0) as u32);
    let row = (w * 3 + 3) & !3;
    let data_size = row * h;
    let file_size = 54 + data_size;
    let mut out = Vec::with_capacity(file_size as usize);
    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes()); // 72 DPI
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        let start = out.len();
        for x in 0..w {
            let c = pixels[(y * w + x) as usize];
            out.extend_from_slice(&[c.b(), c.g(), c.r()]);
        }
        out.resize(start + row as usize, 0);
    }
    out
}

fn draw_eraser(c: &mut Canvas, r: Rect, col: Color) {
    let m = r.center();
    let (a, b) = (Point::new(m.x - 9, m.y + 9), Point::new(m.x + 9, m.y - 9));
    c.line(a, b, 16.0, col);
    c.line(a, b, 11.0, Color::hex(0xf48fb1));
    let mid = Point::new(m.x - 3, m.y + 3);
    c.line(a, mid, 11.0, Color::WHITE);
}

impl App for Paint {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, t.bg);
        let l = Layout::new(area);
        self.fit(l.canvas.w, l.canvas.h);
        self.handle_pointer(ui, l.canvas);

        // Palette: the selected colour gets a ring.
        for (i, r) in l.colors.iter().enumerate() {
            let (_, clicked) = ui.interact(*r);
            if clicked {
                self.color = i;
                self.tool = Tool::Brush;
            }
            let rad = (r.w.min(r.h) / 2) as f32;
            let selected = self.color == i && self.tool == Tool::Brush;
            if selected {
                ui.canvas.stroke_circle(r.center(), rad - 2.0, 3.0, t.text);
            }
            ui.canvas.fill_circle(r.center(), rad - if selected { 8.0 } else { 4.0 }, Color::hex(PALETTE[i]));
        }

        // Brush sizes and eraser.
        for (i, r) in l.tools.iter().enumerate() {
            let selected = if i < SIZES.len() { self.tool == Tool::Brush && self.size == i } else { self.tool == Tool::Eraser };
            let bg = if selected { t.accent } else { t.surface_alt };
            let (held, clicked) = ui.interact(*r);
            ui.canvas.fill_round_rect(*r, 10, if held { bg.lighten(40) } else { bg });
            if i < SIZES.len() {
                let dot = (SIZES[i] / 2.0).min((r.h / 2 - 6) as f32);
                ui.canvas.fill_circle(r.center(), dot, t.text);
                if clicked {
                    self.size = i;
                    self.tool = Tool::Brush;
                }
            } else {
                draw_eraser(ui.canvas, *r, t.text);
                if clicked {
                    self.tool = Tool::Eraser;
                }
            }
        }

        let small = ui.fonts.body;
        let labels = ["Undo", "Clear", "Save"];
        for (i, r) in l.actions.iter().enumerate() {
            let (held, clicked) = ui.interact(*r);
            let bg = if i == 2 { t.accent } else { t.surface_alt };
            ui.canvas.fill_round_rect(*r, 10, if held { bg.lighten(40) } else { bg });
            let enabled = i != 0 || !self.undo.is_empty();
            let fg = if !enabled { t.text_dim } else if i == 2 { t.accent_text } else { t.text };
            ui.text(*r, labels[i], &small, fg, Align::Center);
            if !clicked || !enabled {
                continue;
            }
            match i {
                0 => {
                    if let Some(prev) = self.undo.pop_back() {
                        self.pixels = prev;
                    }
                    self.message.clear();
                }
                1 => {
                    self.snapshot();
                    self.pixels.fill(PAPER);
                    self.message.clear();
                }
                _ => self.save(sys),
            }
        }

        // The drawing itself.
        if !self.pixels.is_empty() {
            ui.canvas.blit(Point::new(l.canvas.x, l.canvas.y), &self.pixels, self.w, self.h);
        }
        ui.canvas.stroke_rect(l.canvas.inset(-1), 1, t.border);

        let status = if self.message.is_empty() {
            let what = match self.tool {
                Tool::Brush => "Brush",
                Tool::Eraser => "Eraser",
            };
            format!("{what} {} px  •  {} x {}  •  Undo steps: {}", SIZES[self.size] as i32, self.w, self.h, self.undo.len())
        } else {
            self.message.clone()
        };
        ui.text(l.status, &status, &small, t.text_dim, Align::Left);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmp_header_and_padding() {
        // 3x2 image: row size 9 bytes padded to 12.
        let px = [Color::rgb(255, 0, 0), Color::WHITE, Color::BLACK, Color::rgb(0, 0, 255), Color::BLACK, Color::WHITE];
        let bmp = encode_bmp(&px, 3, 2);
        let u32_at = |i: usize| u32::from_le_bytes([bmp[i], bmp[i + 1], bmp[i + 2], bmp[i + 3]]);
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp.len(), 54 + 24);
        assert_eq!(u32_at(2) as usize, bmp.len());
        assert_eq!(u32_at(10), 54);
        assert_eq!(u32_at(14), 40);
        assert_eq!((u32_at(18), u32_at(22)), (3, 2));
        assert_eq!(u16::from_le_bytes([bmp[28], bmp[29]]), 24);
        assert_eq!(u32_at(34), 24);
        // Bottom row first, stored as BGR: blue pixel then black, white.
        assert_eq!(&bmp[54..63], &[255, 0, 0, 0, 0, 0, 255, 255, 255]);
        assert_eq!(&bmp[63..66], &[0, 0, 0]);
        // Top row: red is (B, G, R) = (0, 0, 255).
        assert_eq!(&bmp[66..69], &[0, 0, 255]);
    }

    #[test]
    fn picks_next_drawing_number() {
        let e = |name: &str, is_dir| DirEntry { name: name.into(), is_dir, size: 0 };
        assert_eq!(next_drawing_number(&[]), 1);
        let list = [e("drawing-2.bmp", false), e("drawing-10.bmp", false), e("drawing-x.bmp", false), e("drawing-99.bmp", true)];
        assert_eq!(next_drawing_number(&list), 11);
    }

    #[test]
    fn resize_keeps_overlap() {
        let mut p = Paint::default();
        p.fit(4, 3);
        p.pixels[1 * 4 + 2] = Color::BLACK;
        p.fit(3, 5);
        assert_eq!(p.pixels.len(), 15);
        assert_eq!(p.pixels[1 * 3 + 2], Color::BLACK);
        assert_eq!(p.pixels[4 * 3], PAPER);
    }
}
