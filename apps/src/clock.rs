//! Clock with three tabs: an analog/digital clock, a stopwatch with laps
//! and a countdown timer with a progress ring.
//!
//! All timing uses `sys.uptime_ms()`; the wall clock comes from `sys.now()`
//! and falls back to the uptime when the time has not been set.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::f32::consts::PI;
use libm::{atan2f, cosf, sinf, sqrtf};
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, DateTime, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "clock", name: "Clock", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Clock::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x4b5563), Color::hex(0x1f2937));
    let m = r.center();
    let rad = r.w as f32 * 0.34;
    c.fill_circle(m, rad, Color::WHITE);
    let hand = |deg: f32, len: f32| {
        let a = deg * PI / 180.0;
        Point::new(m.x + (sinf(a) * len) as i32, m.y - (cosf(a) * len) as i32)
    };
    c.line(m, hand(300.0, rad * 0.55), r.w as f32 / 18.0, Color::hex(0x1f2937));
    c.line(m, hand(60.0, rad * 0.8), r.w as f32 / 24.0, Color::hex(0x1f2937));
    c.line(m, hand(160.0, rad * 0.85), r.w as f32 / 50.0, Color::hex(0xe5484d));
    c.fill_circle(m, r.w as f32 / 30.0, Color::hex(0xe5484d));
}

const GREEN: Color = Color::hex(0x2fb36d);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Clock,
    Stopwatch,
    Timer,
}

#[derive(Default)]
struct Stopwatch {
    running: bool,
    /// Uptime when the current run started.
    started: u64,
    /// Time accumulated by earlier runs.
    acc: u64,
    /// Total elapsed time at each lap mark.
    laps: Vec<u64>,
    scroll: i32,
}

impl Stopwatch {
    fn elapsed(&self, now: u64) -> u64 {
        self.acc + if self.running { now - self.started } else { 0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TimerState {
    Idle,
    Running { end: u64 },
    Paused { left: u64 },
    Done { at: u64 },
}

struct Timer {
    /// Configured duration in seconds.
    secs: u32,
    state: TimerState,
}

impl Timer {
    fn total_ms(&self) -> u64 {
        self.secs as u64 * 1000
    }

    fn remaining(&self, now: u64) -> u64 {
        match self.state {
            TimerState::Idle => self.total_ms(),
            TimerState::Running { end } => end.saturating_sub(now),
            TimerState::Paused { left } => left,
            TimerState::Done { .. } => 0,
        }
    }

    /// Whether the "time's up" flash is in its bright phase.
    fn flash_on(&self, now: u64) -> bool {
        matches!(self.state, TimerState::Done { at } if (now - at) / 500 % 2 == 0)
    }
}

struct Clock {
    tab: Tab,
    sw: Stopwatch,
    timer: Timer,
    /// Next wake-up requested from the desktop, computed each frame.
    tick: Option<u32>,
}

impl Default for Clock {
    fn default() -> Self {
        Clock {
            tab: Tab::Clock,
            sw: Stopwatch::default(),
            timer: Timer { secs: 5 * 60, state: TimerState::Idle },
            tick: Some(0),
        }
    }
}

/// `MM:SS.cc`, or `H:MM:SS.cc` from one hour.
pub fn fmt_stopwatch(ms: u64) -> String {
    let cs = ms / 10 % 100;
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}.{:02}", s / 3600, s / 60 % 60, s % 60, cs)
    } else {
        format!("{:02}:{:02}.{:02}", s / 60, s % 60, cs)
    }
}

/// Countdown display, rounding up so "00:00" only shows when finished:
/// `MM:SS`, or `H:MM:SS` from one hour.
pub fn fmt_timer(ms: u64) -> String {
    let s = ms.div_ceil(1000);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Day of the week, 0 = Sunday (Sakamoto's method).
pub fn weekday(year: u16, month: u8, day: u8) -> usize {
    const T: [u32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = year as u32 - (month < 3) as u32;
    ((y + y / 4 - y / 100 + y / 400 + T[(month as usize - 1) % 12] + day as u32) % 7) as usize
}

pub fn fmt_date(t: &DateTime) -> String {
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
        "December",
    ];
    let m = (t.month.clamp(1, 12) - 1) as usize;
    format!("{}, {} {} {}", DAYS[weekday(t.year, t.month.clamp(1, 12), t.day)], t.day, MONTHS[m], t.year)
}

/// Point at `len` from `c` in direction `frac` of a full turn (0 = 12 o'clock).
fn polar(c: Point, frac: f32, len: f32) -> Point {
    let a = frac * 2.0 * PI;
    Point::new(c.x + libm::roundf(sinf(a) * len) as i32, c.y - libm::roundf(cosf(a) * len) as i32)
}

/// Anti-aliased ring segment from 12 o'clock clockwise over `frac` of the
/// circle, with round ends.
fn arc(c: &mut Canvas, center: Point, radius: f32, width: f32, frac: f32, color: Color) {
    if frac <= 0.0 {
        return;
    }
    let frac = frac.min(1.0);
    let end = frac * 2.0 * PI;
    let half = width / 2.0;
    let r = (radius + half) as i32 + 2;
    let bb = Rect::new(center.x - r, center.y - r, 2 * r + 1, 2 * r + 1).intersect(&c.clip());
    for y in bb.y..bb.bottom() {
        for x in bb.x..bb.right() {
            let dx = x as f32 + 0.5 - center.x as f32;
            let dy = y as f32 + 0.5 - center.y as f32;
            let d = sqrtf(dx * dx + dy * dy);
            let radial = (half - (d - radius).abs() + 0.5).clamp(0.0, 1.0);
            if radial == 0.0 {
                continue;
            }
            let mut a = atan2f(dx, -dy);
            if a < 0.0 {
                a += 2.0 * PI;
            }
            // Soften the leading edge by its distance in pixels along the ring.
            let angular = if frac >= 1.0 { 1.0 } else { ((end - a) * d + 0.5).clamp(0.0, 1.0) };
            c.blend_pixel(x, y, color, (radial * angular * 255.0) as u8);
        }
    }
    if frac < 1.0 {
        c.fill_circle(polar(center, 0.0, radius), half, color);
        c.fill_circle(polar(center, frac, radius), half, color);
    }
}

/// Split the content area into a primary and secondary region: stacked in
/// portrait, side by side in landscape.
fn split(r: Rect, primary: i32) -> (Rect, Rect) {
    if r.h >= r.w {
        let (a, b) = r.split_top(r.h * primary / 100);
        (a, Rect::new(b.x, b.y + 16, b.w, b.h - 16))
    } else {
        let (a, b) = r.split_left(r.w * primary / 100);
        (a, Rect::new(b.x + 24, b.y, b.w - 24, b.h))
    }
}

impl Clock {
    fn tabs(&mut self, ui: &mut Ui, r: Rect, now: u64) {
        let t = *ui.theme;
        ui.canvas.fill_round_rect(r, t.radius, t.surface);
        let names = ["Clock", "Stopwatch", "Timer"];
        let tabs = [Tab::Clock, Tab::Stopwatch, Tab::Timer];
        let cell = |i: usize| r.inset(4).grid_cell(3, 1, 4, i as i32, 0);
        // Handle taps before drawing so the highlight follows immediately.
        for (i, tab) in tabs.iter().enumerate() {
            if ui.interact(cell(i)).1 {
                self.tab = *tab;
            }
        }
        for (i, (name, tab)) in names.iter().zip(tabs).enumerate() {
            let cell = cell(i);
            // A finished timer flashes its tab while another tab is open.
            let alert = tab == Tab::Timer && self.tab != Tab::Timer && self.timer.flash_on(now);
            let (bg, fg) = if alert {
                (t.danger, Color::WHITE)
            } else if self.tab == tab {
                (t.accent, t.accent_text)
            } else {
                (t.surface, t.text_dim)
            };
            ui.canvas.fill_round_rect(cell, t.radius - 4, bg);
            let f = ui.fonts.body;
            ui.text(cell, name, &f, fg, Align::Center);
        }
    }

    fn clock_tab(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let up = sys.uptime_ms();
        let now = sys.now();
        let (h, m, s) = match now {
            Some(dt) => (dt.hour as u32, dt.minute as u32, dt.second as u32),
            None => {
                let s = (up / 1000) as u32;
                (s / 3600 % 24, s / 60 % 60, s % 60)
            }
        };
        let (face_r, text_r) = split(r, if r.h >= r.w { 64 } else { 50 });
        let radius = (face_r.w.min(face_r.h) / 2 - 8) as f32;
        self.face(ui, face_r.center(), radius, h, m, s);

        // Digital time and date, vertically centred in their region.
        let (huge, large, body) = (ui.fonts.huge, ui.fonts.large, ui.fonts.body);
        let block_h = huge.line_height + 12 + large.line_height + 12 + body.line_height;
        let mut y = text_r.y + (text_r.h - block_h) / 2;
        let row = |y: i32, h: i32| Rect::new(text_r.x, y, text_r.w, h);
        ui.text(row(y, huge.line_height), &format!("{h:02}:{m:02}:{s:02}"), &huge, t.text, Align::Center);
        y += huge.line_height + 12;
        match now {
            Some(dt) => {
                let date = fmt_date(&dt);
                let f = if large.measure(&date) <= text_r.w { large } else { body };
                ui.text(row(y, large.line_height), &date, &f, t.text_dim, Align::Center);
            }
            None => {
                ui.text(row(y, large.line_height), "Time since start-up", &large, t.text_dim, Align::Center);
                y += large.line_height + 12;
                ui.text(row(y, body.line_height), "Set the time in Settings", &body, t.accent, Align::Center);
            }
        }
    }

    fn face(&self, ui: &mut Ui, c: Point, radius: f32, h: u32, m: u32, s: u32) {
        let t = *ui.theme;
        let cv = &mut *ui.canvas;
        cv.fill_circle(c, radius, t.surface);
        cv.stroke_circle(c, radius - 3.0, 6.0, t.surface_alt);
        let outer = radius - 16.0;
        for i in 0..60 {
            let major = i % 5 == 0;
            let inner = outer - if major { radius * 0.09 } else { radius * 0.035 };
            let col = if major { t.text } else { t.text_dim };
            cv.line(polar(c, i as f32 / 60.0, inner), polar(c, i as f32 / 60.0, outer), if major { 4.0 } else { 2.0 }, col);
        }
        let f = if radius > 180.0 { ui.fonts.large } else { ui.fonts.body };
        for n in 1..=12 {
            let p = polar(c, n as f32 / 12.0, radius * 0.7);
            let cell = Rect::new(p.x - 30, p.y - f.line_height / 2, 60, f.line_height);
            ui.text(cell, &format!("{n}"), &f, t.text, Align::Center);
        }
        let cv = &mut *ui.canvas;
        let (sf, mf) = (s as f32, m as f32);
        let hour = ((h % 12) as f32 + mf / 60.0 + sf / 3600.0) / 12.0;
        let minute = (mf + sf / 60.0) / 60.0;
        let second = sf / 60.0;
        cv.line(c, polar(c, hour, radius * 0.48), radius * 0.05, t.text);
        cv.line(c, polar(c, minute, radius * 0.72), radius * 0.035, t.text);
        cv.line(polar(c, second + 0.5, radius * 0.16), polar(c, second, radius * 0.8), 2.5, t.danger);
        cv.fill_circle(c, radius * 0.045, t.danger);
        cv.fill_circle(c, radius * 0.015, t.surface);
    }

    fn stopwatch_tab(&mut self, ui: &mut Ui, r: Rect, now: u64) {
        let t = *ui.theme;
        let sw = &mut self.sw;
        let elapsed = sw.elapsed(now);
        let (top, laps_r) = split(r, 40);

        // Time display and controls, centred in the top region.
        let huge = ui.fonts.huge;
        let block_h = huge.line_height + 32 + 64;
        let y = top.y + (top.h - block_h) / 2;
        ui.text(Rect::new(top.x, y, top.w, huge.line_height), &fmt_stopwatch(elapsed), &huge, t.text, Align::Center);
        let bw = ((top.w - 16) / 2).min(220);
        let buttons = Rect::new(top.x + (top.w - 2 * bw - 16) / 2, y + huge.line_height + 32, 2 * bw + 16, 64);
        let (left, right) = (buttons.grid_cell(2, 1, 16, 0, 0), buttons.grid_cell(2, 1, 16, 1, 0));
        if sw.running {
            if ui.button(left, "Lap") {
                sw.laps.push(elapsed);
                sw.scroll = 0;
            }
            if ui.button_colored(right, "Stop", t.danger, Color::WHITE) {
                sw.acc = elapsed;
                sw.running = false;
            }
        } else {
            let can_reset = elapsed > 0;
            let fg = if can_reset { t.text } else { t.text_dim };
            if ui.button_colored(left, "Reset", t.surface_alt, fg) && can_reset {
                *sw = Stopwatch::default();
            }
            let label = if elapsed > 0 { "Resume" } else { "Start" };
            if ui.button_colored(right, label, GREEN, Color::WHITE) {
                sw.started = now;
                sw.running = true;
            }
        }

        // Lap list, newest first; fastest and slowest highlighted.
        ui.panel(laps_r);
        let inner = laps_r.inset(8);
        let body = ui.fonts.body;
        if sw.laps.is_empty() {
            ui.text(inner, "Laps appear here", &body, t.text_dim, Align::Center);
            return;
        }
        let times: Vec<u64> = sw.laps.iter().enumerate().map(|(i, &v)| v - if i > 0 { sw.laps[i - 1] } else { 0 }).collect();
        let (fast, slow) = (times.iter().min().copied(), times.iter().max().copied());
        let row_h = 52;
        let off = ui.scroll(inner, times.len() as i32 * row_h, &mut sw.scroll);
        let old = ui.canvas.set_clip(inner.intersect(&ui.canvas.clip()));
        for (k, i) in (0..times.len()).rev().enumerate() {
            let y = inner.y + k as i32 * row_h - off;
            if y + row_h < inner.y || y > inner.bottom() {
                continue;
            }
            let row = Rect::new(inner.x + 12, y, inner.w - 36, row_h);
            let col = if times.len() >= 3 && Some(times[i]) == fast {
                GREEN
            } else if times.len() >= 3 && Some(times[i]) == slow {
                t.danger
            } else {
                t.text
            };
            let (a, rest) = row.split_left(row.w / 4);
            let (b, c) = rest.split_left(rest.w / 2);
            ui.text(a, &format!("Lap {}", i + 1), &body, t.text_dim, Align::Left);
            ui.text(b, &fmt_stopwatch(times[i]), &body, col, Align::Right);
            ui.text(c, &fmt_stopwatch(sw.laps[i]), &body, t.text_dim, Align::Right);
            ui.canvas.fill_rect(Rect::new(row.x, row.bottom() - 1, row.w, 1), t.border);
        }
        ui.canvas.restore_clip(old);
    }

    fn timer_tab(&mut self, ui: &mut Ui, r: Rect, now: u64) {
        let t = *ui.theme;
        let tm = &mut self.timer;
        if let TimerState::Running { end } = tm.state {
            if now >= end {
                tm.state = TimerState::Done { at: now };
            }
        }
        let left = tm.remaining(now);
        let flash = tm.flash_on(now);
        let (ring_r, ctl) = split(r, 52);

        // Progress ring with the remaining time inside.
        let c = ring_r.center();
        let radius = (ring_r.w.min(ring_r.h) / 2 - 18) as f32;
        let frac = if tm.total_ms() == 0 { 0.0 } else { left as f32 / tm.total_ms() as f32 };
        if flash {
            ui.canvas.fill_circle(c, radius - 12.0, t.danger.darken(120));
        }
        ui.canvas.stroke_circle(c, radius, 20.0, t.surface_alt);
        match tm.state {
            TimerState::Done { .. } => arc(ui.canvas, c, radius, 20.0, 1.0, if flash { t.danger } else { t.surface_alt }),
            _ => arc(ui.canvas, c, radius, 20.0, frac, t.accent),
        }
        let (huge, body) = (ui.fonts.huge, ui.fonts.body);
        let main = Rect::new(c.x - radius as i32, c.y - huge.line_height / 2 - 10, 2 * radius as i32, huge.line_height);
        let shown = if matches!(tm.state, TimerState::Done { .. }) { String::from("00:00") } else { fmt_timer(left) };
        ui.text(main, &shown, &huge, t.text, Align::Center);
        let sub = match tm.state {
            TimerState::Done { .. } => String::from("Time's up!"),
            TimerState::Paused { .. } => String::from("Paused"),
            _ => format!("of {}", fmt_timer(tm.total_ms())),
        };
        let sub_col = if matches!(tm.state, TimerState::Done { .. }) { t.danger.lighten(60) } else { t.text_dim };
        ui.text(Rect::new(main.x, main.bottom() + 4, main.w, body.line_height), &sub, &body, sub_col, Align::Center);

        // Minute / second steppers (only while idle) and the action buttons.
        let editable = tm.state == TimerState::Idle;
        let w = ctl.w.min(460);
        let block = Rect::new(ctl.x + (ctl.w - w) / 2, ctl.y + (ctl.h - 3 * 64 - 2 * 20).max(0) / 2, w, 3 * 64 + 2 * 20);
        let large = ui.fonts.large;
        let steppers = [("Minutes", tm.secs / 60, 1i32, 99u32), ("Seconds", tm.secs % 60, 5, 55)];
        for (i, (label, value, step, max)) in steppers.into_iter().enumerate() {
            let row = Rect::new(block.x, block.y + i as i32 * 84, block.w, 64);
            let (name, rest) = row.split_left(row.w - 3 * 64 - 2 * 12);
            ui.text(name, label, &body, if editable { t.text } else { t.text_dim }, Align::Left);
            let minus = Rect::new(rest.x, row.y, 64, 64);
            let val = Rect::new(minus.right() + 12, row.y, 64, 64);
            let plus = Rect::new(val.right() + 12, row.y, 64, 64);
            let fg = if editable { t.text } else { t.text_dim };
            let dec = ui.button_colored(minus, "−", t.surface_alt, fg);
            let inc = ui.button_colored(plus, "+", t.surface_alt, fg);
            ui.text(val, &format!("{value:02}"), &large, fg, Align::Center);
            if editable && (dec || inc) {
                let v = if inc { (value as i32 + step).min(max as i32) } else { (value as i32 - step).max(0) } as u32;
                let (mins, secs) = if i == 0 { (v, tm.secs % 60) } else { (tm.secs / 60, v) };
                tm.secs = mins * 60 + secs;
            }
        }
        let actions = Rect::new(block.x, block.y + 2 * 84, block.w, 64);
        let (a, b) = (actions.grid_cell(2, 1, 16, 0, 0), actions.grid_cell(2, 1, 16, 1, 0));
        let can_reset = tm.state != TimerState::Idle;
        if ui.button_colored(a, "Reset", t.surface_alt, if can_reset { t.text } else { t.text_dim }) && can_reset {
            tm.state = TimerState::Idle;
        }
        let (label, bg) = match tm.state {
            TimerState::Idle => ("Start", GREEN),
            TimerState::Running { .. } => ("Pause", t.danger),
            TimerState::Paused { .. } => ("Resume", GREEN),
            TimerState::Done { .. } => ("Restart", t.accent),
        };
        let can_start = tm.secs > 0;
        if ui.button_colored(b, label, bg, if can_start { Color::WHITE } else { t.text_dim }) && can_start {
            tm.state = match tm.state {
                TimerState::Idle | TimerState::Done { .. } => TimerState::Running { end: now + tm.total_ms() },
                TimerState::Running { end } => TimerState::Paused { left: end.saturating_sub(now) },
                TimerState::Paused { left } => TimerState::Running { end: now + left },
            };
        }
    }

    /// How soon the screen needs refreshing for what is visible or running.
    fn next_tick(&self, now: u64) -> Option<u32> {
        let mut tick: Option<u32> = None;
        let mut want = |ms: u64| tick = Some(tick.map_or(ms as u32, |t| t.min(ms as u32)));
        match self.tab {
            // Wake just after the next second boundary.
            Tab::Clock => want(1000 - now % 1000 + 5),
            Tab::Stopwatch if self.sw.running => want(50),
            _ => {}
        }
        match self.timer.state {
            TimerState::Running { end } if self.tab == Tab::Timer => want(100.min(end.saturating_sub(now)).max(1)),
            TimerState::Running { end } => want(end.saturating_sub(now).clamp(1, 1000)),
            TimerState::Done { .. } => want(250),
            _ => {}
        }
        tick
    }
}

impl App for Clock {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, t.bg);
        let now = sys.uptime_ms();
        // The timer finishes even while another tab is shown.
        if let TimerState::Running { end } = self.timer.state {
            if now >= end {
                self.timer.state = TimerState::Done { at: now };
            }
        }
        let inner = area.inset(24);
        let tab_w = inner.w.min(540);
        let tabs = Rect::new(inner.x + (inner.w - tab_w) / 2, inner.y, tab_w, 60);
        self.tabs(ui, tabs, now);
        let content = Rect::new(inner.x, tabs.bottom() + 20, inner.w, inner.bottom() - tabs.bottom() - 20);
        match self.tab {
            Tab::Clock => self.clock_tab(ui, content, sys),
            Tab::Stopwatch => self.stopwatch_tab(ui, content, now),
            Tab::Timer => self.timer_tab(ui, content, now),
        }
        self.tick = self.next_tick(now);
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        self.tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopwatch_format() {
        assert_eq!(fmt_stopwatch(0), "00:00.00");
        assert_eq!(fmt_stopwatch(61_234), "01:01.23");
        assert_eq!(fmt_stopwatch(59 * 60_000 + 59_999), "59:59.99");
        assert_eq!(fmt_stopwatch(3_600_000 + 5_010), "1:00:05.01");
    }

    #[test]
    fn timer_format_rounds_up() {
        assert_eq!(fmt_timer(0), "00:00");
        assert_eq!(fmt_timer(1), "00:01");
        assert_eq!(fmt_timer(300_000), "05:00");
        assert_eq!(fmt_timer(299_001), "05:00");
        assert_eq!(fmt_timer(298_999), "04:59");
        assert_eq!(fmt_timer(3_661_000), "1:01:01");
    }

    #[test]
    fn dates() {
        assert_eq!(weekday(2026, 10, 28), 3); // Wednesday
        assert_eq!(weekday(2000, 1, 1), 6); // Saturday
        assert_eq!(weekday(2024, 2, 29), 4); // Thursday
        let dt = DateTime { year: 2026, month: 10, day: 28, hour: 9, minute: 0, second: 0 };
        assert_eq!(fmt_date(&dt), "Wednesday, 28 October 2026");
    }

    #[test]
    fn timer_and_stopwatch_state() {
        let mut tm = Timer { secs: 10, state: TimerState::Idle };
        assert_eq!(tm.remaining(5_000), 10_000);
        tm.state = TimerState::Running { end: 15_000 };
        assert_eq!(tm.remaining(12_500), 2_500);
        assert_eq!(tm.remaining(20_000), 0);
        let sw = Stopwatch { running: true, started: 1_000, acc: 500, ..Default::default() };
        assert_eq!(sw.elapsed(3_000), 2_500);
    }
}
