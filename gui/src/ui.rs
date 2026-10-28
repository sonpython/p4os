//! Immediate-mode widgets for a touch screen.
//!
//! Each frame the app calls widget functions with the rectangle they occupy;
//! the functions draw themselves and report interactions (a button returns
//! `true` on the frame it is tapped). Widget identity is derived from its
//! rectangle, which is stable for layouts computed from the screen size.

use crate::canvas::Canvas;
use crate::color::Color;
use crate::font::{builtin::Fonts, Font};
use crate::geom::{Point, Rect};
use alloc::string::String;
use alloc::vec::Vec;

/// Pointer and keyboard input for one frame.
#[derive(Clone, Default, Debug)]
pub struct Input {
    pub pos: Point,
    /// Finger currently touching.
    pub down: bool,
    /// Touch started this frame.
    pub pressed: bool,
    /// Touch ended this frame (at `pos`).
    pub released: bool,
    /// Where the current/last touch started.
    pub press_pos: Point,
    /// Pointer movement since the previous frame while touching.
    pub delta: Point,
    pub keys: Vec<Key>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Escape,
}

impl Input {
    /// Movement larger than this turns a tap into a drag.
    pub const DRAG_SLOP: i32 = 12;

    pub fn is_drag(&self) -> bool {
        (self.pos.x - self.press_pos.x).abs() > Self::DRAG_SLOP
            || (self.pos.y - self.press_pos.y).abs() > Self::DRAG_SLOP
    }

    pub fn tapped_in(&self, r: Rect) -> bool {
        self.released && !self.is_drag() && r.contains(self.pos) && r.contains(self.press_pos)
    }
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub surface: Color,
    pub surface_alt: Color,
    pub text: Color,
    pub text_dim: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub danger: Color,
    pub border: Color,
    pub radius: i32,
}

impl Theme {
    pub const fn dark() -> Self {
        Self {
            bg: Color::hex(0x10141c),
            surface: Color::hex(0x1c2230),
            surface_alt: Color::hex(0x283041),
            text: Color::hex(0xeef1f6),
            text_dim: Color::hex(0x8d97a8),
            accent: Color::hex(0x3d8bfd),
            accent_text: Color::WHITE,
            danger: Color::hex(0xe5484d),
            border: Color::hex(0x343d50),
            radius: 12,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// State that survives between frames (which widget is held, which field
/// has keyboard focus).
#[derive(Default)]
pub struct UiState {
    pub active: Option<Rect>,
    pub focus: Option<Rect>,
    /// Set by text fields every frame they are focused; the desktop shows the
    /// on-screen keyboard while it is true.
    pub wants_keyboard: bool,
}

pub struct Ui<'f, 'c> {
    pub canvas: &'f mut Canvas<'c>,
    pub input: &'f Input,
    pub theme: &'f Theme,
    pub fonts: &'f Fonts,
    pub state: &'f mut UiState,
}

impl<'f, 'c> Ui<'f, 'c> {
    pub fn fill(&mut self, r: Rect, c: Color) {
        self.canvas.fill_rect(r, c);
    }

    pub fn panel(&mut self, r: Rect) {
        let t = self.theme;
        self.canvas.fill_round_rect(r, t.radius, t.surface);
    }

    pub fn text(&mut self, r: Rect, text: &str, font: &Font, color: Color, align: Align) {
        let w = font.measure(text);
        let x = match align {
            Align::Left => r.x,
            Align::Center => r.x + (r.w - w) / 2,
            Align::Right => r.right() - w,
        };
        let y = r.y + (r.h - font.line_height) / 2;
        let old = self.canvas.set_clip(r.intersect(&self.canvas.clip()));
        font.draw(self.canvas, Point::new(x, y), text, color);
        self.canvas.restore_clip(old);
    }

    pub fn label(&mut self, r: Rect, text: &str) {
        let (f, c) = (self.fonts.body, self.theme.text);
        self.text(r, text, &f, c, Align::Left);
    }

    /// Tracks press/release over `r`; returns (held_inside, clicked).
    pub fn interact(&mut self, r: Rect) -> (bool, bool) {
        let i = self.input;
        if i.pressed && r.contains(i.pos) {
            self.state.active = Some(r);
        }
        let is_active = self.state.active == Some(r);
        let held = is_active && i.down && r.contains(i.pos) && !i.is_drag();
        let clicked = is_active && i.tapped_in(r);
        if is_active && (i.released || !i.down) {
            self.state.active = None;
        }
        (held, clicked)
    }

    pub fn button_colored(&mut self, r: Rect, label: &str, bg: Color, fg: Color) -> bool {
        let (held, clicked) = self.interact(r);
        let bg = if held { bg.lighten(40) } else { bg };
        self.canvas.fill_round_rect(r, self.theme.radius, bg);
        let f = self.fonts.large;
        self.text(r, label, &f, fg, Align::Center);
        clicked
    }

    pub fn button(&mut self, r: Rect, label: &str) -> bool {
        let t = *self.theme;
        self.button_colored(r, label, t.surface_alt, t.text)
    }

    pub fn button_primary(&mut self, r: Rect, label: &str) -> bool {
        let t = *self.theme;
        self.button_colored(r, label, t.accent, t.accent_text)
    }

    /// A button whose face is drawn by `draw` (for icons).
    pub fn icon_button(&mut self, r: Rect, draw: impl FnOnce(&mut Canvas, Rect, Color)) -> bool {
        let (held, clicked) = self.interact(r);
        let t = *self.theme;
        if held {
            self.canvas.fill_round_rect(r, t.radius, t.surface_alt);
        }
        draw(self.canvas, r, t.text);
        clicked
    }

    pub fn toggle(&mut self, r: Rect, value: &mut bool) -> bool {
        let (_, clicked) = self.interact(r);
        if clicked {
            *value = !*value;
        }
        let t = *self.theme;
        let track = Rect::new(r.right() - 64, r.y + (r.h - 34) / 2, 64, 34);
        self.canvas.fill_round_rect(track, 17, if *value { t.accent } else { t.surface_alt });
        let knob_x = if *value { track.right() - 17 } else { track.x + 17 };
        self.canvas.fill_circle(Point::new(knob_x, track.y + 17), 13.0, Color::WHITE);
        clicked
    }

    /// Horizontal slider for `value` in 0..=1. Returns true while it changes.
    pub fn slider(&mut self, r: Rect, value: &mut f32) -> bool {
        let i = self.input;
        if i.pressed && r.contains(i.pos) {
            self.state.active = Some(r);
        }
        let mut changed = false;
        if self.state.active == Some(r) {
            if i.down {
                let v = ((i.pos.x - r.x) as f32 / r.w as f32).clamp(0.0, 1.0);
                changed = v != *value;
                *value = v;
            } else {
                self.state.active = None;
            }
        }
        let t = *self.theme;
        let cy = r.y + r.h / 2;
        let track = Rect::new(r.x, cy - 4, r.w, 8);
        self.canvas.fill_round_rect(track, 4, t.surface_alt);
        let filled = (r.w as f32 * *value) as i32;
        self.canvas.fill_round_rect(Rect::new(r.x, cy - 4, filled.max(8), 8), 4, t.accent);
        self.canvas.fill_circle(Point::new(r.x + filled, cy), 14.0, Color::WHITE);
        changed
    }

    pub fn progress(&mut self, r: Rect, value: f32) {
        let t = *self.theme;
        self.canvas.fill_round_rect(r, r.h / 2, t.surface_alt);
        let w = (r.w as f32 * value.clamp(0.0, 1.0)) as i32;
        if w > 0 {
            self.canvas.fill_round_rect(Rect::new(r.x, r.y, w.max(r.h), r.h), r.h / 2, t.accent);
        }
    }

    /// Single-line text field. Focus follows taps; while focused it consumes
    /// typed keys. Returns true when Enter is pressed.
    pub fn text_field(&mut self, r: Rect, value: &mut String, placeholder: &str) -> bool {
        let (_, clicked) = self.interact(r);
        if clicked {
            self.state.focus = Some(r);
        } else if self.input.released && !r.contains(self.input.pos) && self.state.focus == Some(r) {
            // Tapping elsewhere (but not the keyboard, which is handled by
            // the desktop before the app sees input) drops focus.
            self.state.focus = None;
        }
        let focused = self.state.focus == Some(r);
        let mut submitted = false;
        if focused {
            self.state.wants_keyboard = true;
            for k in &self.input.keys {
                match k {
                    Key::Char(c) => value.push(*c),
                    Key::Backspace => {
                        value.pop();
                    }
                    Key::Enter => submitted = true,
                    _ => {}
                }
            }
        }
        let t = *self.theme;
        self.canvas.fill_round_rect(r, 10, t.surface_alt);
        if focused {
            self.canvas.stroke_round_rect(r, 10, 2, t.accent);
        }
        let inner = r.inset(12);
        let f = self.fonts.body;
        let old = self.canvas.set_clip(inner);
        let ty = inner.y + (inner.h - f.line_height) / 2;
        if value.is_empty() && !focused {
            f.draw(self.canvas, Point::new(inner.x, ty), placeholder, t.text_dim);
        } else {
            // Keep the end of long text (where the caret is) visible.
            let w = f.measure(value);
            let x = inner.x - (w - inner.w + 4).max(0);
            let adv = f.draw(self.canvas, Point::new(x, ty), value, t.text);
            if focused {
                self.canvas.fill_rect(Rect::new(x + adv + 1, ty + 2, 2, f.line_height - 4), t.accent);
            }
        }
        self.canvas.restore_clip(old);
        submitted
    }

    /// Vertical drag-to-scroll. Updates `offset` (pixels scrolled down) and
    /// returns the clamped offset.
    pub fn scroll(&mut self, r: Rect, content_h: i32, offset: &mut i32) -> i32 {
        let i = self.input;
        if i.down && r.contains(i.press_pos) && i.is_drag() {
            *offset -= i.delta.y;
        }
        for k in &i.keys {
            match k {
                Key::Down => *offset += 40,
                Key::Up => *offset -= 40,
                _ => {}
            }
        }
        *offset = (*offset).clamp(0, (content_h - r.h).max(0));
        if content_h > r.h {
            let t = *self.theme;
            let bar_h = (r.h * r.h / content_h).max(30);
            let bar_y = r.y + (r.h - bar_h) * *offset / (content_h - r.h);
            self.canvas.fill_round_rect(Rect::new(r.right() - 6, bar_y, 4, bar_h), 2, t.text_dim);
        }
        *offset
    }

    /// Scrollable list of rows. Returns the index of a tapped row.
    pub fn list(&mut self, r: Rect, rows: &[&str], row_h: i32, offset: &mut i32, selected: Option<usize>) -> Option<usize> {
        let content_h = rows.len() as i32 * row_h;
        let off = self.scroll(r, content_h, offset);
        let t = *self.theme;
        let f = self.fonts.body;
        let old = self.canvas.set_clip(r);
        let mut tapped = None;
        let first = (off / row_h) as usize;
        for (i, row) in rows.iter().enumerate().skip(first) {
            let y = r.y + i as i32 * row_h - off;
            if y > r.bottom() {
                break;
            }
            let rr = Rect::new(r.x, y, r.w - 10, row_h);
            if selected == Some(i) {
                self.canvas.fill_round_rect(rr.inset(2), 8, t.accent.darken(90));
            }
            if self.input.tapped_in(rr) && r.contains(self.input.pos) {
                tapped = Some(i);
            }
            self.text(Rect::new(rr.x + 14, rr.y, rr.w - 28, rr.h), row, &f, t.text, Align::Left);
            self.canvas.fill_rect(Rect::new(rr.x + 14, rr.bottom() - 1, rr.w - 28, 1), t.border);
        }
        self.canvas.restore_clip(old);
        tapped
    }
}
