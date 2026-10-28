//! The app switcher: running apps as cards (title and a snapshot of the
//! last frame) in a horizontal strip, most recent first. Tap a card to
//! switch to it, swipe it up to close the app, drag sideways to scroll.

use super::thumb::Thumbnail;
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{AppState, Canvas, Color, Point, Rect, Ui};

const GAP: i32 = 24;
const HEADER_H: i32 = 64;
const CARD_TITLE_H: i32 = 40;
/// Upward travel that closes a card on release.
const CLOSE_DIST: i32 = 110;
const SLOP: i32 = 12;
const OPEN_ANIM_MS: u64 = 180;

pub struct Card<'a> {
    pub tag: u8,
    pub name: &'a str,
    pub icon: fn(&mut Canvas, Rect),
    pub thumb: Option<&'a Thumbnail>,
    pub state: AppState,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Open(u8),
    Close(u8),
    CloseAll,
    /// Tap outside the cards: leave the switcher.
    Dismiss,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy)]
struct Drag {
    card: Option<usize>,
    start: Point,
    start_scroll: i32,
    axis: Option<Axis>,
}

pub struct Switcher {
    scroll: i32,
    drag: Option<Drag>,
    opened_at: u64,
}

/// Ease-out cubic over 0..=1.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
}

impl Switcher {
    pub fn new(now: u64) -> Self {
        Self { scroll: 0, drag: None, opened_at: now }
    }

    /// Still animating its opening.
    pub fn animating(&self, now: u64) -> bool {
        now.saturating_sub(self.opened_at) < OPEN_ANIM_MS
    }

    /// Size of the snapshot area of a card for a screen body `area`.
    fn thumb_size(area: Rect) -> (i32, i32) {
        (area.w / super::thumb::SCALE, area.h / super::thumb::SCALE)
    }

    pub fn show(&mut self, ui: &mut Ui, area: Rect, cards: &[Card], now: u64) -> Option<Action> {
        let t = *ui.theme;
        // A flat fill: a gradient this dark bands visibly in RGB565.
        ui.canvas.fill_rect(area, Color::hex(0x0d1119));
        let (head, rest) = area.split_top(HEADER_H);
        let title_f = ui.fonts.title;
        ui.text(Rect::new(head.x + 20, head.y, head.w / 2, head.h), "Recent apps", &title_f, t.text, Align::Left);
        let mut action = None;
        if !cards.is_empty() {
            let close_all = Rect::new(head.right() - 150, head.y + 12, 134, head.h - 24);
            if ui.button_colored(close_all, "Close all", t.surface_alt, t.text) {
                action = Some(Action::CloseAll);
            }
        }

        // The snapshot is taken from the app body, which is a little
        // shorter than this area; scale cards so they fit either way.
        let (tw, th) = Self::thumb_size(Rect::new(0, 0, area.w, area.h - CARD_TITLE_H));
        let card_w = tw + 16;
        let card_h = CARD_TITLE_H + th + 8;
        let anim = 1.0 - ease_out(now.saturating_sub(self.opened_at) as f32 / OPEN_ANIM_MS as f32);
        let top = rest.y + (rest.h - card_h) / 2 - 10 + (anim * 160.0) as i32;
        let content_w = cards.len() as i32 * (card_w + GAP) - GAP;
        let max_scroll = (content_w - area.w + 2 * GAP).max(0);
        // Center when everything fits.
        let left = if content_w + 2 * GAP <= area.w { area.x + (area.w - content_w) / 2 } else { area.x + GAP };

        let input = ui.input;
        let card_rect = |i: usize, scroll: i32| Rect::new(left + i as i32 * (card_w + GAP) - scroll, top, card_w, card_h);

        // Input: start, steer and finish a drag.
        if input.pressed && rest.contains(input.pos) && action.is_none() {
            let card = (0..cards.len()).find(|&i| card_rect(i, self.scroll).contains(input.pos));
            self.drag = Some(Drag { card, start: input.pos, start_scroll: self.scroll, axis: None });
        }
        let mut lift = (None, 0);
        if let Some(d) = self.drag.as_mut() {
            let (dx, dy) = (input.pos.x - d.start.x, input.pos.y - d.start.y);
            if d.axis.is_none() && (dx.abs() > SLOP || dy.abs() > SLOP) {
                d.axis = Some(if dx.abs() > dy.abs() { Axis::Horizontal } else { Axis::Vertical });
            }
            match d.axis {
                Some(Axis::Horizontal) => self.scroll = (d.start_scroll - dx).clamp(0, max_scroll),
                Some(Axis::Vertical) => lift = (d.card, dy.min(0)),
                None => {}
            }
            if input.released || !input.down {
                let d = *d;
                self.drag = None;
                lift = (None, 0);
                action = action.or(match (d.axis, d.card) {
                    (None, Some(i)) => Some(Action::Open(cards[i].tag)),
                    (None, None) => Some(Action::Dismiss),
                    (Some(Axis::Vertical), Some(i)) if dy <= -CLOSE_DIST => Some(Action::Close(cards[i].tag)),
                    _ => None,
                });
            }
        }

        let small = ui.fonts.small;
        let body_f = ui.fonts.body;
        let old = ui.canvas.set_clip(rest.intersect(&ui.canvas.clip()));
        for (i, c) in cards.iter().enumerate() {
            let mut r = card_rect(i, self.scroll);
            if lift.0 == Some(i) {
                r.y += lift.1;
            }
            if r.right() < area.x || r.x > area.right() {
                continue;
            }
            // Fade a card being swiped away.
            let fade = if lift.0 == Some(i) { ((-lift.1) * 160 / CLOSE_DIST).min(160) as u8 } else { 0 };
            ui.canvas.fill_round_rect(r, 14, t.surface.blend(Color::BLACK, 255 - fade));
            let icon = Rect::new(r.x + 10, r.y + 8, 24, 24);
            (c.icon)(ui.canvas, icon);
            ui.text(Rect::new(icon.right() + 8, r.y, r.w - 46, CARD_TITLE_H), c.name, &body_f, t.text, Align::Left);
            let shot = Rect::new(r.x + 8, r.y + CARD_TITLE_H, tw, th);
            match c.thumb {
                Some(img) => ui.canvas.blit_scaled(shot, &img.pixels, img.w, img.h),
                None => {
                    ui.canvas.fill_rect(shot, t.bg);
                    let s = tw.min(th) / 2;
                    (c.icon)(ui.canvas, Rect::new(shot.x + (tw - s) / 2, shot.y + (th - s) / 2, s, s));
                }
            }
            if fade > 0 {
                ui.canvas.fill_rect_alpha(shot, Color::BLACK, fade);
            }
            let label = match c.state {
                AppState::Background => "running in background",
                AppState::Suspended => "paused",
                _ => "",
            };
            if !label.is_empty() {
                let tag = Rect::new(r.x, r.bottom() + 6, r.w, 22);
                ui.text(tag, label, &small, t.text_dim, Align::Center);
            }
        }
        ui.canvas.restore_clip(old);

        if cards.is_empty() {
            let f = ui.fonts.large;
            ui.text(rest, "No recent apps", &f, t.text_dim, Align::Center);
        } else {
            let hint = "Tap to switch, swipe up to close";
            ui.text(Rect::new(area.x, area.bottom() - 48, area.w, 28), hint, &small, t.text_dim, Align::Center);
        }
        action
    }
}
