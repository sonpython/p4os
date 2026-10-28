//! On-screen touch keyboard.

use crate::geom::Rect;
use crate::ui::{Align, Key, Ui};

#[derive(Default)]
pub struct Keyboard {
    shift: bool,
    symbols: bool,
}

const LETTERS: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];
const SYMBOLS: [&str; 3] = ["1234567890", "-/:;()&@\"", ".,?!'#%+="];

pub const HEIGHT: i32 = 280;

enum Cap {
    Char(char),
    Shift,
    Mode,
    Backspace,
    Space,
    Enter,
}

impl Keyboard {
    /// Draw the keyboard in `area` and return the key tapped this frame.
    pub fn show(&mut self, ui: &mut Ui, area: Rect) -> Option<Key> {
        let t = *ui.theme;
        ui.canvas.fill_rect(area, t.surface);
        let rows = if self.symbols { SYMBOLS } else { LETTERS };
        let pad = 8;
        let inner = area.inset(pad);
        let row_h = (inner.h - 3 * pad) / 4;
        let key_w = (inner.w - 9 * pad) / 10;
        let mut result = None;

        let mut cap = |ui: &mut Ui, r: Rect, label: &str, c: Cap, kb: &mut Keyboard| {
            let special = !matches!(c, Cap::Char(_) | Cap::Space);
            let bg = if special { t.surface_alt.darken(30) } else { t.surface_alt };
            if ui.button_colored(r, label, bg, t.text) {
                match c {
                    Cap::Char(ch) => {
                        let ch = if kb.shift { ch.to_ascii_uppercase() } else { ch };
                        kb.shift = false;
                        result = Some(Key::Char(ch));
                    }
                    Cap::Shift => kb.shift = !kb.shift,
                    Cap::Mode => kb.symbols = !kb.symbols,
                    Cap::Backspace => result = Some(Key::Backspace),
                    Cap::Space => result = Some(Key::Char(' ')),
                    Cap::Enter => result = Some(Key::Enter),
                }
            }
        };

        for (ri, row) in rows.iter().enumerate() {
            let n = row.chars().count() as i32;
            let row_w = n * key_w + (n - 1) * pad;
            let mut x = inner.x + (inner.w - row_w) / 2;
            let y = inner.y + ri as i32 * (row_h + pad);
            if ri == 2 {
                let wide = (inner.w - row_w) / 2 - pad;
                let label = if self.symbols { "" } else if self.shift { "SHIFT" } else { "shift" };
                if !self.symbols {
                    cap(ui, Rect::new(inner.x, y, wide, row_h), label, Cap::Shift, self);
                }
                cap(ui, Rect::new(inner.right() - wide, y, wide, row_h), "<-", Cap::Backspace, self);
            }
            for ch in row.chars() {
                let mut buf = [0u8; 4];
                let shown = if self.shift { ch.to_ascii_uppercase() } else { ch };
                cap(ui, Rect::new(x, y, key_w, row_h), shown.encode_utf8(&mut buf), Cap::Char(ch), self);
                x += key_w + pad;
            }
        }
        let y = inner.y + 3 * (row_h + pad);
        let side = key_w * 2 + pad;
        let mode_label = if self.symbols { "ABC" } else { "123" };
        cap(ui, Rect::new(inner.x, y, side, row_h), mode_label, Cap::Mode, self);
        cap(ui, Rect::new(inner.x + side + pad, y, inner.w - 2 * (side + pad), row_h), "", Cap::Space, self);
        cap(ui, Rect::new(inner.right() - side, y, side, row_h), "Enter", Cap::Enter, self);
        let f = ui.fonts.small;
        let dim = t.text_dim;
        ui.text(Rect::new(inner.x + side + pad, y, inner.w - 2 * (side + pad), row_h), "space", &f, dim, Align::Center);
        result
    }
}
