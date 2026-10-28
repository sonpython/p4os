//! Drawing the lock screen, plus the numeric keypad and passcode dots it
//! shares with the Settings > Security page.

use super::{Checking, Lock, throttle};
use alloc::format;
use alloc::vec::Vec;
use p4os_sdk::gui::font::builtin::Fonts;
use p4os_sdk::gui::ui::{Align, Input, Key, Theme};
use p4os_sdk::{Canvas, Color, Point, Rect, System, Ui};

/// How long the dots shake after a wrong passcode.
const SHAKE_MS: u64 = 450;
/// How long "Wrong passcode" stays red.
const WRONG_MS: u64 = 2500;
const DOT_GAP: i32 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PadKey {
    Digit(u8),
    Back,
    Enter,
}

const LAYOUT: [Option<PadKey>; 12] = {
    use PadKey::*;
    [
        Some(Digit(b'1')),
        Some(Digit(b'2')),
        Some(Digit(b'3')),
        Some(Digit(b'4')),
        Some(Digit(b'5')),
        Some(Digit(b'6')),
        Some(Digit(b'7')),
        Some(Digit(b'8')),
        Some(Digit(b'9')),
        Some(Back),
        Some(Digit(b'0')),
        Some(Enter),
    ]
};

/// A 3x4 keypad of round keys filling `r`. Returns the key tapped.
pub fn keypad(ui: &mut Ui, r: Rect, enabled: bool) -> Option<PadKey> {
    let t = *ui.theme;
    let mut hit = None;
    let radius = ((r.w / 3).min(r.h / 4) / 2 - 6).clamp(18, 40);
    for (i, key) in LAYOUT.iter().enumerate() {
        let Some(key) = *key else { continue };
        let cell = r.grid_cell(3, 4, 0, i as i32 % 3, i as i32 / 3);
        let c = cell.center();
        let hot = Rect::new(c.x - radius, c.y - radius, 2 * radius, 2 * radius);
        let (held, clicked) = if enabled { ui.interact(hot) } else { (false, false) };
        let fg = if enabled { t.text } else { t.text_dim.darken(60) };
        match key {
            PadKey::Digit(d) => {
                let bg = if held { t.surface_alt.lighten(50) } else { t.surface_alt };
                ui.canvas.fill_circle(c, radius as f32, if enabled { bg } else { bg.darken(40) });
                let f = if radius >= 32 { ui.fonts.title } else { ui.fonts.large };
                let s = [d];
                let label = core::str::from_utf8(&s).unwrap_or("?");
                ui.text(hot, label, &f, fg, Align::Center);
            }
            PadKey::Back => {
                if held {
                    ui.canvas.fill_circle(c, radius as f32, t.surface);
                }
                // A left-pointing key cap with a cross.
                let s = radius as f32 / 36.0;
                let p = |dx: f32, dy: f32| Point::new(c.x + (dx * s) as i32, c.y + (dy * s) as i32);
                let w = 2.5;
                let outline = [p(-18.0, 0.0), p(-8.0, -11.0), p(16.0, -11.0), p(16.0, 11.0), p(-8.0, 11.0)];
                for k in 0..outline.len() {
                    ui.canvas.line(outline[k], outline[(k + 1) % outline.len()], w, fg);
                }
                ui.canvas.line(p(-1.0, -5.0), p(9.0, 5.0), w, fg);
                ui.canvas.line(p(9.0, -5.0), p(-1.0, 5.0), w, fg);
            }
            PadKey::Enter => {
                let bg = if held { t.accent.lighten(40) } else { t.accent };
                ui.canvas.fill_circle(c, radius as f32, if enabled { bg } else { t.surface_alt.darken(40) });
                let f = ui.fonts.bold;
                ui.text(hot, "OK", &f, if enabled { t.accent_text } else { fg }, Align::Center);
            }
        }
        if clicked {
            hit = Some(key);
        }
    }
    hit
}

/// Keypad keys typed on a keyboard (the USB console or the simulator).
pub fn typed_keys(input: &Input) -> Vec<PadKey> {
    input
        .keys
        .iter()
        .filter_map(|k| match *k {
            Key::Char(c) if c.is_ascii_digit() => Some(PadKey::Digit(c as u8)),
            Key::Backspace => Some(PadKey::Back),
            Key::Enter => Some(PadKey::Enter),
            _ => None,
        })
        .collect()
}

/// One dot per typed digit (at least four, so an empty entry still shows
/// where the passcode goes), centred in `r` and shifted by `dx`.
pub fn dots(canvas: &mut Canvas, r: Rect, typed: usize, color: Color, dx: i32) {
    let n = typed.max(super::pin::MIN_LEN) as i32;
    let x0 = r.x + (r.w - (n - 1) * DOT_GAP) / 2 + dx;
    let cy = r.y + r.h / 2;
    for i in 0..n {
        let c = Point::new(x0 + i * DOT_GAP, cy);
        if (i as usize) < typed {
            canvas.fill_circle(c, 8.0, color);
        } else {
            canvas.stroke_circle(c, 8.0, 2.0, color);
        }
    }
}

/// Horizontal offset of the shaking dots `elapsed` ms after a wrong entry.
pub fn shake_offset(elapsed: u64) -> i32 {
    if elapsed >= SHAKE_MS {
        return 0;
    }
    let p = elapsed as f32 / SHAKE_MS as f32;
    (libm::sinf(p * 6.0 * core::f32::consts::PI) * 22.0 * (1.0 - p)) as i32
}

fn fmt_wait(ms: u64) -> alloc::string::String {
    let s = ms.div_ceil(1000);
    format!("{}:{:02}", s / 60, s % 60)
}

/// Signal bars, lit when the link is up.
fn wifi_icon(canvas: &mut Canvas, at: Point, up: bool, on: Color, off: Color) {
    for i in 0..4 {
        let h = 5 + i * 4;
        let bar = Rect::new(at.x + i * 7, at.y + 18 - h, 5, h);
        canvas.fill_rect(bar, if up { on } else { off });
    }
}

/// A closed padlock centred at `c`.
pub fn padlock(canvas: &mut Canvas, c: Point, size: i32, color: Color) {
    let body = Rect::new(c.x - size / 2, c.y - size / 8, size, size * 5 / 8);
    let shackle = Rect::new(c.x - size * 3 / 10, c.y - size / 2, size * 3 / 5, size * 3 / 4);
    canvas.stroke_round_rect(shackle, size * 3 / 10, (size / 8).max(2), color);
    canvas.fill_round_rect(body, (size / 8).max(2), color);
}

pub(super) fn frame(lock: &mut Lock, canvas: &mut Canvas, input: &Input, fonts: &Fonts, theme: &Theme, sys: &mut dyn System) -> Option<u32> {
    // The hash takes a noticeable fraction of a second on the board, so a
    // submitted passcode is checked only after a frame saying so is shown.
    match lock.checking {
        Checking::Requested => lock.checking = Checking::Shown,
        Checking::Shown => {
            lock.checking = Checking::Idle;
            lock.submit(sys);
            if !lock.locked {
                return Some(0);
            }
        }
        Checking::Idle => {}
    }
    let now = sys.uptime_ms();
    lock.throttle.refresh(sys);
    let wait = lock.throttle.remaining_ms(now);
    let screen = canvas.bounds();
    let landscape = screen.w > screen.h;
    let t = *theme;
    let (up, wall) = (sys.net().is_up(), sys.now());

    let mut ui = Ui { canvas, input, theme, fonts, state: &mut lock.ui };
    ui.canvas.fill_gradient(screen, Color::hex(0x1b2a4a), Color::hex(0x05070c));

    // Status row: link state on the left, a padlock in the middle.
    let (status, body) = screen.split_top(crate::desktop::STATUS_H);
    wifi_icon(ui.canvas, Point::new(status.x + 16, status.y + 9), up, t.text, t.text_dim.darken(60));
    let small = fonts.small;
    let link = if up { "Wi-Fi" } else { "No network" };
    ui.text(Rect::new(status.x + 52, status.y, 160, status.h), link, &small, t.text_dim, Align::Left);
    padlock(ui.canvas, status.center(), 18, t.text);

    // Clock and date on top (portrait) or on the left (landscape).
    let (clock_r, pad_r) = if landscape {
        body.split_left(body.w / 2)
    } else {
        let (c, p) = body.split_top(250);
        (c, p)
    };
    let (huge, large, body_f) = (fonts.huge, fonts.large, fonts.body);
    let block_h = huge.line_height + 10 + large.line_height;
    let y = clock_r.y + (clock_r.h - block_h) / 2;
    let row = |y: i32, h: i32| Rect::new(clock_r.x, y, clock_r.w, h);
    let (time, date) = match wall {
        Some(n) => (format!("{:02}:{:02}", n.hour, n.minute), crate::clock::fmt_date(&n)),
        None => ("--:--".into(), "Time not set".into()),
    };
    ui.text(row(y, huge.line_height), &time, &huge, t.text, Align::Center);
    let date_f = if large.measure(&date) <= clock_r.w - 24 { large } else { body_f };
    ui.text(row(y + huge.line_height + 10, large.line_height), &date, &date_f, t.text_dim, Align::Center);

    // Prompt, dots and keypad.
    let pad_r = if landscape { pad_r.inset(16) } else { Rect::new(pad_r.x + 40, pad_r.y, pad_r.w - 80, pad_r.h - 16) };
    let (msg_r, rest) = pad_r.split_top(40);
    let (dots_r, keys_r) = rest.split_top(44);
    let keys_r = Rect::new(keys_r.x, keys_r.y + 8, keys_r.w, keys_r.h - 8);
    let since_wrong = lock.wrong_at.map(|w| now.saturating_sub(w));
    let checking = lock.checking != Checking::Idle;
    let (msg, color) = if checking {
        ("Checking...".into(), t.text)
    } else if wait > 0 {
        (format!("Too many attempts. Try again in {}", fmt_wait(wait)), t.danger)
    } else if since_wrong.is_some_and(|e| e < WRONG_MS) {
        let left = throttle::ATTEMPTS_PER_LOCKOUT - lock.throttle.fails() % throttle::ATTEMPTS_PER_LOCKOUT;
        let text = if left < throttle::ATTEMPTS_PER_LOCKOUT && left <= 2 {
            format!("Wrong passcode, {} {} left", left, if left == 1 { "try" } else { "tries" })
        } else {
            "Wrong passcode".into()
        };
        (text, t.danger)
    } else {
        ("Enter passcode".into(), t.text)
    };
    let msg_f = if large.measure(&msg) <= msg_r.w { large } else { body_f };
    ui.text(msg_r, &msg, &msg_f, color, Align::Center);
    let shaking = since_wrong.map_or(0, shake_offset);
    let dot_color = if since_wrong.is_some_and(|e| e < SHAKE_MS) { t.danger } else { t.text };
    dots(ui.canvas, dots_r, lock.entry.len(), dot_color, shaking);

    let mut keys = typed_keys(input);
    if let Some(k) = keypad(&mut ui, keys_r, wait == 0) {
        keys.push(k);
    }
    for k in keys {
        if wait > 0 || lock.checking != Checking::Idle {
            break;
        }
        match k {
            PadKey::Digit(d) => {
                lock.entry.push(d);
                // Typing again replaces the "wrong passcode" feedback.
                lock.wrong_at = None;
            }
            PadKey::Back => lock.entry.pop(),
            PadKey::Enter if !lock.entry.is_empty() => lock.checking = Checking::Requested,
            PadKey::Enter => {}
        }
    }

    if checking || lock.checking != Checking::Idle || input.pressed || input.released || !input.keys.is_empty() {
        return Some(0);
    }
    match since_wrong {
        Some(e) if e < SHAKE_MS => Some(16),
        Some(e) if e < WRONG_MS => Some((WRONG_MS - e) as u32),
        _ if wait > 0 => Some(((wait - 1) % 1000 + 1) as u32),
        _ => Some(1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shake_settles() {
        assert_eq!(shake_offset(0), 0);
        assert!(shake_offset(SHAKE_MS / 12).abs() > 10);
        assert_eq!(shake_offset(SHAKE_MS), 0);
        assert_eq!(shake_offset(10_000), 0);
    }

    #[test]
    fn wait_text_rounds_up() {
        assert_eq!(fmt_wait(30_000), "0:30");
        assert_eq!(fmt_wait(29_001), "0:30");
        assert_eq!(fmt_wait(1), "0:01");
        assert_eq!(fmt_wait(15 * 60_000), "15:00");
    }

    #[test]
    fn keyboard_keys_map_to_the_pad() {
        let input = Input { keys: alloc::vec![Key::Char('7'), Key::Char('x'), Key::Backspace, Key::Enter, Key::Up], ..Default::default() };
        assert_eq!(typed_keys(&input), [PadKey::Digit(b'7'), PadKey::Back, PadKey::Enter]);
    }
}
