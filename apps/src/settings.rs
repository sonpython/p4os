//! Settings: display brightness, volume, Wi-Fi, date & time and system
//! information, arranged as a section list with a content pane.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use p4os_sdk::config::{check_wifi, WIFI_SSID};
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, DateTime, Orientation, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "settings", name: "Settings", icon, create };

const OS_NAME: &str = "p4os 0.1";
/// Placeholder for unknown values (the bundled fonts have no em dash).
const DASH: &str = "--";
const MIN_YEAR: u16 = 2000;
const MAX_YEAR: u16 = 2099;

fn create() -> Box<dyn App> {
    Box::new(Settings::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x8395a7), Color::hex(0x576574));
    // Gear: a ring with eight teeth.
    let ctr = r.center();
    let s = r.w as f32 / 64.0;
    for i in 0..8 {
        let a = i as f32 * core::f32::consts::PI / 4.0;
        let (sn, cs) = (libm::sinf(a), libm::cosf(a));
        let p0 = Point::new(ctr.x + (cs * 12.0 * s) as i32, ctr.y + (sn * 12.0 * s) as i32);
        let p1 = Point::new(ctr.x + (cs * 21.0 * s) as i32, ctr.y + (sn * 21.0 * s) as i32);
        c.line(p0, p1, 7.0 * s, Color::WHITE);
    }
    c.fill_circle(ctr, 16.0 * s, Color::WHITE);
    c.fill_circle(ctr, 7.0 * s, Color::hex(0x6c7a89));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Display,
    Sound,
    Wifi,
    DateTime,
    About,
}

const SECTIONS: [(Section, &str); 5] = [
    (Section::Display, "Display"),
    (Section::Sound, "Sound"),
    (Section::Wifi, "Wi-Fi"),
    (Section::DateTime, "Date & time"),
    (Section::About, "About"),
];

/// A date/time field that the editor can step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Day,
    Month,
    Year,
    Hour,
    Minute,
}

const FIELDS: [(Field, &str); 5] =
    [(Field::Day, "Day"), (Field::Month, "Month"), (Field::Year, "Year"), (Field::Hour, "Hour"), (Field::Minute, "Minute")];

fn is_leap(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_valid(t: &DateTime) -> bool {
    (MIN_YEAR..=MAX_YEAR).contains(&t.year)
        && (1..=12).contains(&t.month)
        && t.day >= 1
        && t.day <= days_in_month(t.year, t.month)
        && t.hour < 24
        && t.minute < 60
        && t.second < 60
}

/// Step `v` by `delta` within `lo..=hi`, wrapping around at either end.
fn wrap(v: i32, delta: i32, lo: i32, hi: i32) -> i32 {
    let span = hi - lo + 1;
    lo + (v - lo + delta).rem_euclid(span)
}

/// Step one field of `t` by `delta`. Day, month, hour and minute wrap like
/// a clock face; the year is clamped. The day is clamped afterwards so the
/// result is always a real date (e.g. 31 Jan + 1 month = 28/29 Feb).
fn adjust(t: &mut DateTime, field: Field, delta: i32) {
    match field {
        Field::Day => {
            let dim = days_in_month(t.year, t.month) as i32;
            t.day = wrap(t.day as i32, delta, 1, dim) as u8;
        }
        Field::Month => t.month = wrap(t.month as i32, delta, 1, 12) as u8,
        Field::Year => t.year = (t.year as i32 + delta).clamp(MIN_YEAR as i32, MAX_YEAR as i32) as u16,
        Field::Hour => t.hour = wrap(t.hour as i32, delta, 0, 23) as u8,
        Field::Minute => t.minute = wrap(t.minute as i32, delta, 0, 59) as u8,
    }
    t.day = t.day.clamp(1, days_in_month(t.year, t.month));
}

fn field_text(t: &DateTime, f: Field) -> String {
    match f {
        Field::Day => format!("{:02}", t.day),
        Field::Month => format!("{:02}", t.month),
        Field::Year => format!("{}", t.year),
        Field::Hour => format!("{:02}", t.hour),
        Field::Minute => format!("{:02}", t.minute),
    }
}

/// Day of the week (0 = Monday) using Sakamoto's method.
fn weekday(t: &DateTime) -> usize {
    const OFF: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let m = t.month.clamp(1, 12) as usize;
    let y = t.year as i32 - (m < 3) as i32;
    let d = (y + y / 4 - y / 100 + y / 400 + OFF[m - 1] + t.day as i32).rem_euclid(7);
    ((d + 6) % 7) as usize
}

fn fmt_date(t: &DateTime) -> String {
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    const MONTHS: [&str; 12] =
        ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    let month = MONTHS[(t.month.clamp(1, 12) - 1) as usize];
    format!("{} {} {} {}", DAYS[weekday(t)], t.day, month, t.year)
}

fn fmt_bytes(n: usize) -> String {
    if n == 0 {
        DASH.to_string()
    } else if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f32 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", n / 1024)
    }
}

#[derive(Default)]
struct Settings {
    section: usize,
    /// Portrait only: a section page is open instead of the section list.
    open: bool,
    /// A tap may have changed state after part of the frame was drawn;
    /// ask for one immediate follow-up frame.
    redraw: bool,
    list_offset: i32,
    /// Date/time being edited; seeded from the clock when the section opens.
    draft: Option<DateTime>,
    /// Feedback after pressing Set.
    message: Option<&'static str>,
    /// Wi-Fi form; the SSID is seeded from the saved network on opening.
    ssid: String,
    password: String,
    wifi_loaded: bool,
    wifi_message: Option<String>,
}

/// Validate the Wi-Fi form; returns the SSID to join.
fn wifi_submission(ssid: &str, password: &str) -> Result<String, &'static str> {
    let ssid = ssid.trim();
    check_wifi(ssid, password)?;
    Ok(ssid.to_string())
}

impl Settings {
    fn heading(ui: &mut Ui, r: Rect, title: &str) -> Rect {
        let t = *ui.theme;
        let (head, rest) = r.split_top(52);
        let f = ui.fonts.title;
        ui.text(head, title, &f, t.text, Align::Left);
        Rect::new(rest.x, rest.y + 12, rest.w, rest.h - 12)
    }

    /// Key/value row inside a panel.
    fn row(ui: &mut Ui, r: Rect, key: &str, value: &str) {
        let t = *ui.theme;
        let f = ui.fonts.body;
        ui.text(Rect::new(r.x + 18, r.y, r.w / 2, r.h), key, &f, t.text_dim, Align::Left);
        ui.text(Rect::new(r.x + r.w / 3, r.y, r.w * 2 / 3 - 18, r.h), value, &f, t.text, Align::Right);
    }

    fn slider_panel(ui: &mut Ui, r: Rect, label: &str, value: &mut f32) -> bool {
        let t = *ui.theme;
        let p = Rect::new(r.x, r.y, r.w, 120);
        ui.panel(p);
        let inner = p.inset(20);
        let (head, rest) = inner.split_top(30);
        let (body, large) = (ui.fonts.body, ui.fonts.large);
        ui.text(head, label, &body, t.text, Align::Left);
        ui.text(head, &format!("{:.0}%", *value * 100.0), &large, t.accent, Align::Right);
        // Inset so the knob (radius 14) stays inside the panel at 0 and 100%.
        let slider = Rect::new(rest.x + 14, rest.y + 6, rest.w - 28, 48);
        ui.slider(slider, value)
    }

    fn display(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let r = Self::heading(ui, r, "Display");
        let mut b = sys.brightness();
        if Self::slider_panel(ui, r, "Brightness", &mut b) {
            // Never go fully dark: the user could not find the slider again.
            sys.set_brightness(b.max(0.05));
        }

        // Orientation: two segmented buttons. There is no accelerometer, so
        // rotation is always a manual choice.
        let p = Rect::new(r.x, r.y + 120 + 12, r.w, 150);
        ui.panel(p);
        let inner = p.inset(20);
        let (head, rest) = inner.split_top(30);
        let (body, small) = (ui.fonts.body, ui.fonts.small);
        ui.text(head, "Orientation", &body, t.text, Align::Left);
        let seg = Rect::new(rest.x, rest.y + 8, rest.w, 56);
        let current = sys.orientation();
        let options = [(Orientation::Portrait, "Portrait"), (Orientation::Landscape, "Landscape")];
        for (i, (o, label)) in options.iter().enumerate() {
            let cell = seg.grid_cell(2, 1, 8, i as i32, 0);
            let (bg, fg) = if *o == current { (t.accent, t.accent_text) } else { (t.surface_alt, t.text) };
            if ui.button_colored(cell, label, bg, fg) && *o != current {
                sys.set_orientation(*o);
            }
        }
        let note = Rect::new(rest.x, seg.bottom() + 2, rest.w, rest.bottom() - seg.bottom() - 2);
        ui.text(note, "Rotation is manual (no motion sensor).", &small, t.text_dim, Align::Left);
    }

    fn sound(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let r = Self::heading(ui, r, "Sound");
        let mut v = sys.audio().volume();
        if Self::slider_panel(ui, r, "Volume", &mut v) {
            sys.audio().set_volume(v);
        }
    }

    fn wifi(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let r = Self::heading(ui, r, "Wi-Fi");
        if !self.wifi_loaded {
            self.ssid = sys.config_get(WIFI_SSID).unwrap_or_default();
            self.wifi_loaded = true;
        }
        let (status, up) = {
            let net = sys.net();
            (net.status(), net.is_up())
        };
        // One status row (kept short so the form fits in landscape).
        let p = Rect::new(r.x, r.y, r.w, 56);
        ui.panel(p);
        let dot = if up { Color::hex(0x26de81) } else { t.danger };
        ui.canvas.fill_circle(Point::new(p.right() - 30, p.y + 28), 7.0, dot);
        let status = if status.is_empty() { DASH.to_string() } else { status };
        Self::row(ui, Rect::new(p.x, p.y, p.w - 30, p.h), "Status", &status);

        // Credentials: saved and joined on Connect (or Enter in the
        // password field, since the keyboard may cover the button).
        let form = Rect::new(r.x, p.bottom() + 12, r.w, 3 * 56 + 2 * 10 + 32);
        ui.panel(form);
        let inner = form.inset(16);
        let ssid_r = Rect::new(inner.x, inner.y, inner.w, 56);
        let pass_r = Rect::new(inner.x, ssid_r.bottom() + 10, inner.w, 56);
        let actions = Rect::new(inner.x, pass_r.bottom() + 10, inner.w, 56);
        ui.text_field(ssid_r, &mut self.ssid, "Network name (SSID)");
        let entered = ui.password_field(pass_r, &mut self.password, "Password");
        let connect_r = Rect::new(actions.right() - 160, actions.y, 160, actions.h);
        if ui.button_primary(connect_r, "Connect") || entered {
            match wifi_submission(&self.ssid, &self.password) {
                Ok(ssid) => {
                    sys.wifi_connect(&ssid, &self.password);
                    self.password.clear();
                    ui.state.focus = None;
                    self.wifi_message = Some(format!("Saved, joining {ssid}"));
                    self.ssid = ssid;
                }
                Err(e) => self.wifi_message = Some(e.to_string()),
            }
        }
        if let Some(msg) = &self.wifi_message {
            let small = ui.fonts.small;
            let msg_r = Rect::new(actions.x, actions.y, connect_r.x - actions.x - 12, actions.h);
            ui.text(msg_r, msg, &small, t.text_dim, Align::Left);
        }
    }

    fn date_time(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let r = Self::heading(ui, r, "Date & time");
        let now = sys.now();

        // Current clock.
        let p = Rect::new(r.x, r.y, r.w, 90);
        ui.panel(p);
        let inner = p.inset(16);
        let (large, body, small) = (ui.fonts.title, ui.fonts.body, ui.fonts.small);
        match now {
            Some(n) => {
                let (l, rt) = inner.split_left(inner.w / 2);
                ui.text(l, &format!("{:02}:{:02}:{:02}", n.hour, n.minute, n.second), &large, t.text, Align::Left);
                ui.text(rt, &fmt_date(&n), &body, t.text_dim, Align::Right);
            }
            None => ui.text(inner, "Clock not set", &large, t.text_dim, Align::Left),
        }

        // Editor: a column per field with + above and − below the value.
        let draft = self.draft.get_or_insert_with(|| match now {
            Some(n) if is_valid(&n) => n,
            _ => DateTime { year: 2026, month: 1, day: 1, ..Default::default() },
        });
        let ed = Rect::new(r.x, p.bottom() + 12, r.w, 196);
        ui.panel(ed);
        let cols = Rect::new(ed.x + 12, ed.y + 12, ed.w - 24, ed.h - 24);
        for (i, (field, name)) in FIELDS.iter().enumerate() {
            let c = cols.grid_cell(5, 1, 10, i as i32, 0);
            let plus = Rect::new(c.x, c.y + 22, c.w, 50);
            let value = Rect::new(c.x, plus.bottom(), c.w, 50);
            let minus = Rect::new(c.x, value.bottom(), c.w, 50);
            ui.text(Rect::new(c.x, c.y, c.w, 20), name, &small, t.text_dim, Align::Center);
            if ui.button(plus, "+") {
                adjust(draft, *field, 1);
            }
            ui.text(value, &field_text(draft, *field), &large, t.text, Align::Center);
            if ui.button(minus, "−") {
                adjust(draft, *field, -1);
            }
        }

        let actions = Rect::new(r.x, ed.bottom() + 12, r.w, 56);
        let set_r = Rect::new(actions.right() - 160, actions.y, 160, actions.h);
        let reset_r = Rect::new(set_r.x - 172, actions.y, 160, actions.h);
        if ui.button(reset_r, "Now") {
            if let Some(n) = now.filter(is_valid) {
                *draft = n;
            }
            self.message = None;
        }
        if ui.button_primary(set_r, "Set") {
            let mut new_t = *draft;
            new_t.second = 0;
            if is_valid(&new_t) {
                sys.set_time(new_t);
                self.message = Some("Clock updated");
            } else {
                self.message = Some("Invalid date");
            }
        }
        if let Some(msg) = self.message {
            ui.text(Rect::new(actions.x, actions.y, reset_r.x - actions.x - 12, actions.h), msg, &body, t.text_dim, Align::Left);
        }
    }

    fn about(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let r = Self::heading(ui, r, "About");
        let s = sys.stats();
        let chip = if s.chip.is_empty() { DASH.to_string() } else { s.chip.clone() };
        let mhz = if s.cpu_mhz == 0 { DASH.to_string() } else { format!("{} MHz", s.cpu_mhz) };
        let threads = if s.threads.is_empty() { DASH.to_string() } else { format!("{}", s.threads.len()) };
        let rows = [
            ("Operating system", OS_NAME.to_string()),
            ("Chip", chip),
            ("CPU clock", mhz),
            ("Heap", fmt_bytes(s.heap_total)),
            ("PSRAM", fmt_bytes(s.psram_total)),
            ("Kernel threads", threads),
            ("Installed apps", format!("{}", crate::APPS.len())),
            ("Rotation", "Manual (no accelerometer)".to_string()),
        ];
        let row_h = 50;
        let p = Rect::new(r.x, r.y, r.w, rows.len() as i32 * row_h);
        ui.panel(p);
        for (i, (k, v)) in rows.iter().enumerate() {
            let rr = Rect::new(p.x, p.y + i as i32 * row_h, p.w, row_h);
            Self::row(ui, rr, k, v);
            if i + 1 < rows.len() {
                ui.canvas.fill_rect(Rect::new(rr.x + 18, rr.bottom() - 1, rr.w - 36, 1), t.border);
            }
        }
    }
}

impl Settings {
    fn select(&mut self, i: usize) {
        if i != self.section {
            self.section = i;
            // Re-seed the date editor from the clock next time.
            self.draft = None;
            self.message = None;
            // Re-read the saved network next time; never keep a typed password.
            self.wifi_loaded = false;
            self.wifi_message = None;
            self.password.clear();
        }
    }

    fn content(&mut self, ui: &mut Ui, pane: Rect, sys: &mut dyn System) {
        match SECTIONS[self.section].0 {
            Section::Display => self.display(ui, pane, sys),
            Section::Sound => self.sound(ui, pane, sys),
            Section::Wifi => self.wifi(ui, pane, sys),
            Section::DateTime => self.date_time(ui, pane, sys),
            Section::About => self.about(ui, pane, sys),
        }
    }

    /// Landscape: section sidebar on the left, content on the right.
    fn sidebar_layout(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let (side, content) = area.split_left((area.w / 4).clamp(200, 280));
        ui.fill(side, t.surface);
        ui.canvas.fill_rect(Rect::new(side.right() - 1, side.y, 1, side.h), t.border);
        let names: [&str; 5] = SECTIONS.map(|(_, n)| n);
        let list_r = Rect::new(side.x + 8, side.y + 12, side.w - 16, side.h - 24);
        if let Some(i) = ui.list(list_r, &names, 64, &mut self.list_offset, Some(self.section)) {
            self.select(i);
        }
        self.content(ui, content.inset(24), sys);
    }

    /// Portrait: a full-width section list; tapping one opens its page,
    /// which has a back button.
    fn stacked_layout(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        if self.open {
            let (bar, rest) = area.split_top(64);
            ui.fill(bar, t.surface);
            ui.canvas.fill_rect(Rect::new(bar.x, bar.bottom() - 1, bar.w, 1), t.border);
            let back = Rect::new(bar.x + 8, bar.y + 8, 140, bar.h - 16);
            let back_tapped = ui.icon_button(back, |c, r, col| {
                let cy = r.y + r.h / 2;
                c.line(Point::new(r.x + 24, cy - 9), Point::new(r.x + 15, cy), 3.0, col);
                c.line(Point::new(r.x + 15, cy), Point::new(r.x + 24, cy + 9), 3.0, col);
            });
            let f = ui.fonts.body;
            ui.text(Rect::new(back.x + 34, back.y, back.w - 34, back.h), "Settings", &f, t.accent, Align::Left);
            if back_tapped {
                self.open = false;
            }
            self.content(ui, rest.inset(20), sys);
            return;
        }

        let names: [&str; 5] = SECTIONS.map(|(_, n)| n);
        let list_r = Rect::new(area.x + 12, area.y + 12, area.w - 24, area.h - 24);
        let row_h = 72;
        if let Some(i) = ui.list(list_r, &names, row_h, &mut self.list_offset, None) {
            self.select(i);
            self.open = true;
        }
        // Chevrons hint that rows open a page.
        for i in 0..names.len() as i32 {
            let cy = list_r.y + i * row_h + row_h / 2 - self.list_offset;
            let x = list_r.right() - 40;
            if cy > list_r.bottom() {
                break;
            }
            ui.canvas.line(Point::new(x, cy - 8), Point::new(x + 8, cy), 2.5, t.text_dim);
            ui.canvas.line(Point::new(x + 8, cy), Point::new(x, cy + 8), 2.5, t.text_dim);
        }
    }
}

impl App for Settings {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, t.bg);
        // Follow the screen, not `area`: the on-screen keyboard shrinks the
        // area, and switching layout would move the focused text field.
        if sys.orientation() == Orientation::Portrait {
            self.stacked_layout(ui, area, sys);
        } else {
            self.sidebar_layout(ui, area, sys);
        }
        self.redraw = ui.input.released;
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        if self.redraw {
            return Some(0);
        }
        // Keep the clock and link status live.
        match SECTIONS[self.section].0 {
            Section::DateTime | Section::Wifi => Some(1000),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(year: u16, month: u8, day: u8, hour: u8, minute: u8) -> DateTime {
        DateTime { year, month, day, hour, minute, second: 0 }
    }

    #[test]
    fn leap_years() {
        assert!(is_leap(2024));
        assert!(!is_leap(2026));
        assert!(!is_leap(1900));
        assert!(is_leap(2000));
    }

    #[test]
    fn month_lengths() {
        assert_eq!(days_in_month(2026, 1), 31);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2028, 2), 29);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 13), 0);
    }

    #[test]
    fn validation() {
        assert!(is_valid(&dt(2026, 10, 28, 9, 1)));
        assert!(is_valid(&dt(2028, 2, 29, 0, 0)));
        assert!(!is_valid(&dt(2026, 2, 29, 0, 0)));
        assert!(!is_valid(&dt(2026, 0, 1, 0, 0)));
        assert!(!is_valid(&dt(2026, 4, 31, 0, 0)));
        assert!(!is_valid(&dt(2026, 1, 1, 24, 0)));
        assert!(!is_valid(&dt(1999, 1, 1, 0, 0)));
    }

    #[test]
    fn fields_wrap_like_a_clock() {
        let mut t = dt(2026, 12, 31, 23, 59);
        adjust(&mut t, Field::Minute, 1);
        assert_eq!(t.minute, 0);
        adjust(&mut t, Field::Hour, 1);
        assert_eq!(t.hour, 0);
        adjust(&mut t, Field::Day, 1);
        assert_eq!(t.day, 1);
        adjust(&mut t, Field::Day, -1);
        assert_eq!(t.day, 31);
        adjust(&mut t, Field::Month, 1);
        assert_eq!(t.month, 1);
        adjust(&mut t, Field::Minute, -1);
        assert_eq!(t.minute, 59);
    }

    #[test]
    fn day_is_clamped_to_month_length() {
        let mut t = dt(2028, 1, 31, 0, 0);
        adjust(&mut t, Field::Month, 1);
        assert_eq!((t.month, t.day), (2, 29));
        adjust(&mut t, Field::Year, 1);
        assert_eq!((t.year, t.day), (2029, 28));
        assert!(is_valid(&t));
    }

    #[test]
    fn year_is_clamped() {
        let mut t = dt(MAX_YEAR, 6, 1, 0, 0);
        adjust(&mut t, Field::Year, 1);
        assert_eq!(t.year, MAX_YEAR);
        let mut t = dt(MIN_YEAR, 6, 1, 0, 0);
        adjust(&mut t, Field::Year, -1);
        assert_eq!(t.year, MIN_YEAR);
    }

    #[test]
    fn wifi_form_validation() {
        assert_eq!(wifi_submission("  home ", "password1"), Ok("home".to_string()));
        assert_eq!(wifi_submission("open-net", ""), Ok("open-net".to_string()));
        assert!(wifi_submission("", "password1").is_err());
        assert!(wifi_submission("home", "short").is_err());
    }

    #[test]
    fn weekday_and_date_text() {
        assert_eq!(weekday(&dt(2026, 10, 28, 0, 0)), 2); // Wednesday
        assert_eq!(weekday(&dt(2000, 1, 1, 0, 0)), 5); // Saturday
        assert_eq!(fmt_date(&dt(2026, 10, 28, 0, 0)), "Wednesday 28 October 2026");
    }
}
