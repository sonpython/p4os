//! Horizontal paging of the home screen: the pages follow the finger and
//! snap to the nearest page on release, or to the next one in the swipe's
//! direction when it was fast or went past the halfway point.
//!
//! A touch only becomes a page swipe once it has moved more than the tap
//! slop, mostly sideways ([`gesture::Config::slop`], the rule the edge
//! gesture recogniser uses), so icon taps still work. Touches that start
//! in an edge zone never get here: the recogniser turns them into Back /
//! Forward, which the desktop maps to the previous / next page.
//!
//! Pure state machine over (position, down, time) samples; host-tested.

use p4os_sdk::Point;

/// Inward speed (px/ms) at release that flips the page.
const FLICK_SPEED: f32 = 0.5;
/// Minimum travel for a flick to count.
const FLICK_MIN: i32 = 30;
const SETTLE_MS: u64 = 220;
/// Past the first and last page the pages move this much slower.
const RUBBER: i32 = 3;
const VELOCITY_WINDOW_MS: u64 = 80;
const HISTORY: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub pos: Point,
    pub down: bool,
    pub t_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    start: Point,
    axis: Option<Axis>,
    history: [(i32, u64); HISTORY],
    len: usize,
}

impl Drag {
    fn push(&mut self, x: i32, t: u64) {
        if self.len == HISTORY {
            self.history.copy_within(1.., 0);
            self.len -= 1;
        }
        self.history[self.len] = (x, t);
        self.len += 1;
    }

    /// Horizontal speed in px/ms (positive = finger moving right).
    fn speed(&self) -> f32 {
        let h = &self.history[..self.len];
        let Some(&(x1, t1)) = h.last() else { return 0.0 };
        match h.iter().find(|(_, t)| t1.saturating_sub(*t) <= VELOCITY_WINDOW_MS) {
            Some(&(x0, t0)) if t1 > t0 => (x1 - x0) as f32 / (t1 - t0) as f32,
            _ => 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Settle {
    /// Offset from the target page when the settling started.
    from: i32,
    start: u64,
}

#[derive(Debug, Default)]
pub struct Pager {
    page: usize,
    drag: Option<Drag>,
    /// Offset from the current page while dragging (positive = towards
    /// the next page).
    offset: i32,
    settle: Option<Settle>,
}

fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
}

impl Pager {
    pub fn page(&self) -> usize {
        self.page
    }

    /// True while the touch is a page swipe: it must not click icons.
    pub fn swiping(&self) -> bool {
        self.drag.is_some_and(|d| d.axis == Some(Axis::Horizontal))
    }

    /// Pages are moving (dragged or settling): keep frames coming.
    pub fn animating(&self, now: u64) -> bool {
        self.swiping() || self.settle.is_some_and(|s| now.saturating_sub(s.start) < SETTLE_MS)
    }

    /// Go to page `p` (clamped), sliding from where the pages are now.
    pub fn go_to(&mut self, p: usize, pages: usize, now: u64, width: i32) {
        let target = p.min(pages.saturating_sub(1));
        let current = self.scroll(now, width);
        self.page = target;
        self.drag = None;
        self.offset = 0;
        self.settle = Some(Settle { from: current - target as i32 * width, start: now });
    }

    /// Keep the page valid when the number of pages changes.
    pub fn clamp(&mut self, pages: usize) {
        if self.page >= pages {
            self.page = pages.saturating_sub(1);
            self.offset = 0;
            self.settle = None;
        }
    }

    /// Horizontal scroll position in pixels (page * width + offset) at `now`.
    pub fn scroll(&self, now: u64, width: i32) -> i32 {
        let base = self.page as i32 * width;
        if self.drag.is_some() {
            return base + self.offset;
        }
        match self.settle {
            Some(s) => {
                let t = now.saturating_sub(s.start) as f32 / SETTLE_MS as f32;
                base + (s.from as f32 * (1.0 - ease_out(t))) as i32
            }
            None => base,
        }
    }

    /// Feed this frame's touch. `slop` is the tap-vs-drag distance.
    /// Returns the scroll position to draw.
    pub fn update(&mut self, s: Sample, width: i32, pages: usize, slop: i32) -> i32 {
        self.clamp(pages);
        match (self.drag.as_mut(), s.down) {
            (None, true) => {
                // A touch during settling catches the pages where they are.
                let current = self.scroll(s.t_ms, width);
                self.settle = None;
                self.offset = current - self.page as i32 * width;
                let mut d = Drag { start: Point::new(s.pos.x + self.offset, s.pos.y), axis: None, history: [(0, 0); HISTORY], len: 0 };
                d.push(s.pos.x, s.t_ms);
                if self.offset != 0 {
                    d.axis = Some(Axis::Horizontal);
                }
                self.drag = Some(d);
            }
            (Some(d), true) => {
                d.push(s.pos.x, s.t_ms);
                let (dx, dy) = (s.pos.x - d.start.x, s.pos.y - d.start.y);
                if d.axis.is_none() && (dx.abs() > slop || dy.abs() > slop) {
                    d.axis = Some(if dx.abs() > dy.abs() { Axis::Horizontal } else { Axis::Vertical });
                }
                if d.axis == Some(Axis::Horizontal) {
                    let max = (pages.saturating_sub(1)) as i32 * width;
                    let raw = self.page as i32 * width - dx;
                    // Rubber band beyond the ends.
                    let clamped = if raw < 0 { raw / RUBBER } else if raw > max { max + (raw - max) / RUBBER } else { raw };
                    self.offset = clamped - self.page as i32 * width;
                }
            }
            (Some(d), false) => {
                let d = *d;
                self.drag = None;
                if d.axis == Some(Axis::Horizontal) {
                    let v = d.speed();
                    let dx = -self.offset;
                    let target = if v.abs() >= FLICK_SPEED && dx.abs() >= FLICK_MIN {
                        if v < 0.0 { self.page + 1 } else { self.page.saturating_sub(1) }
                    } else if dx <= -width / 2 {
                        self.page + 1
                    } else if dx >= width / 2 {
                        self.page.saturating_sub(1)
                    } else {
                        self.page
                    };
                    let current = self.page as i32 * width + self.offset;
                    self.page = target.min(pages.saturating_sub(1));
                    self.offset = 0;
                    self.settle = Some(Settle { from: current - self.page as i32 * width, start: s.t_ms });
                }
                self.offset = 0;
            }
            (None, false) => {}
        }
        self.scroll(s.t_ms, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i32 = 480;
    const SLOP: i32 = 12;

    struct Run {
        p: Pager,
        t: u64,
        pages: usize,
    }

    impl Run {
        fn new(pages: usize) -> Self {
            Self { p: Pager::default(), t: 1000, pages }
        }

        fn at(&mut self, x: i32, y: i32, down: bool, dt: u64) -> i32 {
            self.t += dt;
            self.p.update(Sample { pos: Point::new(x, y), down, t_ms: self.t }, W, self.pages, SLOP)
        }

        fn swipe(&mut self, from: i32, to: i32, steps: i32, dt: u64) -> i32 {
            self.at(from, 400, true, 16);
            for i in 1..=steps {
                self.at(from + (to - from) * i / steps, 400, true, dt);
            }
            self.at(to, 400, false, dt)
        }

        fn settle(&mut self) -> i32 {
            self.t += 1000;
            self.p.scroll(self.t, W)
        }
    }

    #[test]
    fn pages_follow_the_finger() {
        let mut r = Run::new(3);
        r.at(400, 400, true, 16);
        assert_eq!(r.at(390, 400, true, 16), 0, "inside the slop nothing moves");
        assert!(!r.p.swiping());
        assert_eq!(r.at(300, 400, true, 16), 100);
        assert!(r.p.swiping());
        assert_eq!(r.at(200, 400, true, 16), 200);
    }

    #[test]
    fn slow_drag_past_half_flips_short_one_snaps_back() {
        let mut r = Run::new(3);
        r.swipe(400, 120, 20, 50);
        assert_eq!(r.p.page(), 1);
        assert_eq!(r.settle(), W);
        r.swipe(400, 300, 20, 50);
        assert_eq!(r.p.page(), 1, "less than half and slow: stays");
        assert_eq!(r.settle(), W);
    }

    #[test]
    fn fast_flick_flips_even_when_short() {
        let mut r = Run::new(3);
        r.swipe(300, 220, 4, 10);
        assert_eq!(r.p.page(), 1);
        r.swipe(200, 280, 4, 10);
        assert_eq!(r.p.page(), 0);
    }

    #[test]
    fn ends_rubber_band_and_never_overflow() {
        let mut r = Run::new(2);
        r.at(100, 400, true, 16);
        // Pulling right on the first page moves a third as far.
        assert_eq!(r.at(400, 400, true, 16), -100);
        r.at(400, 400, false, 16);
        assert_eq!(r.p.page(), 0);
        assert_eq!(r.settle(), 0);
        r.swipe(400, 50, 4, 10);
        assert_eq!(r.p.page(), 1);
        r.swipe(400, 50, 4, 10);
        assert_eq!(r.p.page(), 1, "no page after the last");
    }

    #[test]
    fn settling_animates_from_release_to_target() {
        let mut r = Run::new(2);
        r.swipe(400, 100, 10, 40);
        assert_eq!(r.p.page(), 1);
        let t0 = r.t;
        let mid = r.p.scroll(t0 + 60, W);
        assert!(mid > 300 && mid < W, "{mid}");
        assert!(r.p.animating(t0 + 60));
        assert_eq!(r.p.scroll(t0 + 500, W), W);
        assert!(!r.p.animating(t0 + 500));
    }

    #[test]
    fn taps_and_vertical_drags_do_not_page() {
        let mut r = Run::new(3);
        assert_eq!(r.swipe(200, 205, 2, 16), 0);
        assert_eq!(r.p.page(), 0);
        r.at(200, 300, true, 16);
        r.at(210, 450, true, 16);
        assert!(!r.p.swiping());
        assert_eq!(r.at(300, 450, true, 16), 0, "a vertical drag stays vertical");
        r.at(300, 450, false, 16);
        assert_eq!(r.p.page(), 0);
    }

    #[test]
    fn go_to_and_clamp() {
        let mut r = Run::new(3);
        r.p.go_to(2, 3, r.t, W);
        assert_eq!(r.p.page(), 2);
        assert_eq!(r.settle(), 2 * W);
        r.p.go_to(9, 3, r.t, W);
        assert_eq!(r.p.page(), 2);
        // Fewer pages after rotating: stay on the last valid one.
        r.p.clamp(2);
        assert_eq!(r.p.page(), 1);
        assert_eq!(r.p.scroll(r.t, W), W);
    }

    #[test]
    fn a_touch_while_settling_catches_the_pages() {
        let mut r = Run::new(3);
        r.swipe(400, 100, 10, 40);
        let t0 = r.t;
        r.t = t0 + 50;
        let caught = r.p.scroll(r.t, W);
        let s = r.at(240, 400, true, 0);
        assert_eq!(s, caught);
        assert!(r.p.swiping());
    }
}
