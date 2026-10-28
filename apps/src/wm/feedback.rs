//! Visual feedback for system gestures: the home indicator, the pill that
//! follows a bottom swipe, the edge arrow of Back/Forward, and the
//! app-to-home transition. Everything here draws a few small shapes so a
//! gesture frame costs little more than the app's own frame.

use super::gesture::{Config, Edge, Gesture, Track};
use super::switcher::ease_out;
use super::thumb::Thumbnail;
use p4os_sdk::{Canvas, Color, Point, Rect};

const PILL_W: i32 = 134;
const PILL_H: i32 = 6;

/// The resting home indicator, centred in the bottom strip.
pub fn home_indicator(c: &mut Canvas, strip: Rect, bg: Color, fg: Color) {
    c.fill_rect(strip, bg);
    let pill = Rect::new(strip.x + (strip.w - PILL_W) / 2, strip.y + (strip.h - PILL_H) / 2, PILL_W, PILL_H);
    c.fill_round_rect(pill, PILL_H / 2, fg);
}

/// Feedback while a gesture is tracked. `screen` is the whole logical
/// screen, `accent` marks an armed gesture.
pub fn track(c: &mut Canvas, screen: Rect, t: &Track, cfg: &Config, accent: Color) {
    match t.edge {
        Edge::Bottom => bottom(c, screen, t, accent),
        Edge::Left | Edge::Right => side(c, screen, t, cfg, accent),
    }
}

fn bottom(c: &mut Canvas, screen: Rect, t: &Track, accent: Color) {
    let y = t.pos.y.clamp(screen.y + 60, screen.bottom() - PILL_H - 9);
    let w = PILL_W + (t.travel / 2).min(90);
    let pill = Rect::new(screen.x + (screen.w - w) / 2, y, w, PILL_H + 4);
    let colour = match t.armed {
        Some(_) => accent,
        None => Color::WHITE,
    };
    // A dark outline keeps the pill visible on light content.
    c.fill_round_rect(Rect::new(pill.x - 2, pill.y - 2, pill.w + 4, pill.h + 4), (pill.h + 4) / 2, Color::hex(0x10131a));
    c.fill_round_rect(pill, pill.h / 2, colour);
    match t.armed {
        // Stacked cards hint at the switcher.
        Some(Gesture::Switcher) => {
            for (i, dw) in [(0, 60), (1, 40), (2, 20)] {
                let cw = 90 - dw;
                let r = Rect::new(screen.x + (screen.w - cw) / 2 + (i - 1) * 34, y - 64 + i * 6, cw / 2 + 20, 44);
                c.fill_round_rect(Rect::new(r.x - 2, r.y - 2, r.w + 4, r.h + 4), 8, Color::hex(0x10131a));
                c.fill_round_rect(r, 7, accent.blend(Color::hex(0x283041), 120 + i as u8 * 50));
            }
        }
        // An arrow pointing home.
        _ => {
            let cx = screen.x + screen.w / 2;
            let ay = y - 18;
            let col = if t.armed.is_some() { accent } else { Color::WHITE };
            c.fill_circle(Point::new(cx, ay - 4), 17.0, Color::hex(0x10131a));
            c.line(Point::new(cx - 8, ay), Point::new(cx, ay - 9), 3.0, col);
            c.line(Point::new(cx, ay - 9), Point::new(cx + 8, ay), 3.0, col);
        }
    }
}

fn side(c: &mut Canvas, screen: Rect, t: &Track, cfg: &Config, accent: Color) {
    let p = t.progress(cfg);
    let r = 16.0 + 30.0 * p;
    let left = t.edge == Edge::Left;
    // The bubble slides in from the edge with the finger.
    let inset = (r * 0.9 * p) as i32;
    let cx = if left { screen.x + inset } else { screen.right() - 1 - inset };
    let cy = t.pos.y.clamp(screen.y + 50, screen.bottom() - 50);
    let armed = t.armed.is_some();
    c.fill_circle(Point::new(cx, cy), r + 2.0, Color::hex(0x0b0e14));
    c.fill_circle(Point::new(cx, cy), r, if armed { accent } else { Color::hex(0x2a3244) });
    // Chevron pointing the way the page will go: "<" for Back.
    let s = 4.0 + 6.0 * p;
    let tip = (s * 0.6) as i32;
    let ax = if left { cx + (r * 0.35) as i32 } else { cx - (r * 0.35) as i32 };
    let dir = if left { -1 } else { 1 };
    let (a, b, m) = (
        Point::new(ax - dir * tip, cy - s as i32),
        Point::new(ax - dir * tip, cy + s as i32),
        Point::new(ax + dir * tip, cy),
    );
    c.line(a, m, 3.0, Color::WHITE);
    c.line(m, b, 3.0, Color::WHITE);
}

/// The app's snapshot shrinking from `from` (its full area) into `to`
/// (its launcher icon) as `t` goes 0..1, fading towards the end.
pub fn shrink_to(c: &mut Canvas, thumb: &Thumbnail, from: Rect, to: Rect, t: f32) {
    let e = ease_out(t);
    let lerp = |a: i32, b: i32| a + ((b - a) as f32 * e) as i32;
    let r = Rect::new(lerp(from.x, to.x), lerp(from.y, to.y), lerp(from.w, to.w), lerp(from.h, to.h));
    if r.is_empty() {
        return;
    }
    c.blit_scaled(r, &thumb.pixels, thumb.w, thumb.h);
    let fade = (e * 200.0) as u8;
    if fade > 0 {
        c.fill_rect_alpha(r, Color::hex(0x0b0f18), fade);
    }
}
