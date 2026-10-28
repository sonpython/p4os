//! Calculator with operator precedence, a running expression line and a
//! large result display.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "calculator", name: "Calculator", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Calculator::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0xff9f43), Color::hex(0xee5a24));
    let inner = r.inset(r.w / 4);
    for row in 0..3 {
        for col in 0..3 {
            let cell = inner.grid_cell(3, 3, r.w / 16, col, row);
            c.fill_round_rect(cell, cell.w / 4, Color::WHITE);
        }
    }
}

#[derive(Default)]
struct Calculator {
    /// Expression typed so far, e.g. "12+3×4".
    expr: String,
    /// Last evaluated result, shown large.
    result: String,
    just_evaluated: bool,
}

const KEYS: [[&str; 4]; 5] = [
    ["C", "(", ")", "÷"],
    ["7", "8", "9", "×"],
    ["4", "5", "6", "−"],
    ["1", "2", "3", "+"],
    ["0", ".", "⌫", "="],
];

impl App for Calculator {
    fn update(&mut self, ui: &mut Ui, area: Rect, _sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, t.bg);

        // Keyboard input as an alternative to tapping.
        for k in ui.input.keys.clone() {
            if let p4os_sdk::gui::ui::Key::Char(c) = k {
                match c {
                    '0'..='9' | '.' | '(' | ')' => self.press(&c.to_string()),
                    '+' => self.press("+"),
                    '-' => self.press("−"),
                    '*' => self.press("×"),
                    '/' => self.press("÷"),
                    '=' => self.press("="),
                    _ => {}
                }
            } else if k == p4os_sdk::gui::ui::Key::Enter {
                self.press("=");
            } else if k == p4os_sdk::gui::ui::Key::Backspace {
                self.press("⌫");
            }
        }

        let (display, pad) = area.inset(16).split_top(170);
        ui.panel(display);
        let inner = display.inset(18);
        let (expr_r, res_r) = inner.split_top(40);
        let (small, huge) = (ui.fonts.large, ui.fonts.huge);
        ui.text(expr_r, &self.expr, &small, t.text_dim, Align::Right);
        let shown = if self.result.is_empty() { "0" } else { &self.result };
        ui.text(res_r, shown, &huge, t.text, Align::Right);

        let pad = Rect::new(pad.x, pad.y + 16, pad.w, pad.h - 16);
        for (row, keys) in KEYS.iter().enumerate() {
            for (col, key) in keys.iter().enumerate() {
                let r = pad.grid_cell(4, 5, 12, col as i32, row as i32);
                let is_op = col == 3;
                let (bg, fg) = match *key {
                    "=" => (t.accent, t.accent_text),
                    "C" => (t.danger, Color::WHITE),
                    _ if is_op => (t.surface_alt.lighten(20), t.accent),
                    _ => (t.surface_alt, t.text),
                };
                if ui.button_colored(r, key, bg, fg) {
                    self.press(key);
                }
            }
        }
    }
}

impl Calculator {
    fn press(&mut self, key: &str) {
        match key {
            "C" => {
                self.expr.clear();
                self.result.clear();
            }
            "⌫" => {
                self.expr.pop();
            }
            "=" => {
                self.result = match eval(&self.expr) {
                    Some(v) => format_number(v),
                    None => "Error".to_string(),
                };
                self.just_evaluated = true;
                return;
            }
            _ => {
                let is_operator = matches!(key, "+" | "−" | "×" | "÷");
                if self.just_evaluated {
                    // Continue from the result with an operator, otherwise
                    // start a fresh expression.
                    self.expr = if is_operator && self.result != "Error" { self.result.clone() } else { String::new() };
                }
                self.expr.push_str(key);
            }
        }
        self.just_evaluated = false;
    }
}

fn format_number(v: f64) -> String {
    if !v.is_finite() {
        return "Error".to_string();
    }
    if v == libm::trunc(v) && libm::fabs(v) < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{:.10}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Recursive-descent evaluator: expr := term (('+'|'−') term)*,
/// term := factor (('×'|'÷') factor)*, factor := '−'? (number | '(' expr ')').
pub fn eval(s: &str) -> Option<f64> {
    let chars: alloc::vec::Vec<char> = s.chars().collect();
    let mut p = Parser { s: &chars, i: 0 };
    let v = p.expr()?;
    (p.i == chars.len()).then_some(v)
}

struct Parser<'a> {
    s: &'a [char],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(c) = self.peek() {
            match c {
                '+' => { self.i += 1; v += self.term()?; }
                '−' | '-' => { self.i += 1; v -= self.term()?; }
                _ => break,
            }
        }
        Some(v)
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.factor()?;
        while let Some(c) = self.peek() {
            match c {
                '×' | '*' => { self.i += 1; v *= self.factor()?; }
                '÷' | '/' => { self.i += 1; v /= self.factor()?; }
                _ => break,
            }
        }
        Some(v)
    }

    fn factor(&mut self) -> Option<f64> {
        match self.peek()? {
            '−' | '-' => { self.i += 1; Some(-self.factor()?) }
            '(' => {
                self.i += 1;
                let v = self.expr()?;
                (self.peek()? == ')').then(|| { self.i += 1; v })
            }
            _ => {
                let start = self.i;
                while matches!(self.peek(), Some('0'..='9' | '.')) {
                    self.i += 1;
                }
                let text: String = self.s[start..self.i].iter().collect();
                text.parse().ok()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::eval;

    #[test]
    fn precedence_and_parens() {
        assert_eq!(eval("2+3×4"), Some(14.0));
        assert_eq!(eval("(2+3)×4"), Some(20.0));
        assert_eq!(eval("10÷4"), Some(2.5));
        assert_eq!(eval("−3+5"), Some(2.0));
        assert_eq!(eval("2×"), None);
    }
}
