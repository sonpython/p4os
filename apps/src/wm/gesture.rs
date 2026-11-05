//! System edge gestures, recognised before input reaches an app.
//!
//! A touch that starts inside an edge zone is held back from the app until
//! it is clear what it is:
//!
//! * moving inwards from the edge (up from the bottom, right from the left
//!   edge, left from the right edge) makes it a system gesture;
//! * lifting without moving, staying put for a while, or moving along the
//!   edge hands it to the app, replayed from where it started, so taps and
//!   scrolls near the edges still work;
//! * a second finger cancels it.
//!
//! Touches that start elsewhere are never looked at again until released,
//! so in-app scrolling is unaffected.
//!
//! The recogniser is a pure state machine over (position, finger count,
//! time) samples in logical screen coordinates, so it works in either
//! orientation and is tested on the host.

use p4os_sdk::Point;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Width of the edge zones.
    pub edge: i32,
    /// Movement that turns a touch into a drag.
    pub slop: i32,
    /// Inward travel that commits a gesture on release.
    pub commit: i32,
    /// Inward speed (px/ms) at release that commits a shorter gesture.
    pub flick_speed: f32,
    /// Minimum travel for a flick to count.
    pub flick_min: i32,
    /// A bottom swipe that rests this long opens the app switcher.
    pub hold_ms: u64,
    /// Movement still counted as resting.
    pub hold_still: i32,
    /// Travel needed before resting counts as a hold.
    pub hold_min_travel: i32,
    /// A touch resting in an edge zone this long goes to the app.
    pub edge_press_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            edge: 24,
            slop: 12,
            commit: 90,
            flick_speed: 0.6,
            flick_min: 36,
            hold_ms: 300,
            hold_still: 14,
            hold_min_travel: 70,
            edge_press_ms: 450,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Bottom,
    Left,
    Right,
}

impl Edge {
    /// Unit vector pointing away from the edge into the screen.
    fn inward(self) -> (i32, i32) {
        match self {
            Edge::Bottom => (0, -1),
            Edge::Left => (1, 0),
            Edge::Right => (-1, 0),
        }
    }

    /// Distance moved from `a` to `b` in the inward direction.
    fn travel(self, a: Point, b: Point) -> i32 {
        let (dx, dy) = self.inward();
        (b.x - a.x) * dx + (b.y - a.y) * dy
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gesture {
    Home,
    Switcher,
    Back,
    Forward,
}

/// One input sample.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub pos: Point,
    pub down: bool,
    /// Fingers on the panel (0 = unknown, treated as one).
    pub fingers: u8,
    pub t_ms: u64,
}

/// A gesture being tracked, for drawing feedback.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Track {
    pub edge: Edge,
    pub start: Point,
    pub pos: Point,
    /// Inward travel so far (never negative).
    pub travel: i32,
    /// What lifting the finger now would do, if anything.
    pub armed: Option<Gesture>,
}

impl Track {
    /// Travel relative to the commit distance, 0..=1.
    pub fn progress(&self, cfg: &Config) -> f32 {
        (self.travel as f32 / cfg.commit as f32).clamp(0.0, 1.0)
    }
}

/// What to do with this frame's input.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Verdict {
    /// Not a system gesture: give the input to the app unchanged.
    Pass,
    /// Touch in an edge zone, not decided yet: hold the input back.
    Pending,
    /// The held-back touch belongs to the app after all: deliver a press at
    /// `at` (and a release in the same frame if `released`), then pass.
    Replay { at: Point, released: bool },
    /// A gesture is being tracked: swallow the input, draw feedback.
    Tracking(Track),
    /// A gesture completed.
    Fire(Gesture),
    /// A gesture was abandoned (or a multi-finger touch is ignored):
    /// swallow the input.
    Cancelled,
}

#[derive(Clone, Copy, Debug)]
struct Edges {
    bottom: bool,
    left: bool,
    right: bool,
}

impl Edges {
    fn any(&self) -> bool {
        self.bottom || self.left || self.right
    }

    fn has(&self, e: Edge) -> bool {
        match e {
            Edge::Bottom => self.bottom,
            Edge::Left => self.left,
            Edge::Right => self.right,
        }
    }
}

const HISTORY: usize = 8;
/// Window used for the release velocity.
const VELOCITY_WINDOW_MS: u64 = 80;

#[derive(Clone, Copy, Debug)]
struct Tracking {
    edge: Edge,
    start: Point,
    /// Where and when the finger last started resting.
    anchor: (Point, u64),
    paused: bool,
    history: [(Point, u64); HISTORY],
    len: usize,
}

impl Tracking {
    fn push(&mut self, p: Point, t: u64) {
        if self.len == HISTORY {
            self.history.copy_within(1.., 0);
            self.len -= 1;
        }
        self.history[self.len] = (p, t);
        self.len += 1;
    }

    /// Inward speed in px/ms over the last few samples.
    fn speed(&self) -> f32 {
        let Some(&(last, t1)) = self.history[..self.len].last() else { return 0.0 };
        let first = self.history[..self.len].iter().find(|(_, t)| t1.saturating_sub(*t) <= VELOCITY_WINDOW_MS);
        match first {
            Some(&(p0, t0)) if t1 > t0 => self.edge.travel(p0, last) as f32 / (t1 - t0) as f32,
            _ => 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum State {
    Idle,
    /// The touch belongs to the app until it is released.
    Passing,
    Candidate { edges: Edges, start: Point, t0: u64 },
    Tracking(Tracking),
    /// Multi-finger touch: swallow until every finger is up.
    Ignored,
}

pub struct Recognizer {
    pub cfg: Config,
    size: (i32, i32),
    state: State,
}

impl Recognizer {
    pub fn new(cfg: Config) -> Self {
        Self { cfg, size: (0, 0), state: State::Idle }
    }

    /// Forget any touch in progress (used when gestures are disabled).
    pub fn reset(&mut self) {
        self.state = State::Idle;
    }

    /// True while a touch is being held back or tracked.
    pub fn busy(&self) -> bool {
        matches!(self.state, State::Candidate { .. } | State::Tracking(_) | State::Ignored)
    }

    fn edges_at(&self, p: Point) -> Edges {
        let (w, h) = self.size;
        let e = self.cfg.edge;
        Edges { bottom: p.y >= h - e, left: p.x < e, right: p.x >= w - e }
    }

    /// Feed the sample for this frame; `screen` is the logical screen size.
    pub fn update(&mut self, s: Sample, screen: (i32, i32)) -> Verdict {
        self.size = screen;
        let multi = s.fingers > 1;
        match self.state {
            State::Idle => {
                if !s.down {
                    return Verdict::Pass;
                }
                let edges = self.edges_at(s.pos);
                if edges.any() && !multi {
                    self.state = State::Candidate { edges, start: s.pos, t0: s.t_ms };
                    Verdict::Pending
                } else {
                    self.state = State::Passing;
                    Verdict::Pass
                }
            }
            State::Passing => {
                if !s.down {
                    self.state = State::Idle;
                }
                Verdict::Pass
            }
            State::Ignored => {
                if !s.down {
                    self.state = State::Idle;
                }
                Verdict::Cancelled
            }
            State::Candidate { edges, start, t0 } => {
                if multi {
                    self.state = if s.down { State::Ignored } else { State::Idle };
                    return Verdict::Cancelled;
                }
                if !s.down {
                    self.state = State::Idle;
                    return Verdict::Replay { at: start, released: true };
                }
                let (dx, dy) = (s.pos.x - start.x, s.pos.y - start.y);
                if dx.abs() > self.cfg.slop || dy.abs() > self.cfg.slop {
                    // The dominant direction decides; in a corner it picks
                    // between the two edges.
                    let edge = if dy.abs() >= dx.abs() {
                        (dy < 0 && edges.has(Edge::Bottom)).then_some(Edge::Bottom)
                    } else if dx > 0 {
                        edges.has(Edge::Left).then_some(Edge::Left)
                    } else {
                        edges.has(Edge::Right).then_some(Edge::Right)
                    };
                    return match edge {
                        Some(edge) => {
                            let mut tr = Tracking {
                                edge,
                                start,
                                anchor: (s.pos, s.t_ms),
                                paused: false,
                                history: [(start, t0); HISTORY],
                                len: 1,
                            };
                            tr.push(s.pos, s.t_ms);
                            self.state = State::Tracking(tr);
                            Verdict::Tracking(self.track(&tr, s.pos))
                        }
                        None => {
                            self.state = State::Passing;
                            Verdict::Replay { at: start, released: false }
                        }
                    };
                }
                if s.t_ms.saturating_sub(t0) >= self.cfg.edge_press_ms {
                    self.state = State::Passing;
                    return Verdict::Replay { at: start, released: false };
                }
                Verdict::Pending
            }
            State::Tracking(mut tr) => {
                if multi {
                    self.state = if s.down { State::Ignored } else { State::Idle };
                    return Verdict::Cancelled;
                }
                if s.down {
                    tr.push(s.pos, s.t_ms);
                    self.update_hold(&mut tr, s.pos, s.t_ms);
                    self.state = State::Tracking(tr);
                    return Verdict::Tracking(self.track(&tr, s.pos));
                }
                self.state = State::Idle;
                // The release sample repeats the last position; the speed
                // is measured up to the last movement.
                match self.decide(&tr, s.pos) {
                    Some(g) => Verdict::Fire(g),
                    None => Verdict::Cancelled,
                }
            }
        }
    }

    fn update_hold(&self, tr: &mut Tracking, pos: Point, t: u64) {
        if tr.edge != Edge::Bottom {
            return;
        }
        let (a, ta) = tr.anchor;
        if (pos.x - a.x).abs() > self.cfg.hold_still || (pos.y - a.y).abs() > self.cfg.hold_still {
            tr.anchor = (pos, t);
            // Moving on after a pause: a fresh flick may still mean Home,
            // which `decide` checks with the release speed.
            return;
        }
        if tr.edge.travel(tr.start, pos) >= self.cfg.hold_min_travel && t.saturating_sub(ta) >= self.cfg.hold_ms {
            tr.paused = true;
        }
    }

    fn decide(&self, tr: &Tracking, pos: Point) -> Option<Gesture> {
        let travel = tr.edge.travel(tr.start, pos);
        let speed = tr.speed();
        let flicked = speed >= self.cfg.flick_speed && travel >= self.cfg.flick_min;
        let committed = travel >= self.cfg.commit || flicked;
        match tr.edge {
            Edge::Bottom if tr.paused && !flicked && travel >= self.cfg.hold_min_travel => Some(Gesture::Switcher),
            Edge::Bottom => committed.then_some(Gesture::Home),
            Edge::Left => committed.then_some(Gesture::Back),
            Edge::Right => committed.then_some(Gesture::Forward),
        }
    }

    fn track(&self, tr: &Tracking, pos: Point) -> Track {
        let travel = tr.edge.travel(tr.start, pos).max(0);
        let armed = match tr.edge {
            Edge::Bottom if tr.paused && travel >= self.cfg.hold_min_travel => Some(Gesture::Switcher),
            _ if travel >= self.cfg.commit => self.decide(tr, pos),
            _ => None,
        };
        Track { edge: tr.edge, start: tr.start, pos, travel, armed }
    }
}

/// Position of a scripted swipe `elapsed` ms after it started: a straight
/// line from `from` to `to` over `duration` ms, then resting at `to` for
/// `hold` ms. `None` once the finger has lifted. Used by the shell `swipe`
/// command and the simulator.
pub fn swipe_point(from: Point, to: Point, duration: u64, hold: u64, elapsed: u64) -> Option<Point> {
    if elapsed > duration + hold {
        return None;
    }
    if elapsed >= duration || duration == 0 {
        return Some(to);
    }
    let f = |a: i32, b: i32| a + ((b - a) as i64 * elapsed as i64 / duration as i64) as i32;
    Some(Point::new(f(from.x, to.x), f(from.y, to.y)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTRAIT: (i32, i32) = (480, 800);
    const LANDSCAPE: (i32, i32) = (800, 480);

    struct Run {
        r: Recognizer,
        t: u64,
        screen: (i32, i32),
    }

    impl Run {
        fn new(screen: (i32, i32)) -> Self {
            Self { r: Recognizer::new(Config::default()), t: 1000, screen }
        }

        fn at(&mut self, x: i32, y: i32, down: bool, fingers: u8, dt: u64) -> Verdict {
            self.t += dt;
            self.r.update(Sample { pos: Point::new(x, y), down, fingers, t_ms: self.t }, self.screen)
        }

        fn down(&mut self, x: i32, y: i32) -> Verdict {
            self.at(x, y, true, 1, 16)
        }

        fn up(&mut self, x: i32, y: i32) -> Verdict {
            self.at(x, y, false, 0, 16)
        }

        /// Move in `steps` frames of `dt` ms from (x0,y0) to (x1,y1) and
        /// return the last verdict.
        fn slide(&mut self, from: (i32, i32), to: (i32, i32), steps: i32, dt: u64) -> Verdict {
            let mut v = Verdict::Pass;
            for i in 1..=steps {
                let x = from.0 + (to.0 - from.0) * i / steps;
                let y = from.1 + (to.1 - from.1) * i / steps;
                v = self.at(x, y, true, 1, dt);
            }
            v
        }
    }

    #[test]
    fn quick_flick_from_bottom_goes_home() {
        let mut r = Run::new(PORTRAIT);
        assert_eq!(r.down(240, 790), Verdict::Pending);
        assert!(matches!(r.slide((240, 790), (240, 640), 6, 16), Verdict::Tracking(_)));
        assert_eq!(r.up(240, 640), Verdict::Fire(Gesture::Home));
    }

    #[test]
    fn short_fast_flick_still_counts() {
        let mut r = Run::new(PORTRAIT);
        r.down(240, 795);
        r.slide((240, 795), (240, 745), 3, 10);
        assert_eq!(r.up(240, 745), Verdict::Fire(Gesture::Home));
    }

    #[test]
    fn short_slow_swipe_is_cancelled() {
        let mut r = Run::new(PORTRAIT);
        r.down(240, 795);
        r.slide((240, 795), (240, 750), 10, 40);
        assert_eq!(r.up(240, 750), Verdict::Cancelled);
    }

    #[test]
    fn swipe_up_and_hold_opens_switcher() {
        let mut r = Run::new(PORTRAIT);
        r.down(240, 790);
        r.slide((240, 790), (240, 620), 8, 16);
        // Rest for 320 ms with a little jitter.
        let mut last = Verdict::Pass;
        for i in 0..20 {
            last = r.at(240 + (i % 3), 620, true, 1, 16);
        }
        match last {
            Verdict::Tracking(t) => assert_eq!(t.armed, Some(Gesture::Switcher)),
            v => panic!("{v:?}"),
        }
        assert_eq!(r.up(240, 620), Verdict::Fire(Gesture::Switcher));
    }

    #[test]
    fn hold_then_flick_goes_home() {
        let mut r = Run::new(PORTRAIT);
        r.down(240, 790);
        r.slide((240, 790), (240, 650), 8, 16);
        for _ in 0..22 {
            r.at(240, 650, true, 1, 16);
        }
        r.slide((240, 650), (240, 450), 4, 10);
        assert_eq!(r.up(240, 450), Verdict::Fire(Gesture::Home));
    }

    #[test]
    fn pause_too_close_to_the_edge_is_not_a_hold() {
        let mut r = Run::new(PORTRAIT);
        r.down(240, 790);
        r.slide((240, 790), (240, 760), 3, 30);
        for _ in 0..30 {
            r.at(240, 760, true, 1, 16);
        }
        assert_eq!(r.up(240, 760), Verdict::Cancelled);
    }

    #[test]
    fn back_and_forward_from_side_edges() {
        let mut r = Run::new(PORTRAIT);
        assert_eq!(r.down(5, 400), Verdict::Pending);
        r.slide((5, 400), (140, 410), 8, 16);
        assert_eq!(r.up(140, 410), Verdict::Fire(Gesture::Back));

        assert_eq!(r.down(475, 300), Verdict::Pending);
        r.slide((475, 300), (330, 300), 8, 16);
        assert_eq!(r.up(330, 300), Verdict::Fire(Gesture::Forward));
    }

    #[test]
    fn back_released_after_returning_is_cancelled() {
        let mut r = Run::new(PORTRAIT);
        r.down(3, 400);
        r.slide((3, 400), (150, 400), 8, 16);
        r.slide((150, 400), (20, 400), 8, 16);
        assert_eq!(r.up(20, 400), Verdict::Cancelled);
    }

    #[test]
    fn tap_in_edge_zone_is_replayed_to_the_app() {
        let mut r = Run::new(PORTRAIT);
        assert_eq!(r.down(10, 300), Verdict::Pending);
        assert_eq!(r.at(12, 302, true, 1, 16), Verdict::Pending);
        assert_eq!(r.up(12, 302), Verdict::Replay { at: Point::new(10, 300), released: true });
        // The next touch starts fresh.
        assert_eq!(r.down(240, 400), Verdict::Pass);
    }

    #[test]
    fn long_press_in_edge_zone_goes_to_the_app() {
        let mut r = Run::new(PORTRAIT);
        r.down(470, 300);
        let mut v = Verdict::Pending;
        for _ in 0..40 {
            v = r.at(470, 300, true, 1, 16);
            if v != Verdict::Pending {
                break;
            }
        }
        assert_eq!(v, Verdict::Replay { at: Point::new(470, 300), released: false });
        assert_eq!(r.at(400, 300, true, 1, 16), Verdict::Pass);
        assert_eq!(r.up(400, 300), Verdict::Pass);
    }

    #[test]
    fn vertical_scroll_starting_at_side_edge_goes_to_the_app() {
        let mut r = Run::new(PORTRAIT);
        r.down(8, 500);
        assert_eq!(r.at(9, 470, true, 1, 16), Verdict::Replay { at: Point::new(8, 500), released: false });
        assert_eq!(r.at(9, 400, true, 1, 16), Verdict::Pass);
        assert_eq!(r.up(9, 400), Verdict::Pass);
    }

    #[test]
    fn horizontal_move_along_bottom_edge_goes_to_the_app() {
        let mut r = Run::new(PORTRAIT);
        r.down(200, 790);
        assert!(matches!(r.at(240, 788, true, 1, 16), Verdict::Replay { released: false, .. }));
    }

    #[test]
    fn scroll_starting_in_the_middle_is_never_stolen() {
        let mut r = Run::new(PORTRAIT);
        assert_eq!(r.down(240, 400), Verdict::Pass);
        // Even ending deep in an edge zone.
        assert_eq!(r.slide((240, 400), (2, 799), 10, 16), Verdict::Pass);
        assert_eq!(r.up(2, 799), Verdict::Pass);
    }

    #[test]
    fn second_finger_cancels_and_is_ignored_until_lifted() {
        let mut r = Run::new(PORTRAIT);
        r.down(240, 790);
        r.slide((240, 790), (240, 700), 4, 16);
        assert_eq!(r.at(240, 690, true, 2, 16), Verdict::Cancelled);
        assert_eq!(r.at(240, 600, true, 1, 16), Verdict::Cancelled);
        assert_eq!(r.up(240, 600), Verdict::Cancelled);
        assert_eq!(r.down(240, 400), Verdict::Pass);
    }

    #[test]
    fn multi_finger_touch_never_starts_a_gesture() {
        let mut r = Run::new(PORTRAIT);
        assert_eq!(r.at(5, 400, true, 2, 16), Verdict::Pass);
        assert_eq!(r.slide((5, 400), (200, 400), 5, 16), Verdict::Pass);
    }

    #[test]
    fn corner_picks_edge_by_direction() {
        let mut r = Run::new(PORTRAIT);
        r.down(4, 796);
        match r.at(30, 795, true, 1, 16) {
            Verdict::Tracking(t) => assert_eq!(t.edge, Edge::Left),
            v => panic!("{v:?}"),
        }
        r.up(30, 795);
        r.down(4, 796);
        match r.at(5, 770, true, 1, 16) {
            Verdict::Tracking(t) => assert_eq!(t.edge, Edge::Bottom),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn landscape_uses_its_own_edges() {
        let mut r = Run::new(LANDSCAPE);
        // y = 470 is the bottom zone in landscape but not in portrait.
        assert_eq!(r.down(400, 470), Verdict::Pending);
        r.slide((400, 470), (400, 330), 6, 16);
        assert_eq!(r.up(400, 330), Verdict::Fire(Gesture::Home));
        // x = 790 is the right edge in landscape.
        assert_eq!(r.down(790, 200), Verdict::Pending);
        r.slide((790, 200), (640, 200), 6, 16);
        assert_eq!(r.up(640, 200), Verdict::Fire(Gesture::Forward));
        // y = 700 would be the bottom zone in portrait: here it is off-screen
        // anyway, and x = 460 is the middle, so a touch at (460, 300) passes.
        assert_eq!(r.down(460, 300), Verdict::Pass);
    }

    #[test]
    fn progress_and_arming_follow_travel() {
        let mut r = Run::new(PORTRAIT);
        r.down(2, 300);
        match r.at(47, 300, true, 1, 16) {
            Verdict::Tracking(t) => {
                assert_eq!(t.travel, 45);
                assert!((t.progress(&r.r.cfg) - 0.5).abs() < 0.01);
                assert_eq!(t.armed, None);
            }
            v => panic!("{v:?}"),
        }
        match r.at(120, 300, true, 1, 16) {
            Verdict::Tracking(t) => assert_eq!(t.armed, Some(Gesture::Back)),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn swipe_path() {
        let (a, b) = (Point::new(0, 100), Point::new(100, 0));
        assert_eq!(swipe_point(a, b, 100, 0, 0), Some(a));
        assert_eq!(swipe_point(a, b, 100, 0, 50), Some(Point::new(50, 50)));
        assert_eq!(swipe_point(a, b, 100, 300, 250), Some(b));
        assert_eq!(swipe_point(a, b, 100, 300, 401), None);
        assert_eq!(swipe_point(a, b, 0, 0, 0), Some(b));
    }
}
