//! The desktop shell: status bar, launcher grid, app chrome and the
//! on-screen keyboard. The kernel (or the simulator) calls [`Desktop::frame`]
//! for every input event or requested tick and presents the canvas.

use crate::APPS;
use crate::lock::{self, Lock};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use p4os_sdk::gui::font::builtin::{self, Fonts};
use p4os_sdk::gui::keyboard::{self, Keyboard};
use p4os_sdk::gui::ui::{Align, Input, Theme, UiState};
use p4os_sdk::{
    App, Audio, Canvas, Color, DateTime, FileSystem, Image, Network, Orientation, Point, Rect, System, SystemStats, Ui,
};

pub const STATUS_H: i32 = 36;
pub const TITLE_H: i32 = 56;
const ICON: i32 = 112;
const STATUS_TICK_MS: u32 = 1000;

pub struct Desktop {
    fonts: Fonts,
    theme: Theme,
    state: UiState,
    keyboard: Keyboard,
    keyboard_open: bool,
    running: Option<(usize, Box<dyn App>)>,
    lock: Lock,
}

impl Default for Desktop {
    fn default() -> Self {
        Self::new()
    }
}

impl Desktop {
    pub fn new() -> Self {
        Self {
            fonts: builtin::load(),
            theme: Theme::dark(),
            state: UiState::default(),
            keyboard: Keyboard::default(),
            keyboard_open: false,
            running: None,
            lock: Lock::default(),
        }
    }

    /// True while the lock screen is up: nothing else is drawn and no input
    /// reaches apps or system gestures.
    pub fn is_locked(&self) -> bool {
        self.lock.is_locked()
    }

    /// Lock immediately (if a passcode is configured).
    pub fn lock_now(&mut self, sys: &mut dyn System) {
        self.lock.lock_now(sys);
    }

    /// The display turned off (power management).
    pub fn on_screen_off(&mut self, sys: &mut dyn System) {
        self.lock.on_screen_off(sys);
    }

    /// The display woke up again (power management).
    pub fn on_wake(&mut self, sys: &mut dyn System) {
        self.lock.on_wake(sys);
    }

    pub fn running_app(&self) -> Option<&'static str> {
        self.running.as_ref().map(|(i, _)| APPS[*i].id)
    }

    pub fn launch(&mut self, id: &str, sys: &mut dyn System) {
        self.close(sys);
        if let Some(i) = APPS.iter().position(|a| a.id == id) {
            self.running = Some((i, (APPS[i].create)()));
            self.state = UiState::default();
        }
    }

    fn close(&mut self, sys: &mut dyn System) {
        if let Some((_, mut app)) = self.running.take() {
            app.on_close(sys);
        }
        self.keyboard_open = false;
        self.state = UiState::default();
    }

    /// Render one frame. Returns how many milliseconds until the desktop
    /// wants another frame without input (`None`: only on input).
    pub fn frame(&mut self, canvas: &mut Canvas, input: &Input, sys: &mut dyn System) -> Option<u32> {
        let screen = canvas.bounds();
        let (status, body) = screen.split_top(STATUS_H);
        let mut proxy = SysProxy { inner: sys, launch: None, lock: false };

        // The lock screen covers everything and takes all input. The touch
        // that found the screen locked only wakes it.
        let just_locked = self.lock.check(input, proxy.inner);
        if self.lock.is_locked() {
            let quiet = Input { pos: input.pos, down: input.down, press_pos: input.press_pos, ..Default::default() };
            let input = if just_locked { &quiet } else { input };
            return self.lock.frame(canvas, input, &self.fonts, &self.theme, proxy.inner);
        }

        let keyboard_was_open = self.keyboard_open;

        let next = if self.running.is_some() {
            self.app_frame(canvas, input, body, &mut proxy)
        } else {
            self.home_frame(canvas, input, body, &mut proxy);
            Some(STATUS_TICK_MS)
        };
        self.status_bar(canvas, input, status, &mut proxy);

        if proxy.lock {
            self.lock.lock_now(proxy.inner);
            return Some(0);
        }
        if let Some(id) = proxy.launch.take() {
            self.launch(&id, proxy.inner);
            return Some(0);
        }
        // Layout changes (keyboard shown/hidden) need an immediate redraw,
        // and so does any interaction: widgets react while drawing, so state
        // changed by a tap or key only shows up in the following frame.
        let interacted = input.pressed || input.released || !input.keys.is_empty();
        if self.keyboard_open != keyboard_was_open || interacted {
            return Some(0);
        }
        Some(next.map_or(STATUS_TICK_MS, |n| n.min(STATUS_TICK_MS)))
    }

    fn ui<'f, 'c>(&'f mut self, canvas: &'f mut Canvas<'c>, input: &'f Input) -> Ui<'f, 'c> {
        Ui { canvas, input, theme: &self.theme, fonts: &self.fonts, state: &mut self.state }
    }

    fn status_bar(&mut self, canvas: &mut Canvas, input: &Input, r: Rect, sys: &mut SysProxy) {
        let stats = sys.stats();
        let clock = match sys.now() {
            Some(t) => format!("{:02}:{:02}", t.hour, t.minute),
            None => {
                let s = stats.uptime_ms / 1000;
                format!("up {:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
            }
        };
        let used = (stats.heap_used + stats.psram_used) / 1024;
        let total = (stats.heap_total + stats.psram_total) / 1024;
        let right = if r.w >= 800 {
            format!("CPU {:>3.0}%   RAM {}K/{}K   {}", stats.cpu_load * 100.0, used, total, sys.net().status())
        } else {
            format!("CPU {:.0}%  RAM {}M", stats.cpu_load * 100.0, used / 1024)
        };
        let orientation = sys.orientation();
        let lockable = lock::is_configured(sys);
        let mut ui = self.ui(canvas, input);
        ui.fill(r, Color::hex(0x080a0f));
        let f = ui.fonts.small;
        let (dim, text) = (ui.theme.text_dim, ui.theme.text);
        // Rotate button: a small phone outline in the target orientation.
        let rot = Rect::new(r.x + 4, r.y, 44, r.h);
        let rotate = ui.icon_button(rot, |c, b, col| {
            let (w, h) = if orientation == Orientation::Portrait { (22, 14) } else { (14, 22) };
            let phone = Rect::new(b.x + (b.w - w) / 2, b.y + (b.h - h) / 2, w, h);
            c.stroke_round_rect(phone, 3, 2, col);
        });
        // Lock button, shown only when there is a lock to engage.
        let mut lock_tapped = false;
        let mut label_x = r.x + 52;
        if lockable {
            let lr = Rect::new(r.x + 52, r.y, 40, r.h);
            lock_tapped = ui.icon_button(lr, |c, b, col| lock::padlock(c, b.center(), 16, col));
            label_x = lr.right() + 6;
        }
        ui.text(Rect::new(label_x, r.y, 120, r.h), "p4os", &f, dim, Align::Left);
        ui.text(r, &clock, &f, text, Align::Center);
        ui.text(Rect::new(r.x, r.y, r.w - 14, r.h), &right, &f, dim, Align::Right);
        if rotate {
            sys.set_orientation(orientation.toggled());
        }
        if lock_tapped {
            sys.lock_now();
        }
    }

    fn home_frame(&mut self, canvas: &mut Canvas, input: &Input, body: Rect, sys: &mut SysProxy) {
        canvas.fill_gradient(body, Color::hex(0x1b2a4a), Color::hex(0x0b0f18));
        // 4 columns in portrait, 6 in landscape.
        let cols = (body.w / 150).max(3);
        let cell_w = body.w / cols;
        let cell_h = ICON + 56;
        let top = body.y + 40;
        let mut tapped = None;
        let mut ui = self.ui(canvas, input);
        for (i, app) in APPS.iter().enumerate() {
            let (col, row) = (i as i32 % cols, i as i32 / cols);
            let cell = Rect::new(body.x + col * cell_w, top + row * cell_h, cell_w, cell_h);
            let icon = Rect::new(cell.x + (cell.w - ICON) / 2, cell.y, ICON, ICON);
            let (held, clicked) = ui.interact(cell);
            (app.icon)(ui.canvas, icon);
            if held {
                ui.canvas.fill_rect_alpha(icon, Color::BLACK, 70);
            }
            let f = ui.fonts.body;
            let c = ui.theme.text;
            ui.text(Rect::new(cell.x, icon.bottom() + 8, cell.w, 28), app.name, &f, c, Align::Center);
            if clicked {
                tapped = Some(app.id);
            }
        }
        if let Some(id) = tapped {
            sys.launch(id);
        }
    }

    fn app_frame(&mut self, canvas: &mut Canvas, input: &Input, body: Rect, sys: &mut SysProxy) -> Option<u32> {
        let (title_r, mut area) = body.split_top(TITLE_H);
        let mut app_input = input.clone();

        // Keyboard first: its key goes to the app in the same frame, and
        // touches on it must not reach the app.
        if self.keyboard_open {
            let (rest, kb) = area.split_top(area.h - keyboard::HEIGHT);
            area = rest;
            let mut kb_state = core::mem::take(&mut self.keyboard);
            let key = {
                let mut ui = self.ui(canvas, input);
                kb_state.show(&mut ui, kb)
            };
            self.keyboard = kb_state;
            if kb.contains(input.press_pos) {
                app_input.pressed = false;
                app_input.released = false;
                app_input.down = false;
            }
            if let Some(k) = key {
                app_input.keys.push(k);
            }
        }

        let (index, mut app) = self.running.take().expect("app_frame without app");
        let name = APPS[index].name;
        let mut home = false;
        {
            let mut ui = self.ui(canvas, &app_input);
            let t = *ui.theme;
            ui.fill(title_r, t.surface);
            ui.canvas.fill_rect(Rect::new(title_r.x, title_r.bottom() - 1, title_r.w, 1), t.border);
            let back = Rect::new(title_r.x + 8, title_r.y + 6, 130, title_r.h - 12);
            home = ui.icon_button(back, |c, r, col| {
                let cy = r.y + r.h / 2;
                c.line(Point::new(r.x + 26, cy - 10), Point::new(r.x + 16, cy), 3.0, col);
                c.line(Point::new(r.x + 16, cy), Point::new(r.x + 26, cy + 10), 3.0, col);
            }) || home;
            let f = ui.fonts.body;
            ui.text(Rect::new(back.x + 36, back.y, 90, back.h), "Home", &f, t.text, Align::Left);
            let bold = ui.fonts.bold;
            ui.text(title_r, name, &bold, t.text, Align::Center);
            for k in &input.keys {
                if *k == p4os_sdk::gui::ui::Key::Escape {
                    home = true;
                }
            }

            ui.state.wants_keyboard = false;
            let old = ui.canvas.set_clip(area);
            app.update(&mut ui, area, sys);
            ui.canvas.restore_clip(old);
        }
        self.keyboard_open = self.state.wants_keyboard;
        let tick = app.tick_interval_ms();
        self.running = Some((index, app));
        if home {
            self.close(sys.inner);
        }
        tick
    }
}

/// Wraps the OS `System` to intercept `launch` requests for the desktop.
struct SysProxy<'a> {
    inner: &'a mut dyn System,
    launch: Option<String>,
    /// An app or the status bar asked to lock the screen.
    lock: bool,
}

impl System for SysProxy<'_> {
    fn uptime_ms(&self) -> u64 {
        self.inner.uptime_ms()
    }
    fn now(&self) -> Option<DateTime> {
        self.inner.now()
    }
    fn set_time(&mut self, t: DateTime) {
        self.inner.set_time(t)
    }
    fn fs(&mut self) -> &mut dyn FileSystem {
        self.inner.fs()
    }
    fn audio(&mut self) -> &mut dyn Audio {
        self.inner.audio()
    }
    fn net(&mut self) -> &mut dyn Network {
        self.inner.net()
    }
    fn stats(&self) -> SystemStats {
        self.inner.stats()
    }
    fn decode_image(&mut self, data: &[u8], max_w: i32, max_h: i32) -> Option<Image> {
        self.inner.decode_image(data, max_w, max_h)
    }
    fn brightness(&self) -> f32 {
        self.inner.brightness()
    }
    fn set_brightness(&mut self, value: f32) {
        self.inner.set_brightness(value)
    }
    fn run_command(&mut self, line: &str) -> String {
        self.inner.run_command(line)
    }
    fn launch(&mut self, app_id: &str) {
        self.launch = Some(String::from(app_id));
    }
    fn orientation(&self) -> Orientation {
        self.inner.orientation()
    }
    fn set_orientation(&mut self, o: Orientation) {
        self.inner.set_orientation(o)
    }
    fn wifi_connect(&mut self, ssid: &str, password: &str) {
        self.inner.wifi_connect(ssid, password)
    }
    fn config_get(&self, key: &str) -> Option<String> {
        self.inner.config_get(key)
    }
    fn config_set(&mut self, key: &str, value: &str) {
        self.inner.config_set(key, value)
    }
    fn fill_random(&mut self, buf: &mut [u8]) -> bool {
        self.inner.fill_random(buf)
    }
    fn lock_now(&mut self) {
        self.lock = true;
    }
}
