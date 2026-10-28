//! The smallest useful app: a greeting and a counter.
//!
//! Shows the shape every built-in app has:
//! * an [`AppInfo`] with an id, a name, an icon and a constructor;
//! * a struct holding the app state between frames;
//! * [`App::update`], which draws the whole app every frame and reacts to
//!   this frame's input in the same pass (immediate mode);
//! * layout computed from `area`, so portrait and landscape both work.

use alloc::boxed::Box;
use alloc::format;
use p4os_sdk::gui::ui::{Align, Key};
use p4os_sdk::{App, AppInfo, Canvas, Color, Rect, System, Ui};

/// What the launcher (here: the Examples folder) needs to know.
pub const INFO: AppInfo = AppInfo { id: "example-hello", name: "Hello Counter", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Hello::default())
}

/// Icons are drawn with canvas primitives at any size.
fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x34d399), Color::hex(0x059669));
    let m = r.center();
    let bar = (r.w / 10).max(2);
    let len = r.w / 2;
    c.fill_round_rect(Rect::new(m.x - len / 2, m.y - bar / 2, len, bar), bar / 2, Color::WHITE);
    c.fill_round_rect(Rect::new(m.x - bar / 2, m.y - len / 2, bar, len), bar / 2, Color::WHITE);
}

/// Everything the app remembers between frames.
#[derive(Default)]
struct Hello {
    count: Counter,
}

/// The app logic, kept apart from drawing so it is easy to unit-test.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Counter(pub i32);

impl Counter {
    /// The counter stays within -999..=999 so it always fits on screen.
    pub const LIMIT: i32 = 999;

    pub fn add(&mut self, delta: i32) {
        self.0 = (self.0 + delta).clamp(-Self::LIMIT, Self::LIMIT);
    }

    pub fn reset(&mut self) {
        self.0 = 0;
    }
}

impl App for Hello {
    fn update(&mut self, ui: &mut Ui, area: Rect, _sys: &mut dyn System) {
        // Copy the theme out: `ui` is borrowed mutably by every widget call.
        let t = *ui.theme;
        ui.fill(area, t.bg);

        // A hardware or USB keyboard works too: + and - change the count.
        for key in ui.input.keys.clone() {
            match key {
                Key::Char('+') | Key::Up => self.count.add(1),
                Key::Char('-') | Key::Down => self.count.add(-1),
                _ => {}
            }
        }

        // Layout: a text block on top and a row of buttons below it. In
        // landscape the screen is wider than tall, so the buttons get less
        // height and the text block more width; the code is the same.
        let inner = area.inset(24);
        let buttons_h = 96;
        let (top, bottom) = inner.split_top(inner.h - buttons_h);
        let (greeting, number) = top.split_top(top.h / 3);

        let (title, body) = (ui.fonts.title, ui.fonts.body);
        ui.text(greeting.split_top(greeting.h / 2).0, "Hello, p4os!", &title, t.text, Align::Center);
        let hint = "Tap the buttons to count";
        ui.text(greeting.split_top(greeting.h / 2).1, hint, &body, t.text_dim, Align::Center);

        ui.panel(number.inset(8));
        let huge = ui.fonts.huge;
        let color = if self.count.0 < 0 { t.danger } else { t.text };
        ui.text(number, &format!("{}", self.count.0), &huge, color, Align::Center);

        // Widgets return true on the frame they are tapped.
        if ui.button(bottom.grid_cell(3, 1, 16, 0, 0), "-1") {
            self.count.add(-1);
        }
        if ui.button_colored(bottom.grid_cell(3, 1, 16, 1, 0), "Reset", t.danger, Color::WHITE) {
            self.count.reset();
        }
        if ui.button_primary(bottom.grid_cell(3, 1, 16, 2, 0), "+1") {
            self.count.add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::examples::testkit::Harness;
    use p4os_sdk::Orientation;

    #[test]
    fn counter_is_clamped() {
        let mut c = Counter::default();
        c.add(5);
        c.add(-2);
        assert_eq!(c, Counter(3));
        c.add(10_000);
        assert_eq!(c, Counter(Counter::LIMIT));
        c.add(-10_000);
        assert_eq!(c, Counter(-Counter::LIMIT));
        c.reset();
        assert_eq!(c, Counter(0));
    }

    #[test]
    fn buttons_change_the_count() {
        let mut h = Harness::new();
        let mut app = Hello::default();
        h.frame(&mut app);
        // Portrait 480x800: the button row is the bottom 96 px inside the
        // 24 px margin, split into three cells.
        let row = Rect::new(24, 800 - 24 - 96, 480 - 48, 96);
        let [minus, reset, plus] = [0, 1, 2].map(|col| row.grid_cell(3, 1, 16, col, 0).center());
        h.tap(&mut app, plus);
        h.tap(&mut app, plus);
        assert_eq!(app.count, Counter(2));
        h.tap(&mut app, minus);
        assert_eq!(app.count, Counter(1));
        h.tap(&mut app, reset);
        assert_eq!(app.count, Counter(0));
    }

    #[test]
    fn works_in_landscape() {
        let mut h = Harness::new();
        h.orientation = Orientation::Landscape;
        let mut app = Hello::default();
        let row = Rect::new(24, 480 - 24 - 96, 800 - 48, 96);
        h.tap(&mut app, row.grid_cell(3, 1, 16, 2, 0).center());
        assert_eq!(app.count, Counter(1));
    }
}
