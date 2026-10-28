//! Terminal: a scrollback view over the kernel shell.
//!
//! Each line typed at the `p4os$ ` prompt is handed to
//! [`System::run_command`] and its output appended to the scrollback. A few
//! commands (`clear`, `history`) are handled locally. Tapping the terminal
//! toggles the on-screen keyboard; physical keys work at any time.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use p4os_sdk::gui::ui::Key;
use p4os_sdk::{App, AppInfo, Canvas, Color, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "terminal", name: "Terminal", icon, create };

const PROMPT: &str = "p4os$ ";
/// Logical lines kept in the scrollback.
const SCROLLBACK_LINES: usize = 500;
/// Commands remembered for Up/Down recall and `history`.
const HISTORY_LEN: usize = 100;
const PAD: i32 = 12;

fn create() -> Box<dyn App> {
    Box::new(Terminal::new())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x3b4252), Color::hex(0x1b1f27));
    let s = r.w as f32 / 64.0;
    let p = |x: f32, y: f32| Point::new(r.x + (x * s) as i32, r.y + (y * s) as i32);
    let green = Color::hex(0x5af78e);
    // ">_" prompt glyph.
    c.line(p(16.0, 22.0), p(28.0, 32.0), 4.0 * s, green);
    c.line(p(28.0, 32.0), p(16.0, 42.0), 4.0 * s, green);
    c.fill_rect(Rect::new(r.x + (32.0 * s) as i32, r.y + (40.0 * s) as i32, (16.0 * s) as i32, (4.0 * s) as i32), Color::WHITE);
}

/// Bounded list of output lines; the oldest lines fall off the top.
struct Scrollback {
    lines: Vec<String>,
    cap: usize,
}

impl Scrollback {
    fn new(cap: usize) -> Self {
        Self { lines: Vec::new(), cap }
    }

    /// Append `text`, one entry per `\n`-separated line. A single trailing
    /// newline does not produce an empty line.
    fn push_text(&mut self, text: &str) {
        let text = text.strip_suffix('\n').unwrap_or(text);
        for line in text.split('\n') {
            // Drop carriage returns and expand tabs; the font has neither.
            let clean: String = line
                .chars()
                .filter(|&c| c != '\r')
                .map(|c| if c == '\t' { ' ' } else { c })
                .collect();
            self.lines.push(clean);
        }
        if self.lines.len() > self.cap {
            let excess = self.lines.len() - self.cap;
            self.lines.drain(..excess);
        }
    }

    fn clear(&mut self) {
        self.lines.clear();
    }
}

/// Command history with shell-style Up/Down recall.
struct History {
    entries: Vec<String>,
    cap: usize,
    /// Index being recalled; `None` means editing a fresh line.
    cursor: Option<usize>,
}

impl History {
    fn new(cap: usize) -> Self {
        Self { entries: Vec::new(), cap, cursor: None }
    }

    /// Record a submitted command, skipping blanks and immediate repeats.
    fn push(&mut self, cmd: &str) {
        self.cursor = None;
        let cmd = cmd.trim();
        if cmd.is_empty() || self.entries.last().map(String::as_str) == Some(cmd) {
            return;
        }
        self.entries.push(cmd.to_string());
        if self.entries.len() > self.cap {
            self.entries.remove(0);
        }
    }

    /// Step to an older entry (Up).
    fn prev(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let i = match self.cursor {
            None => self.entries.len() - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.cursor = Some(i);
        Some(&self.entries[i])
    }

    /// Step to a newer entry (Down); past the newest returns an empty line.
    fn next(&mut self) -> Option<&str> {
        let i = self.cursor?;
        if i + 1 < self.entries.len() {
            self.cursor = Some(i + 1);
            Some(&self.entries[i + 1])
        } else {
            self.cursor = None;
            Some("")
        }
    }

    /// Numbered listing as printed by the `history` command.
    fn listing(&self) -> String {
        let mut out = String::new();
        for (i, e) in self.entries.iter().enumerate() {
            out.push_str(&format!("{:>4}  {}\n", i + 1, e));
        }
        out
    }
}

/// Split `line` into rows of at most `cols` characters. Unlike
/// `Font::wrap` this keeps every space, so column-aligned output (`ps`,
/// `history`) stays aligned in the monospace font.
fn wrap_cols(line: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return alloc::vec![String::new()];
    }
    chars.chunks(cols).map(|c| c.iter().collect()).collect()
}

struct Terminal {
    scrollback: Scrollback,
    history: History,
    input: String,
    /// Pixels scrolled down from the top of the content.
    offset: i32,
    /// Keep the view pinned to the newest output.
    follow: bool,
    keyboard: bool,
    /// The keyboard was toggled; redraw at once so the view resizes.
    redraw: bool,
}

impl Terminal {
    fn new() -> Self {
        let mut scrollback = Scrollback::new(SCROLLBACK_LINES);
        scrollback.push_text("p4os terminal. Type `help` for kernel commands; `clear` and `history` are local.");
        Self {
            scrollback,
            history: History::new(HISTORY_LEN),
            input: String::new(),
            offset: 0,
            follow: true,
            keyboard: false,
            redraw: false,
        }
    }

    fn submit(&mut self, sys: &mut dyn System) {
        let line = core::mem::take(&mut self.input);
        self.scrollback.push_text(&format!("{PROMPT}{line}"));
        let cmd = line.trim();
        match cmd {
            "" => {}
            "clear" => self.scrollback.clear(),
            "history" => {
                // Include this command, as shells do.
                self.history.push(cmd);
                let listing = self.history.listing();
                self.scrollback.push_text(&listing);
                return;
            }
            _ => {
                let out = sys.run_command(cmd);
                if !out.is_empty() {
                    self.scrollback.push_text(&out);
                }
            }
        }
        self.history.push(cmd);
    }

    fn handle_keys(&mut self, keys: &[Key], sys: &mut dyn System) {
        for k in keys {
            match *k {
                Key::Char(c) => self.input.push(c),
                Key::Backspace => {
                    self.input.pop();
                }
                Key::Enter => self.submit(sys),
                Key::Up => {
                    if let Some(s) = self.history.prev() {
                        self.input = s.to_string();
                    }
                }
                Key::Down => {
                    if let Some(s) = self.history.next() {
                        self.input = s.to_string();
                    }
                }
                _ => continue,
            }
            // Any keystroke brings the prompt back into view.
            self.follow = true;
        }
    }
}

impl App for Terminal {
    fn tick_interval_ms(&self) -> Option<u32> {
        self.redraw.then_some(0)
    }

    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let bg = Color::hex(0x0b0e13);
        let fg = Color::hex(0xd8dee9);
        let prompt_c = Color::hex(0x5af78e);
        let t = *ui.theme;
        ui.fill(area, bg);

        let input = ui.input;
        self.redraw = input.tapped_in(area);
        if self.redraw {
            self.keyboard = !self.keyboard;
            self.follow = true;
        }
        if self.keyboard {
            ui.state.wants_keyboard = true;
        }
        let keys = input.keys.clone();
        self.handle_keys(&keys, sys);

        // Wrap the scrollback plus the prompt line to the view width.
        let view = area.inset(PAD);
        let font = ui.fonts.mono;
        let lh = font.line_height + 2;
        let cols = ((view.w - 10) / font.char_width('M').max(1)) as usize;
        let mut rows: Vec<(String, Color)> = Vec::new();
        for line in &self.scrollback.lines {
            let c = if line.starts_with(PROMPT) { t.text_dim } else { fg };
            for w in wrap_cols(line, cols) {
                rows.push((w, c));
            }
        }
        let prompt_line = format!("{PROMPT}{}", self.input);
        let prompt_rows = wrap_cols(&prompt_line, cols);
        let n_prompt = prompt_rows.len();
        for w in prompt_rows {
            rows.push((w, fg));
        }

        // Drag to scroll back; new output re-pins the view to the bottom.
        let content_h = rows.len() as i32 * lh;
        let max_off = (content_h - view.h).max(0);
        if input.down && area.contains(input.press_pos) && input.is_drag() {
            self.offset -= input.delta.y;
            self.follow = false;
        }
        if self.follow {
            self.offset = max_off;
        }
        self.offset = self.offset.clamp(0, max_off);
        if self.offset >= max_off {
            self.follow = true;
        }

        let old = ui.canvas.set_clip(view.intersect(&ui.canvas.clip()));
        let first = (self.offset / lh) as usize;
        let mut last_end = None;
        for (i, (text, color)) in rows.iter().enumerate().skip(first) {
            let y = view.y + i as i32 * lh - self.offset;
            if y > view.bottom() {
                break;
            }
            let is_prompt = i + n_prompt >= rows.len();
            let at = Point::new(view.x, y);
            if is_prompt && i + n_prompt == rows.len() {
                // First prompt row: colour the prompt itself.
                let adv = font.draw(ui.canvas, at, PROMPT, prompt_c);
                let rest = text.get(PROMPT.len()..).unwrap_or("");
                let adv2 = font.draw(ui.canvas, Point::new(at.x + adv, y), rest, fg);
                last_end = Some(Point::new(at.x + adv + adv2, y));
            } else {
                let adv = font.draw(ui.canvas, at, text, *color);
                if is_prompt {
                    last_end = Some(Point::new(at.x + adv, y));
                }
            }
        }
        // Block caret, solid while the keyboard is open.
        if let Some(end) = last_end {
            let caret = Rect::new(end.x + 1, end.y + 1, font.char_width('M').max(8), lh - 2);
            if self.keyboard {
                ui.canvas.fill_rect(caret, prompt_c);
            } else {
                ui.canvas.stroke_rect(caret, 1, prompt_c);
            }
        }
        ui.canvas.restore_clip(old);

        if content_h > view.h {
            let bar_h = (view.h * view.h / content_h).max(30);
            let bar_y = view.y + (view.h - bar_h) * self.offset / max_off.max(1);
            ui.canvas.fill_round_rect(Rect::new(area.right() - 8, bar_y, 4, bar_h), 2, t.text_dim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_splits_lines_and_ignores_trailing_newline() {
        let mut s = Scrollback::new(10);
        s.push_text("a\nb\n");
        s.push_text("c\r\n");
        assert_eq!(s.lines, ["a", "b", "c"]);
    }

    #[test]
    fn scrollback_keeps_only_newest_lines() {
        let mut s = Scrollback::new(3);
        for i in 0..5 {
            s.push_text(&format!("{i}"));
        }
        assert_eq!(s.lines, ["2", "3", "4"]);
        s.push_text("x\ny\nz\nw");
        assert_eq!(s.lines, ["y", "z", "w"]);
    }

    #[test]
    fn scrollback_expands_tabs_and_keeps_blank_lines() {
        let mut s = Scrollback::new(10);
        s.push_text("a\tb\n\nc");
        assert_eq!(s.lines, ["a b", "", "c"]);
    }

    #[test]
    fn wrap_keeps_spaces_and_splits_by_columns() {
        assert_eq!(wrap_cols("", 4), [""]);
        assert_eq!(wrap_cols("   1  ps", 4), ["   1", "  ps"]);
        assert_eq!(wrap_cols("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn history_skips_blanks_and_repeats() {
        let mut h = History::new(10);
        h.push("ps");
        h.push("  ");
        h.push("ps");
        h.push("mem");
        assert_eq!(h.entries, ["ps", "mem"]);
        assert_eq!(h.listing(), "   1  ps\n   2  mem\n");
    }

    #[test]
    fn history_recall_walks_back_and_forward() {
        let mut h = History::new(10);
        assert_eq!(h.prev(), None);
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.prev(), Some("c"));
        assert_eq!(h.prev(), Some("b"));
        assert_eq!(h.prev(), Some("a"));
        assert_eq!(h.prev(), Some("a"));
        assert_eq!(h.next(), Some("b"));
        assert_eq!(h.next(), Some("c"));
        assert_eq!(h.next(), Some(""));
        assert_eq!(h.next(), None);
    }

    #[test]
    fn history_is_capped() {
        let mut h = History::new(2);
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.entries, ["b", "c"]);
    }
}
