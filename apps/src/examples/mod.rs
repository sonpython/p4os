//! Example apps for the developer guide (`docs/app-development.md`).
//!
//! The examples are ordinary built-in apps, but instead of adding one
//! launcher icon each they live behind a single "Examples" icon. This app is
//! that folder: it lists the examples and hosts the one the user picks,
//! forwarding `update`, `tick_interval_ms` and `on_close` to it. The
//! examples therefore build, run and get tested with the rest of the
//! workspace without crowding the home screen, and registering them costs
//! one entry in [`crate::APPS`].
//!
//! To add an example: write a module with an `INFO: AppInfo` (see
//! [`hello`]) and list it in [`EXAMPLES`].

use alloc::boxed::Box;
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, Point, Rect, System, Ui};

pub mod hello;
pub mod lap_timer;
pub mod weather;

#[cfg(test)]
pub(crate) mod testkit;

/// The examples, in the order the folder lists them.
pub static EXAMPLES: &[AppInfo] = &[hello::INFO, lap_timer::INFO, weather::INFO];

pub const INFO: AppInfo = AppInfo { id: "examples", name: "Examples", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Folder::default())
}

/// Launcher icon: a folder tile showing three tiny example icons.
fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x64748b), Color::hex(0x334155));
    let inner = r.inset(r.w / 6);
    for (i, ex) in EXAMPLES.iter().take(4).enumerate() {
        let cell = inner.grid_cell(2, 2, r.w / 14, i as i32 % 2, i as i32 / 2);
        (ex.icon)(c, cell);
    }
}

/// Height of the bar above a running example ("< Examples" and its name).
const BAR_H: i32 = 52;
const ROW_H: i32 = 96;

#[derive(Default)]
struct Folder {
    /// The example being shown and its index in [`EXAMPLES`].
    running: Option<(usize, Box<dyn App>)>,
}

impl Folder {
    fn open(&mut self, index: usize, sys: &mut dyn System) {
        self.close(sys);
        self.running = Some((index, (EXAMPLES[index].create)()));
    }

    fn close(&mut self, sys: &mut dyn System) {
        if let Some((_, mut app)) = self.running.take() {
            app.on_close(sys);
        }
    }

    fn list(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, t.bg);
        let (head, rest) = area.inset(16).split_top(48);
        let f = ui.fonts.body;
        ui.text(head, "Small apps that show how to use the SDK.", &f, t.text_dim, Align::Left);
        let mut tapped = None;
        for (i, ex) in EXAMPLES.iter().enumerate() {
            let row = Rect::new(rest.x, rest.y + i as i32 * (ROW_H + 12), rest.w, ROW_H);
            let (held, clicked) = ui.interact(row);
            let bg = if held { t.surface_alt } else { t.surface };
            ui.canvas.fill_round_rect(row, t.radius, bg);
            let icon = Rect::new(row.x + 14, row.y + 14, ROW_H - 28, ROW_H - 28);
            (ex.icon)(ui.canvas, icon);
            let large = ui.fonts.large;
            let label = Rect::new(icon.right() + 18, row.y, row.w - icon.w - 50, row.h);
            ui.text(label, ex.name, &large, t.text, Align::Left);
            if clicked {
                tapped = Some(i);
            }
        }
        if let Some(i) = tapped {
            self.open(i, sys);
        }
    }
}

impl App for Folder {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let Some((index, app)) = self.running.as_mut() else {
            self.list(ui, area, sys);
            return;
        };
        let (bar, body) = area.split_top(BAR_H);
        let t = *ui.theme;
        ui.fill(bar, t.surface_alt);
        let back = Rect::new(bar.x + 4, bar.y + 4, 160, bar.h - 8);
        let go_back = ui.icon_button(back, |c, r, col| {
            let cy = r.y + r.h / 2;
            c.line(Point::new(r.x + 22, cy - 9), Point::new(r.x + 13, cy), 3.0, col);
            c.line(Point::new(r.x + 13, cy), Point::new(r.x + 22, cy + 9), 3.0, col);
        });
        let f = ui.fonts.body;
        ui.text(Rect::new(back.x + 32, back.y, back.w - 32, back.h), "Examples", &f, t.text, Align::Left);
        ui.text(bar, EXAMPLES[*index].name, &f, t.text_dim, Align::Center);

        // The example only draws inside its own area.
        let clip = body.intersect(&ui.canvas.clip());
        let old = ui.canvas.set_clip(clip);
        app.update(ui, body, sys);
        ui.canvas.restore_clip(old);

        if go_back {
            self.close(sys);
        }
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        self.running.as_ref().and_then(|(_, app)| app.tick_interval_ms())
    }

    fn on_close(&mut self, sys: &mut dyn System) {
        self.close(sys);
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::Harness;
    use super::*;

    #[test]
    fn ids_are_unique() {
        for (i, a) in EXAMPLES.iter().enumerate() {
            assert!(EXAMPLES[i + 1..].iter().all(|b| b.id != a.id), "duplicate id {}", a.id);
            assert!(crate::APPS.iter().all(|b| b.id != a.id), "{} clashes with a built-in app", a.id);
        }
    }

    #[test]
    fn opens_an_example_and_goes_back() {
        let mut h = Harness::new();
        let mut folder = Folder::default();
        h.frame(&mut folder);
        // The first row (below the 16 px margin and the 48 px heading).
        h.tap(&mut folder, Point::new(240, 16 + 48 + ROW_H / 2));
        assert_eq!(folder.running.as_ref().map(|(i, _)| *i), Some(0));
        // "< Examples" in the bar closes it again.
        h.tap(&mut folder, Point::new(60, BAR_H / 2));
        assert!(folder.running.is_none());
    }
}
