//! A stopwatch whose laps survive a reboot.
//!
//! Shows:
//! * timing with [`System::uptime_ms`] (monotonic, unlike the wall clock);
//! * periodic redraws with [`App::tick_interval_ms`], only while running;
//! * persisting a small value with [`System::config_get`] /
//!   [`System::config_set`], written only when something changed;
//! * loading state lazily on the first frame, because `System` is only
//!   handed to `update`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "example-laps", name: "Lap Timer", icon, create };

fn create() -> Box<dyn App> {
    Box::new(LapTimer::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0xf472b6), Color::hex(0xbe185d));
    let m = Point::new(r.center().x, r.center().y + r.h / 20);
    let rad = r.w as f32 * 0.3;
    c.fill_circle(m, rad, Color::WHITE);
    c.fill_rect(Rect::new(m.x - r.w / 16, r.y + r.h / 8, r.w / 8, r.h / 10), Color::WHITE);
    c.line(m, Point::new(m.x + (rad * 0.5) as i32, m.y - (rad * 0.5) as i32), r.w as f32 / 20.0, Color::hex(0xbe185d));
}

/// Settings key. Apps may store their own keys: printable ASCII without
/// spaces, at most 32 bytes; values at most 255 bytes. Prefix keys with the
/// app id so they cannot clash with OS keys (`wifi.*`, `ui.*`, `lock.*`).
pub const LAPS_KEY: &str = "example-laps.laps";
/// The newest laps kept. Twelve 7-digit values fit easily in 255 bytes.
pub const MAX_LAPS: usize = 12;
/// 99:59.99, the most the display shows; longer laps are clamped.
pub const MAX_LAP_MS: u64 = 99 * 60_000 + 59_990;
/// Redraw rate while running: 20 frames per second is smooth enough for
/// hundredths and leaves the CPU to other threads.
const TICK_MS: u32 = 50;

/// The stopwatch itself, free of drawing so it can be unit-tested with a
/// made-up clock.
#[derive(Default, Debug)]
pub struct Stopwatch {
    running: bool,
    /// Uptime when the current run started.
    started: u64,
    /// Time accumulated by earlier runs.
    acc: u64,
    /// Elapsed time at the last lap mark.
    mark: u64,
    /// Lap durations, oldest first.
    pub laps: Vec<u64>,
}

impl Stopwatch {
    pub fn elapsed(&self, now: u64) -> u64 {
        self.acc + if self.running { now.saturating_sub(self.started) } else { 0 }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn start(&mut self, now: u64) {
        if !self.running {
            self.running = true;
            self.started = now;
        }
    }

    pub fn stop(&mut self, now: u64) {
        if self.running {
            self.acc = self.elapsed(now);
            self.running = false;
        }
    }

    /// Record a lap; returns its duration.
    pub fn lap(&mut self, now: u64) -> u64 {
        let total = self.elapsed(now);
        let lap = (total - self.mark).min(MAX_LAP_MS);
        self.mark = total;
        self.laps.push(lap);
        if self.laps.len() > MAX_LAPS {
            self.laps.remove(0);
        }
        lap
    }

    /// Back to zero, forgetting the laps.
    pub fn reset(&mut self) {
        *self = Stopwatch::default();
    }

    /// Index of the fastest lap.
    pub fn best(&self) -> Option<usize> {
        (0..self.laps.len()).min_by_key(|&i| self.laps[i])
    }
}

/// `1234567` -> `"20:34.56"`.
pub fn format_ms(ms: u64) -> String {
    let ms = ms.min(MAX_LAP_MS);
    format!("{:02}:{:02}.{:02}", ms / 60_000, ms / 1000 % 60, ms % 1000 / 10)
}

/// Laps as stored in the settings: decimal milliseconds, comma-separated.
pub fn encode_laps(laps: &[u64]) -> String {
    let mut out = String::new();
    for (i, lap) in laps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", lap);
    }
    out
}

/// Parse a stored value. Settings can hold anything (an older version of
/// the app, a typo in the shell), so entries that do not parse are skipped
/// rather than trusted.
pub fn decode_laps(value: &str) -> Vec<u64> {
    let mut laps: Vec<u64> = value
        .split(',')
        .filter_map(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.min(MAX_LAP_MS))
        .collect();
    if laps.len() > MAX_LAPS {
        laps.drain(..laps.len() - MAX_LAPS);
    }
    laps
}

#[derive(Default)]
struct LapTimer {
    watch: Stopwatch,
    loaded: bool,
    list_scroll: i32,
}

impl LapTimer {
    fn save(&self, sys: &mut dyn System) {
        // config_set writes a flash sector (tens of milliseconds), so it is
        // called on a button press, never every frame.
        sys.config_set(LAPS_KEY, &encode_laps(&self.watch.laps));
    }
}

impl App for LapTimer {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        if !self.loaded {
            self.loaded = true;
            self.watch.laps = decode_laps(&sys.config_get(LAPS_KEY).unwrap_or_default());
        }
        let now = sys.uptime_ms();
        let t = *ui.theme;
        ui.fill(area, t.bg);

        let inner = area.inset(20);
        let (display, rest) = inner.split_top(150);
        let (buttons, list) = rest.split_top(110);

        ui.panel(display);
        let huge = ui.fonts.huge;
        ui.text(display, &format_ms(self.watch.elapsed(now)), &huge, t.text, Align::Center);

        let buttons = Rect::new(buttons.x, buttons.y + 20, buttons.w, 76);
        let left = buttons.grid_cell(2, 1, 16, 0, 0);
        let right = buttons.grid_cell(2, 1, 16, 1, 0);
        if self.watch.is_running() {
            if ui.button(left, "Lap") {
                self.watch.lap(now);
                self.save(sys);
            }
            if ui.button_colored(right, "Stop", t.danger, Color::WHITE) {
                self.watch.stop(now);
            }
        } else {
            let can_reset = self.watch.elapsed(now) > 0 || !self.watch.laps.is_empty();
            if ui.button(left, "Reset") && can_reset {
                self.watch.reset();
                sys.config_remove(LAPS_KEY);
            }
            if ui.button_primary(right, "Start") {
                self.watch.start(now);
            }
        }

        // Laps, newest first, the fastest one in green.
        let best = self.watch.best();
        let rows: Vec<String> = (0..self.watch.laps.len())
            .rev()
            .map(|i| {
                let star = if Some(i) == best && self.watch.laps.len() > 1 { "   fastest" } else { "" };
                format!("Lap {:>2}    {}{}", i + 1, format_ms(self.watch.laps[i]), star)
            })
            .collect();
        if rows.is_empty() {
            let body = ui.fonts.body;
            let msg = "Laps appear here and are kept after a restart.";
            ui.text(list.split_top(40).0, msg, &body, t.text_dim, Align::Center);
        } else {
            let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
            let selected = best.map(|b| self.watch.laps.len() - 1 - b);
            ui.list(list, &refs, 56, &mut self.list_scroll, selected);
        }
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        // No ticks while stopped: the app then only redraws on input.
        self.watch.is_running().then_some(TICK_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::examples::testkit::Harness;

    #[test]
    fn formats_minutes_seconds_hundredths() {
        assert_eq!(format_ms(0), "00:00.00");
        assert_eq!(format_ms(1_234_567), "20:34.56");
        assert_eq!(format_ms(u64::MAX), "99:59.99");
    }

    #[test]
    fn measures_runs_and_laps() {
        let mut w = Stopwatch::default();
        w.start(1_000);
        assert_eq!(w.elapsed(3_500), 2_500);
        assert_eq!(w.lap(3_500), 2_500);
        w.stop(4_000);
        assert_eq!(w.elapsed(99_000), 3_000, "stopped time does not count");
        w.start(10_000);
        assert_eq!(w.lap(11_000), 1_500);
        assert_eq!(w.laps, [2_500, 1_500]);
        assert_eq!(w.best(), Some(1));
        w.reset();
        assert_eq!(w.elapsed(20_000), 0);
        assert!(w.laps.is_empty());
    }

    #[test]
    fn keeps_only_the_newest_laps() {
        let mut w = Stopwatch::default();
        w.start(0);
        for i in 1..=(MAX_LAPS as u64 + 3) {
            w.lap(i * 1_000);
        }
        assert_eq!(w.laps.len(), MAX_LAPS);
        assert!(encode_laps(&[MAX_LAP_MS; MAX_LAPS]).len() <= 255);
    }

    #[test]
    fn stored_laps_round_trip_and_bad_values_are_skipped() {
        let laps = [61_230, 59_990, 5];
        assert_eq!(decode_laps(&encode_laps(&laps)), laps);
        assert_eq!(decode_laps(""), [] as [u64; 0]);
        assert_eq!(decode_laps("12,abc,,-4, 30 ,99999999999"), [12, 30, MAX_LAP_MS]);
        let many = encode_laps(&[1; MAX_LAPS + 5]);
        assert_eq!(decode_laps(&many).len(), MAX_LAPS);
    }

    #[test]
    fn laps_persist_across_app_restarts() {
        let mut h = Harness::new();
        h.sys.uptime = 5_000;
        let mut app = LapTimer::default();
        h.frame(&mut app);
        let inner = Rect::new(20, 20, 440, 760);
        let buttons = Rect::new(20, 20 + 150 + 20, 440, 76);
        let (left, right) = (buttons.grid_cell(2, 1, 16, 0, 0).center(), buttons.grid_cell(2, 1, 16, 1, 0).center());
        assert!(inner.contains(left));

        h.tap(&mut app, right); // Start
        assert!(app.watch.is_running());
        assert_eq!(app.tick_interval_ms(), Some(TICK_MS));
        h.sys.uptime = 7_340;
        h.tap(&mut app, left); // Lap
        h.tap(&mut app, right); // Stop
        assert_eq!(app.tick_interval_ms(), None);
        assert_eq!(h.sys.config.get(LAPS_KEY).map(String::as_str), Some("2340"));
        assert_eq!(h.sys.config_writes, 1, "only the lap wrote to flash");

        // A fresh instance (the user reopened the app, or the board
        // rebooted) finds the lap again.
        let mut again = LapTimer::default();
        h.frame(&mut again);
        assert_eq!(again.watch.laps, [2_340]);
        h.tap(&mut again, left); // Reset
        assert!(again.watch.laps.is_empty());
        assert_eq!(h.sys.config.get(LAPS_KEY), None);
    }
}
