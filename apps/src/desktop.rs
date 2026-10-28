//! The desktop shell: status bar, launcher grid, app chrome, the on-screen
//! keyboard, system gestures and the app manager. The kernel (or the
//! simulator) calls [`Desktop::frame`] for every input event or requested
//! tick and presents the canvas.
//!
//! Apps keep running when the user leaves them: the app manager tracks a
//! lifecycle state per app (`p4os_sdk::AppState`), runs the SDK callbacks
//! on every change, attributes heap and CPU time to each app, and enforces
//! the limits in [`policy::Limits`] (see `wm/`).

use crate::wm::gesture::{self, Gesture, Recognizer, Sample, Track, Verdict};
use crate::wm::launcher::{self, Catalogue, Grid};
use crate::wm::pager::{self, Pager};
use crate::wm::lifecycle::{self, Callback};
use crate::wm::policy::{self, AppRecord, CpuMeter, HangWatch, KillReason, Limits};
use crate::wm::switcher::{self, Card, Switcher};
use crate::wm::thumb::{self, Thumbnail};
use crate::wm::feedback;
use crate::lock::{self, Lock};
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use p4os_sdk::gui::font::builtin::{self, Fonts};
use p4os_sdk::gui::keyboard::{self, Keyboard};
use p4os_sdk::gui::ui::{Align, Input, Key, Theme, UiState};
use p4os_sdk::{
    App, AppHandle, AppInfo, AppState, AppUsage, Audio, Canvas, Capabilities, Color, DateTime, FileSystem, Image, Network,
    Orientation, Point, Rect, System, SystemStats, Ui,
};

pub const STATUS_H: i32 = 36;
pub const TITLE_H: i32 = 56;
/// Bottom strip holding the home indicator. It is also the bottom gesture
/// zone, so app content never sits under it.
pub const HOME_BAR_H: i32 = 24;
const STATUS_TICK_MS: u32 = 1000;
/// Frame interval while a gesture or an animation is running.
const ANIM_FRAME_MS: u32 = 16;
const HOME_ANIM_MS: u64 = 220;
const NOTICE_MS: u64 = 3500;
/// How often memory, CPU budgets and usage figures are re-evaluated.
const CHECK_MS: u64 = 1000;
/// Heap owner tags 1..=MAX_TAG identify running apps (0 is the system).
const MAX_TAG: u8 = 15;

/// One running built-in app.
struct Running {
    info: &'static AppInfo,
    app: Box<dyn App>,
    state: AppState,
    /// Heap owner tag, also the app's handle.
    tag: u8,
    /// The app's widget state (focus, held button), kept while it is away.
    ui: UiState,
    last_used: u64,
    cpu: CpuMeter,
    hang: HangWatch,
    thumb: Option<Thumbnail>,
    trimmed_at: Option<u64>,
    next_bg_tick: u64,
    /// Heap attributed to the app at the last check (SRAM, PSRAM).
    heap: (usize, usize),
    granted: Capabilities,
    /// Duration of the app's last foreground `update`.
    frame_us: u32,
}

impl Running {
    fn name(&self) -> &'static str {
        self.info.name
    }
}

/// Call into an app with its heap tag set and its CPU time measured.
fn call<R>(r: &mut Running, sys: &mut dyn System, f: impl FnOnce(&mut dyn App, &mut dyn System) -> R) -> (R, u64) {
    let prev = sys.set_heap_owner(r.tag);
    let t0 = sys.uptime_us();
    let out = f(r.app.as_mut(), sys);
    let us = sys.uptime_us().saturating_sub(t0);
    sys.set_heap_owner(prev);
    r.cpu.add(us);
    (out, us)
}

enum Anim {
    /// The app's snapshot shrinks into its launcher icon.
    ToHome { tag: u8, start: u64 },
}

struct HangPrompt {
    tag: u8,
    worst_ms: u32,
}

/// The launcher pages' static layer (wallpaper, title, icons, labels).
/// Drawing a page takes about 280 ms on the board, far too slow for
/// gestures, page swipes and transitions, so each page is drawn once and
/// copied afterwards (about 710 KB per page in portrait, in PSRAM by size).
struct HomeCache {
    size: (i32, i32),
    /// Catalogue version the pages were drawn from.
    version: u32,
    pages: Vec<Option<Vec<Color>>>,
}

/// Render times of gesture and animation frames, logged per gesture.
#[derive(Default)]
struct Perf {
    frames: u32,
    total_us: u64,
    max_us: u64,
}

pub struct Desktop {
    fonts: Fonts,
    theme: Theme,
    /// Widget state of the home screen, the switcher and dialogs.
    shell_ui: UiState,
    keyboard: Keyboard,
    keyboard_open: bool,
    running: Vec<Running>,
    /// Tag of the foreground app.
    fg: Option<u8>,
    limits: Limits,
    gestures: Recognizer,
    gestures_enabled: bool,
    track: Option<Track>,
    /// The open switcher and the app to return to when it is dismissed.
    switcher: Option<(Switcher, Option<u8>)>,
    anim: Option<Anim>,
    notice: Option<(String, u64)>,
    hang: Option<HangPrompt>,
    /// Snapshot to take from the canvas at the start of the next frame.
    pending_snapshot: Option<u8>,
    last_check: u64,
    usage: Rc<Vec<AppUsage>>,
    perf: Perf,
    lock: Lock,
    home_cache: Option<HomeCache>,
    catalogue: Catalogue,
    /// Home screen page; kept while apps come and go (not across reboots).
    pager: Pager,
}

impl Default for Desktop {
    fn default() -> Self {
        Self::new()
    }
}

/// Input with the touch held back from the app (keys still go through).
fn hold_back(i: &mut Input) {
    i.pressed = false;
    i.released = false;
    i.down = false;
    i.delta = Point::default();
}

/// Turn a held-back touch into the press the app should have seen.
fn replay(i: &mut Input, at: Point, released: bool) {
    i.pressed = true;
    i.press_pos = at;
    if released {
        i.down = false;
        i.released = true;
        i.pos = at;
        i.delta = Point::default();
    } else {
        i.down = true;
        i.released = false;
        i.delta = Point::new(i.pos.x - at.x, i.pos.y - at.y);
    }
}

fn caps_note(c: Capabilities) -> &'static str {
    match (c.contains(Capabilities::AUDIO), c.contains(Capabilities::NETWORK)) {
        (true, true) => "audio, network",
        (true, false) => "audio",
        (false, true) => "network",
        _ => "",
    }
}

impl Desktop {
    pub fn new() -> Self {
        Self {
            fonts: builtin::load(),
            theme: Theme::dark(),
            shell_ui: UiState::default(),
            keyboard: Keyboard::default(),
            keyboard_open: false,
            running: Vec::new(),
            fg: None,
            limits: Limits::default(),
            gestures: Recognizer::new(gesture::Config::default()),
            gestures_enabled: true,
            track: None,
            switcher: None,
            anim: None,
            notice: None,
            hang: None,
            pending_snapshot: None,
            last_check: 0,
            usage: Rc::new(Vec::new()),
            perf: Perf::default(),
            lock: Lock::default(),
            home_cache: None,
            catalogue: Catalogue::builtin(),
            pager: Pager::default(),
        }
        .with_group("Examples", crate::examples::EXAMPLES)
    }

    fn with_group(mut self, group: &'static str, apps: &'static [AppInfo]) -> Self {
        self.register_group(group, apps);
        self
    }

    /// Add an app to the launcher. Apps of the main group ([`launcher::MAIN`],
    /// the built-in ones) come first; every other group, such as
    /// "Examples", starts on a page of its own titled with the group name.
    /// Registering a known id again moves it to `group`.
    pub fn register_app(&mut self, group: &'static str, app: &'static AppInfo) {
        self.catalogue.register(group, app);
    }

    /// Register every app of `apps` in `group`.
    pub fn register_group(&mut self, group: &'static str, apps: &'static [AppInfo]) {
        for app in apps {
            self.catalogue.register(group, app);
        }
    }

    /// Home screen page shown (0 = first).
    pub fn home_page(&self) -> usize {
        self.pager.page()
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

    /// Id of the app in the foreground.
    pub fn running_app(&self) -> Option<&'static str> {
        self.fg.and_then(|t| self.find(t)).map(|i| self.running[i].info.id)
    }

    /// Enable or disable system gestures. The lock screen turns them off so
    /// it cannot be swiped away.
    pub fn set_gestures_enabled(&mut self, on: bool) {
        if !on {
            self.gestures.reset();
            self.track = None;
        }
        self.gestures_enabled = on;
    }

    /// Resource use of the running built-in apps (refreshed once a second).
    pub fn app_usage(&self) -> Vec<AppUsage> {
        self.usage.as_ref().clone()
    }

    fn find(&self, tag: u8) -> Option<usize> {
        self.running.iter().position(|r| r.tag == tag)
    }

    /// Start app `id`, or bring it to the front if it is already running.
    pub fn launch(&mut self, id: &str, sys: &mut dyn System) {
        // Called from outside a frame: the canvas still shows the app.
        if let Some(tag) = self.fg {
            self.pending_snapshot = Some(tag);
        }
        self.start(id, sys);
    }

    fn start(&mut self, id: &str, sys: &mut dyn System) {
        let Some(info) = self.catalogue.find(id) else {
            sys.log(&format!("desktop: no app '{id}'"));
            return;
        };
        self.switcher = None;
        self.anim = None;
        if let Some(tag) = self.running.iter().find(|r| r.info.id == info.id).map(|r| r.tag) {
            self.bring_to_front(tag, sys);
            return;
        }
        self.leave_foreground(sys);
        let records = self.records();
        if let Some(v) = policy::launch_victim(&records, &self.limits) {
            let tag = self.running[v].tag;
            self.stop(tag, sys, Some(KillReason::AppLimit));
        }
        let Some(tag) = (1..=MAX_TAG).find(|t| self.find(*t).is_none()) else {
            sys.log("desktop: no free app slot");
            return;
        };
        let now = sys.uptime_ms();
        let prev = sys.set_heap_owner(tag);
        let app = (info.create)();
        sys.set_heap_owner(prev);
        self.running.push(Running {
            info,
            app,
            state: AppState::Foreground,
            tag,
            ui: UiState::default(),
            last_used: now,
            cpu: CpuMeter::new(now),
            hang: HangWatch::default(),
            thumb: None,
            trimmed_at: None,
            next_bg_tick: 0,
            heap: (0, 0),
            granted: Capabilities::NONE,
            frame_us: 0,
        });
        self.fg = Some(tag);
        self.keyboard_open = false;
        self.refresh_usage(sys);
    }

    /// Move a running app to `to`, running the lifecycle callbacks.
    fn set_state(&mut self, idx: usize, to: AppState, sys: &mut dyn System) {
        let r = &mut self.running[idx];
        let callbacks = match lifecycle::transition(r.state, to) {
            Ok(c) => c,
            Err(e) => {
                sys.log(&format!("desktop: {} cannot go from {:?} to {:?}", r.name(), e.from, e.to));
                return;
            }
        };
        for cb in callbacks {
            call(r, sys, |app, sys| match cb {
                Callback::Pause => app.on_pause(sys),
                Callback::Resume => app.on_resume(sys),
                Callback::Stop => app.on_stop(sys),
            });
        }
        r.state = to;
    }

    fn leave_foreground(&mut self, sys: &mut dyn System) {
        let Some(idx) = self.fg.take().and_then(|t| self.find(t)) else { return };
        let r = &mut self.running[idx];
        r.last_used = sys.uptime_ms();
        r.ui.wants_keyboard = false;
        r.ui.active = None;
        r.next_bg_tick = 0;
        let granted = lifecycle::granted(r.info.background, r.app.background_work());
        r.granted = granted;
        self.keyboard_open = false;
        self.set_state(idx, lifecycle::off_screen_state(granted), sys);
    }

    fn bring_to_front(&mut self, tag: u8, sys: &mut dyn System) {
        if self.fg == Some(tag) {
            return;
        }
        self.leave_foreground(sys);
        let Some(idx) = self.find(tag) else { return };
        self.set_state(idx, AppState::Foreground, sys);
        self.running[idx].last_used = sys.uptime_ms();
        self.running[idx].granted = Capabilities::NONE;
        self.fg = Some(tag);
        self.switcher = None;
        self.anim = None;
    }

    fn go_home(&mut self, sys: &mut dyn System, animate: bool) {
        let tag = self.fg;
        self.leave_foreground(sys);
        self.switcher = None;
        if let (true, Some(tag)) = (animate, tag) {
            self.anim = Some(Anim::ToHome { tag, start: sys.uptime_ms() });
        }
    }

    /// Destroy a running app; `reason` is shown to the user when the
    /// system (not the user) closed it.
    fn stop(&mut self, tag: u8, sys: &mut dyn System, reason: Option<KillReason>) {
        let Some(idx) = self.find(tag) else { return };
        if self.fg == Some(tag) {
            self.keyboard_open = false;
        }
        self.set_state(idx, AppState::Stopped, sys);
        if self.fg == Some(tag) {
            self.fg = None;
        }
        let r = self.running.remove(idx);
        let name = r.name();
        let prev = sys.set_heap_owner(tag);
        drop(r);
        sys.set_heap_owner(prev);
        sys.clear_heap_owner(tag);
        if self.hang.as_ref().is_some_and(|h| h.tag == tag) {
            self.hang = None;
        }
        if let Some(reason) = reason {
            let why = match reason {
                KillReason::OverBudget => "it used too much memory",
                KillReason::Pressure => "memory is low",
                KillReason::AppLimit => "too many apps were open",
            };
            let msg = format!("Closed {name}: {why}");
            sys.log(&format!("desktop: {msg}"));
            self.notice = Some((msg, sys.uptime_ms() + NOTICE_MS));
        }
        self.refresh_usage(sys);
    }

    fn capture(&mut self, canvas: &Canvas, body: Rect, tag: u8) {
        if let Some(i) = self.find(tag) {
            // Drop the old snapshot first so two never coexist.
            self.running[i].thumb = None;
            self.running[i].thumb = Some(thumb::capture(canvas.pixels(), canvas.width(), body));
        }
    }

    fn records(&self) -> Vec<AppRecord> {
        self.running
            .iter()
            .map(|r| AppRecord { state: r.state, last_used: r.last_used, sram: r.heap.0, psram: r.heap.1, trimmed_at: r.trimmed_at })
            .collect()
    }

    fn refresh_usage(&mut self, sys: &dyn System) {
        let list = self
            .running
            .iter()
            .map(|r| {
                let (sram, psram) = sys.heap_owner_usage(r.tag);
                let note = match r.state {
                    AppState::Background => caps_note(r.granted),
                    _ if r.trimmed_at.is_some() => "trimmed",
                    _ => "",
                };
                AppUsage {
                    handle: AppHandle::Builtin(r.tag),
                    name: r.name().into(),
                    state: r.state,
                    cpu_permille: r.cpu.permille,
                    cpu_ms: r.cpu.total_us / 1000,
                    sram,
                    psram,
                    note: note.into(),
                    frame_us: r.frame_us,
                }
            })
            .collect();
        self.usage = Rc::new(list);
    }

    /// Render one frame. Returns how many milliseconds until the desktop
    /// wants another frame without input (`None`: only on input).
    pub fn frame(&mut self, canvas: &mut Canvas, input: &Input, sys: &mut dyn System) -> Option<u32> {
        let started_us = sys.uptime_us();
        let now = sys.uptime_ms();
        let screen = canvas.bounds();
        let (status, rest) = screen.split_top(STATUS_H);
        let body = Rect::new(rest.x, rest.y, rest.w, rest.h - HOME_BAR_H);
        let strip = Rect::new(rest.x, body.bottom(), rest.w, HOME_BAR_H);
        if let Some(tag) = self.pending_snapshot.take() {
            self.capture(canvas, body, tag);
        }
        let mut proxy = SysProxy { inner: sys, launch: None, close: Vec::new(), usage: self.usage.clone(), lock: false };

        // The lock screen covers everything and takes all input, so it
        // comes before gestures. The touch that found the screen locked only
        // wakes it. Background work (music) carries on behind it.
        let just_locked = self.lock.check(input, proxy.inner);
        if self.lock.is_locked() {
            self.gestures.reset();
            self.track = None;
            let quiet = Input { pos: input.pos, down: input.down, press_pos: input.press_pos, ..Default::default() };
            let input = if just_locked { &quiet } else { input };
            let next = self.lock.frame(canvas, input, &self.fonts, &self.theme, proxy.inner);
            let bg_due = self.background_ticks(&mut proxy, now, true);
            if now.saturating_sub(self.last_check) >= CHECK_MS {
                self.check_resources(&mut proxy, now);
            }
            return match (next, bg_due) {
                (Some(n), Some(due)) => Some(n.min(due.saturating_sub(now) as u32)),
                (None, Some(due)) => Some(due.saturating_sub(now) as u32),
                (n, None) => n,
            };
        }

        let keyboard_was_open = self.keyboard_open;
        let fg_before = self.fg;
        let switcher_before = self.switcher.is_some();

        // System gestures see the touch first.
        let enabled = self.gestures_enabled && self.hang.is_none();
        let verdict = if enabled {
            let s = Sample { pos: input.pos, down: input.down, fingers: input.fingers, t_ms: now };
            self.gestures.update(s, (screen.w, screen.h))
        } else {
            Verdict::Pass
        };
        let mut app_input = input.clone();
        let was_tracking = self.track.take().is_some();
        let mut fired = None;
        match verdict {
            Verdict::Pass => {}
            Verdict::Pending | Verdict::Cancelled => hold_back(&mut app_input),
            Verdict::Replay { at, released } => replay(&mut app_input, at, released),
            Verdict::Tracking(t) => {
                hold_back(&mut app_input);
                // The canvas still holds the app's last clean frame.
                if let (false, Some(tag), None) = (was_tracking, self.fg, &self.switcher) {
                    self.capture(canvas, body, tag);
                }
                self.track = Some(t);
            }
            Verdict::Fire(g) => {
                hold_back(&mut app_input);
                fired = Some(g);
            }
        }
        // Escape on the USB console works like the Back gesture.
        if app_input.keys.contains(&Key::Escape) {
            app_input.keys.retain(|k| *k != Key::Escape);
            if enabled {
                fired = fired.or(Some(Gesture::Back));
            }
        }
        if let Some(g) = fired {
            self.on_gesture(g, &mut proxy, now);
        }

        let mut content_input = app_input;
        if self.hang.is_some() {
            hold_back(&mut content_input);
            content_input.keys.clear();
        }
        let mut next = None;
        let mut leave = false;
        if self.switcher.is_some() {
            self.switcher_frame(canvas, &content_input, body, &mut proxy, now);
        } else if self.fg.is_some() {
            (next, leave) = self.app_frame(canvas, &content_input, body, &mut proxy, now);
        } else {
            self.home_frame(canvas, &content_input, body, &mut proxy, now);
            self.draw_home_anim(canvas, body, now);
        }
        if leave {
            if let Some(tag) = self.fg {
                self.capture(canvas, body, tag);
            }
            self.go_home(&mut proxy, true);
        }

        let t = self.theme;
        feedback::home_indicator(canvas, strip, Color::hex(0x080a0f), if self.track.is_some() { t.surface_alt } else { t.text_dim });
        if let Some(tr) = &self.track {
            feedback::track(canvas, screen, tr, &self.gestures.cfg, t.accent);
        }
        if self.hang.is_some() {
            self.hang_dialog(canvas, input, body, &mut proxy);
        }
        self.draw_notice(canvas, body, now);
        self.status_bar(canvas, input, status, &mut proxy);

        if proxy.lock {
            self.lock.lock_now(proxy.inner);
            return Some(0);
        }
        // Requests made by apps during the frame.
        let closes = core::mem::take(&mut proxy.close);
        for tag in closes {
            self.stop(tag, &mut proxy, None);
        }
        if let Some(id) = proxy.launch.take() {
            if let Some(tag) = self.fg {
                self.capture(canvas, body, tag);
            }
            self.start(&id, &mut proxy);
        }
        let bg_due = self.background_ticks(&mut proxy, now, false);
        if now.saturating_sub(self.last_check) >= CHECK_MS {
            self.check_resources(&mut proxy, now);
        }

        let animating = self.track.is_some()
            || self.gestures.busy()
            || (self.fg.is_none() && self.switcher.is_none() && self.pager.animating(now))
            || self.anim.is_some()
            || self.switcher.as_ref().is_some_and(|(s, _)| s.animating(now));
        self.account_perf(&mut proxy, animating, started_us);

        // Layout changes (keyboard, app switch) need an immediate redraw,
        // and so does any interaction: widgets react while drawing, so state
        // changed by a tap or key only shows up in the following frame.
        let interacted = input.pressed || input.released || !input.keys.is_empty();
        let changed = self.fg != fg_before || self.switcher.is_some() != switcher_before;
        if self.keyboard_open != keyboard_was_open || interacted || changed {
            return Some(0);
        }
        let mut wait = next.map_or(STATUS_TICK_MS, |n| n.min(STATUS_TICK_MS));
        if animating {
            wait = wait.min(ANIM_FRAME_MS);
        }
        if let Some(due) = bg_due {
            wait = wait.min(due.saturating_sub(now) as u32);
        }
        if let Some((_, until)) = &self.notice {
            wait = wait.min(until.saturating_sub(now).max(1) as u32);
        }
        Some(wait)
    }

    /// Run background work without drawing, for when nothing is shown
    /// (screen off): background ticks (the foreground app included, as
    /// while locked) and the periodic resource checks. Returns when it
    /// wants to run again.
    pub fn background_step(&mut self, sys: &mut dyn System) -> Option<u32> {
        let now = sys.uptime_ms();
        let mut proxy = SysProxy { inner: sys, launch: None, close: Vec::new(), usage: self.usage.clone(), lock: false };
        let due = self.background_ticks(&mut proxy, now, true);
        if now.saturating_sub(self.last_check) >= CHECK_MS {
            self.check_resources(&mut proxy, now);
        }
        due.map(|d| d.saturating_sub(now) as u32)
    }

    fn on_gesture(&mut self, g: Gesture, sys: &mut SysProxy, now: u64) {
        match g {
            Gesture::Home => {
                self.switcher = None;
                self.go_home(sys, true);
            }
            Gesture::Switcher => {
                if self.switcher.is_none() {
                    let back_to = self.fg;
                    self.leave_foreground(sys);
                    self.anim = None;
                    self.shell_ui = UiState::default();
                    self.switcher = Some((Switcher::new(now), back_to));
                }
            }
            Gesture::Back if self.switcher.is_none() && self.fg.is_none() => self.turn_page(-1, sys, now),
            Gesture::Forward if self.switcher.is_none() && self.fg.is_none() => self.turn_page(1, sys, now),
            Gesture::Back => {
                if self.switcher.is_some() {
                    self.dismiss_switcher(sys);
                } else if let Some(idx) = self.fg.and_then(|t| self.find(t)) {
                    let (handled, _) = call(&mut self.running[idx], sys, |app, sys| app.on_back(sys));
                    if !handled {
                        self.go_home(sys, true);
                    }
                }
            }
            Gesture::Forward => {
                if let Some(idx) = self.fg.and_then(|t| self.find(t)) {
                    call(&mut self.running[idx], sys, |app, sys| app.on_forward(sys));
                }
            }
        }
    }

    /// Edge Back / Forward on the home screen turn to the previous / next
    /// page, as a page swipe in the same direction would.
    fn turn_page(&mut self, delta: i32, sys: &mut dyn System, now: u64) {
        let (w, h) = sys.orientation().size();
        let body = Rect::new(0, STATUS_H, w, h - STATUS_H - HOME_BAR_H);
        let n = self.catalogue.pages(Grid::for_area(body).per_page()).len();
        let target = (self.pager.page() as i32 + delta).clamp(0, n as i32 - 1) as usize;
        self.pager.go_to(target, n, now, w);
    }

    fn dismiss_switcher(&mut self, sys: &mut dyn System) {
        if let Some((_, back_to)) = self.switcher.take() {
            match back_to.filter(|t| self.find(*t).is_some()) {
                Some(tag) => self.bring_to_front(tag, sys),
                None => self.fg = None,
            }
        }
    }

    fn account_perf(&mut self, sys: &mut dyn System, animating: bool, started_us: u64) {
        if animating {
            let us = sys.uptime_us().saturating_sub(started_us);
            self.perf.frames += 1;
            self.perf.total_us += us;
            self.perf.max_us = self.perf.max_us.max(us);
        } else if self.perf.frames > 0 {
            let p = core::mem::take(&mut self.perf);
            sys.log(&format!(
                "desktop: gesture/animation {} frames, render avg {:.1} ms, max {:.1} ms",
                p.frames,
                p.total_us as f32 / p.frames as f32 / 1000.0,
                p.max_us as f32 / 1000.0
            ));
        }
    }

    /// Ticks for background apps with a granted capability. Returns when
    /// the next one is due. While the screen is locked the foreground app
    /// is not drawn either, so it is treated the same way (without being
    /// suspended when it has no background work).
    fn background_ticks(&mut self, sys: &mut dyn System, now: u64, locked: bool) -> Option<u64> {
        let mut due: Option<u64> = None;
        let mut suspend = Vec::new();
        for (i, r) in self.running.iter_mut().enumerate() {
            let fg = r.state == AppState::Foreground;
            if r.state != AppState::Background && !(locked && fg) {
                continue;
            }
            let granted = lifecycle::granted(r.info.background, r.app.background_work());
            if !fg {
                r.granted = granted;
            }
            if granted.is_empty() {
                if !fg {
                    suspend.push(i);
                }
                continue;
            }
            if now >= r.next_bg_tick {
                if r.cpu.bg_allowed(&self.limits) {
                    call(r, sys, |app, sys| app.background_tick(sys));
                }
                let every = r.app.tick_interval_ms().unwrap_or(STATUS_TICK_MS).max(self.limits.bg_min_tick_ms);
                r.next_bg_tick = now + every as u64;
            }
            due = Some(due.map_or(r.next_bg_tick, |d| d.min(r.next_bg_tick)));
        }
        for i in suspend {
            self.set_state(i, AppState::Suspended, sys);
        }
        due
    }

    /// Once a second: CPU windows, heap figures, memory policy.
    fn check_resources(&mut self, sys: &mut dyn System, now: u64) {
        self.last_check = now;
        let limits = self.limits;
        let mut throttled = Vec::new();
        for (i, r) in self.running.iter_mut().enumerate() {
            r.heap = sys.heap_owner_usage(r.tag);
            let bg = r.state == AppState::Background;
            if r.cpu.roll(now, bg, &limits) && bg {
                throttled.push(i);
            }
        }
        for i in throttled {
            let name = self.running[i].name();
            sys.log(&format!("desktop: {name} exceeded its background CPU budget; suspended"));
            self.set_state(i, AppState::Suspended, sys);
        }

        let stats = sys.stats();
        let free = |used: usize, total: usize| if total == 0 { usize::MAX } else { total.saturating_sub(used) };
        let (free_sram, free_psram) = (free(stats.heap_used, stats.heap_total), free(stats.psram_used, stats.psram_total));
        if policy::under_pressure(free_sram, free_psram, &limits) && self.home_cache.take().is_some() {
            sys.log("desktop: low memory, dropped the home screen cache");
        }
        let mut records = self.records();
        let actions = policy::plan(&mut records, free_sram, free_psram, &limits, now);
        for (r, rec) in self.running.iter_mut().zip(&records) {
            r.trimmed_at = rec.trimmed_at;
        }
        // Indices refer to this moment's list; resolve them to tags first.
        let actions: Vec<(u8, Option<KillReason>)> = actions
            .into_iter()
            .map(|a| match a {
                policy::Action::Trim(i) => (self.running[i].tag, None),
                policy::Action::Kill(i, why) => (self.running[i].tag, Some(why)),
            })
            .collect();
        for (tag, kill) in actions {
            match kill {
                Some(why) => self.stop(tag, sys, Some(why)),
                None => {
                    if let Some(i) = self.find(tag) {
                        let r = &mut self.running[i];
                        sys.log(&format!("desktop: asking {} to free memory ({} K SRAM, {} K PSRAM)", r.name(), r.heap.0 / 1024, r.heap.1 / 1024));
                        call(r, sys, |app, sys| app.on_low_memory(sys));
                    }
                }
            }
        }
        self.refresh_usage(sys);
    }

    fn ui<'f, 'c>(&'f mut self, canvas: &'f mut Canvas<'c>, input: &'f Input) -> Ui<'f, 'c> {
        Ui { canvas, input, theme: &self.theme, fonts: &self.fonts, state: &mut self.shell_ui }
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
        let battery = stats.battery_mv.map(|mv| format!("{}.{:02}V", mv / 1000, mv % 1000 / 10));
        let right = if r.w >= 800 {
            let battery = battery.map(|b| format!("   {b}")).unwrap_or_default();
            format!("CPU {:>3.0}%   RAM {}K/{}K   {}{}", stats.cpu_load * 100.0, used, total, sys.net().status(), battery)
        } else {
            let battery = battery.map(|b| format!("  {b}")).unwrap_or_default();
            format!("CPU {:.0}%  RAM {}M{}", stats.cpu_load * 100.0, used / 1024, battery)
        };
        let orientation = sys.orientation();
        let lockable = lock::is_configured(sys);
        // While a dialog is up, the status bar does not react.
        let blocked = self.hang.is_some();
        let mut idle = input.clone();
        if blocked {
            hold_back(&mut idle);
        }
        let mut ui = self.ui(canvas, &idle);
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

    fn home_frame(&mut self, canvas: &mut Canvas, input: &Input, body: Rect, sys: &mut SysProxy, now: u64) {
        let grid = Grid::for_area(body);
        let pages = self.catalogue.pages(grid.per_page());
        let n = pages.len();
        let s = pager::Sample { pos: input.pos, down: input.down, t_ms: now };
        let scroll = self.pager.update(s, body.w, n, self.gestures.cfg.slop);

        let size = (body.w, body.h);
        let version = self.catalogue.version;
        if !self.home_cache.as_ref().is_some_and(|c| c.size == size && c.version == version && c.pages.len() == n) {
            self.home_cache = Some(HomeCache { size, version, pages: (0..n).map(|_| None).collect() });
        }
        // Pages on screen: the one scrolled to and its right neighbour.
        let first = (scroll.max(0) / body.w) as usize;
        let visible: Vec<usize> = [first, first + 1].into_iter().filter(|&p| p < n).collect();
        for &p in &visible {
            if self.home_cache.as_ref().is_some_and(|c| c.pages[p].is_none()) {
                self.draw_home_page(canvas, body, &grid, &pages[p]);
                let mut pixels = Vec::with_capacity(((body.w * body.h) as usize).max(thumb::PSRAM_MIN / 2));
                let stride = canvas.width();
                for y in body.y..body.bottom() {
                    let row = (y * stride + body.x) as usize;
                    pixels.extend_from_slice(&canvas.pixels()[row..row + body.w as usize]);
                }
                if let Some(c) = self.home_cache.as_mut() {
                    c.pages[p] = Some(pixels);
                }
            }
        }
        // Rubber-banding past either end shows plain wallpaper.
        if scroll < 0 || scroll > (n as i32 - 1) * body.w {
            canvas.fill_gradient(body, Color::hex(0x1b2a4a), Color::hex(0x0b0f18));
        }
        let old = canvas.set_clip(body.intersect(&canvas.clip()));
        if let Some(cache) = &self.home_cache {
            for &p in &visible {
                if let Some(px) = &cache.pages[p] {
                    canvas.blit(Point::new(body.x + p as i32 * body.w - scroll, body.y), px, body.w, body.h);
                }
            }
        }
        let page_rect = |p: usize| Rect::new(body.x + p as i32 * body.w - scroll, body.y, body.w, body.h);
        // A dot under running apps.
        for r in &self.running {
            if let Some((p, slot)) = launcher::locate(&pages, r.info.id).filter(|(p, _)| visible.contains(p)) {
                let (cell, icon) = grid.cell(page_rect(p), slot);
                canvas.fill_circle(Point::new(cell.x + cell.w / 2, icon.bottom() + 42), 3.0, self.theme.accent);
            }
        }
        canvas.restore_clip(old);

        // Icons react only while the pages rest.
        let mut tapped = None;
        if !self.pager.swiping() && !self.pager.animating(now) {
            let page = self.pager.page().min(n.saturating_sub(1));
            let mut ui = self.ui(canvas, input);
            for (slot, app) in pages[page].apps.iter().enumerate() {
                let (cell, icon) = grid.cell(body, slot);
                let (held, clicked) = ui.interact(cell);
                if held {
                    ui.canvas.fill_rect_alpha(icon, Color::BLACK, 70);
                }
                if clicked {
                    tapped = Some(app.id);
                }
            }
        }
        if n > 1 {
            feedback::page_dots(canvas, body, n, scroll as f32 / body.w as f32, self.theme.text, self.theme.text_dim);
        }
        if let Some(id) = tapped {
            sys.launch(id);
        }
    }

    /// The static layer of one launcher page, drawn at `body`.
    fn draw_home_page(&mut self, canvas: &mut Canvas, body: Rect, grid: &Grid, page: &launcher::Page) {
        canvas.fill_gradient(body, Color::hex(0x1b2a4a), Color::hex(0x0b0f18));
        let (f, text) = (self.fonts.body, self.theme.text);
        if page.group != launcher::MAIN {
            let tf = self.fonts.large;
            let w = tf.measure(page.group);
            let y = body.y + (launcher::TOP - tf.line_height) / 2 + 4;
            tf.draw(canvas, Point::new(body.x + (body.w - w) / 2, y), page.group, text);
        }
        for (slot, app) in page.apps.iter().enumerate() {
            let (cell, icon) = grid.cell(body, slot);
            (app.icon)(canvas, icon);
            let label = Rect::new(cell.x, icon.bottom() + 8, cell.w, 28);
            let w = f.measure(app.name);
            let old = canvas.set_clip(label.intersect(&canvas.clip()));
            f.draw(canvas, Point::new(label.x + (label.w - w) / 2, label.y + (label.h - f.line_height) / 2), app.name, text);
            canvas.restore_clip(old);
        }
    }

    /// Where an app's icon is on the home screen page shown now, if there.
    fn icon_on_current_page(&self, body: Rect, id: &str) -> Option<Rect> {
        let grid = Grid::for_area(body);
        let pages = self.catalogue.pages(grid.per_page());
        match launcher::locate(&pages, id) {
            Some((p, slot)) if p == self.pager.page() => Some(grid.cell(body, slot).1),
            _ => None,
        }
    }

    fn draw_home_anim(&mut self, canvas: &mut Canvas, body: Rect, now: u64) {
        let Some(Anim::ToHome { tag, start }) = self.anim else { return };
        let t = now.saturating_sub(start) as f32 / HOME_ANIM_MS as f32;
        let r = self.find(tag).map(|i| &self.running[i]);
        match r.and_then(|r| r.thumb.as_ref().map(|th| (r.info, th))) {
            Some((info, th)) if t < 1.0 => {
                // Into its icon, or the middle of the page if the icon is
                // on another page.
                let c = body.center();
                let to = self.icon_on_current_page(body, info.id).unwrap_or(Rect::new(c.x - 30, c.y - 50, 60, 100));
                feedback::shrink_to(canvas, th, body, to, t);
            }
            _ => self.anim = None,
        }
    }

    fn switcher_frame(&mut self, canvas: &mut Canvas, input: &Input, body: Rect, sys: &mut SysProxy, now: u64) {
        let mut order: Vec<usize> = (0..self.running.len()).collect();
        order.sort_by_key(|&i| core::cmp::Reverse(self.running[i].last_used));
        let cards: Vec<Card> = order
            .iter()
            .map(|&i| {
                let r = &self.running[i];
                Card { tag: r.tag, name: r.info.name, icon: r.info.icon, thumb: r.thumb.as_ref(), state: r.state }
            })
            .collect();
        let Some((sw, _)) = self.switcher.as_mut() else { return };
        let mut ui = Ui { canvas, input, theme: &self.theme, fonts: &self.fonts, state: &mut self.shell_ui };
        let action = sw.show(&mut ui, body, &cards, now);
        drop(cards);
        match action {
            Some(switcher::Action::Open(tag)) => {
                self.switcher = None;
                self.bring_to_front(tag, sys);
            }
            Some(switcher::Action::Close(tag)) => self.stop(tag, sys, None),
            Some(switcher::Action::CloseAll) => {
                let tags: Vec<u8> = self.running.iter().map(|r| r.tag).collect();
                for tag in tags {
                    self.stop(tag, sys, None);
                }
                self.switcher = None;
            }
            Some(switcher::Action::Dismiss) => self.dismiss_switcher(sys),
            None => {}
        }
    }

    /// Draw the foreground app with its title bar. Returns its tick request
    /// and whether the user asked to go home.
    fn app_frame(&mut self, canvas: &mut Canvas, input: &Input, body: Rect, sys: &mut SysProxy, now: u64) -> (Option<u32>, bool) {
        let Some(idx) = self.fg.and_then(|t| self.find(t)) else { return (None, false) };
        let (title_r, mut area) = body.split_top(TITLE_H);
        let mut app_input = input.clone();
        let r = &mut self.running[idx];

        // Keyboard first: its key goes to the app in the same frame, and
        // touches on it must not reach the app.
        if self.keyboard_open {
            let (rest, kb) = area.split_top(area.h - keyboard::HEIGHT);
            area = rest;
            let key = {
                let mut ui = Ui { canvas, input, theme: &self.theme, fonts: &self.fonts, state: &mut r.ui };
                self.keyboard.show(&mut ui, kb)
            };
            if kb.contains(input.press_pos) {
                app_input.pressed = false;
                app_input.released = false;
                app_input.down = false;
            }
            if let Some(k) = key {
                app_input.keys.push(k);
            }
        }

        let name = r.info.name;
        let mut ui = Ui { canvas, input: &app_input, theme: &self.theme, fonts: &self.fonts, state: &mut r.ui };
        let t = *ui.theme;
        ui.fill(title_r, t.surface);
        ui.canvas.fill_rect(Rect::new(title_r.x, title_r.bottom() - 1, title_r.w, 1), t.border);
        let back = Rect::new(title_r.x + 8, title_r.y + 6, 130, title_r.h - 12);
        let home = ui.icon_button(back, |c, r, col| {
            let cy = r.y + r.h / 2;
            c.line(Point::new(r.x + 26, cy - 10), Point::new(r.x + 16, cy), 3.0, col);
            c.line(Point::new(r.x + 16, cy), Point::new(r.x + 26, cy + 10), 3.0, col);
        });
        let f = ui.fonts.body;
        ui.text(Rect::new(back.x + 36, back.y, 90, back.h), "Home", &f, t.text, Align::Left);
        let bold = ui.fonts.bold;
        ui.text(title_r, name, &bold, t.text, Align::Center);

        ui.state.wants_keyboard = false;
        let old = ui.canvas.set_clip(area);
        let prev = sys.set_heap_owner(r.tag);
        let t0 = sys.uptime_us();
        r.app.update(&mut ui, area, sys);
        let us = sys.uptime_us().saturating_sub(t0);
        sys.set_heap_owner(prev);
        ui.canvas.restore_clip(old);
        r.cpu.add(us);
        r.frame_us = us.min(u32::MAX as u64) as u32;
        self.keyboard_open = r.ui.wants_keyboard;
        let tick = r.app.tick_interval_ms();

        // Frame watchdog: a handler that blocks the GUI loop.
        let ms = (us / 1000) as u32;
        if ms > self.limits.frame_limit_ms {
            sys.log(&format!("desktop: {name} blocked the screen for {ms} ms"));
        }
        if r.hang.record(ms, now, &self.limits) {
            let worst_ms = r.hang.worst_ms;
            sys.log(&format!("desktop: {name} is not responding (worst frame {worst_ms} ms)"));
            self.hang = Some(HangPrompt { tag: r.tag, worst_ms });
        }
        (tick, home)
    }

    fn hang_dialog(&mut self, canvas: &mut Canvas, input: &Input, body: Rect, sys: &mut SysProxy) {
        let Some(h) = &self.hang else { return };
        let (tag, worst) = (h.tag, h.worst_ms);
        let Some(idx) = self.find(tag) else {
            self.hang = None;
            return;
        };
        let name = self.running[idx].name();
        canvas.fill_rect_alpha(body, Color::BLACK, 140);
        let w = (body.w - 48).min(420);
        let panel = Rect::new(body.x + (body.w - w) / 2, body.y + body.h / 2 - 120, w, 230);
        let mut ui = self.ui(canvas, input);
        let t = *ui.theme;
        ui.panel(panel);
        let inner = panel.inset(20);
        let (bold, small) = (ui.fonts.bold, ui.fonts.small);
        ui.text(Rect::new(inner.x, inner.y, inner.w, 36), &format!("{name} isn't responding"), &bold, t.text, Align::Left);
        let detail = format!("It blocked the screen for up to {:.1} s several times.", worst as f32 / 1000.0);
        let mut y = inner.y + 48;
        for line in small.wrap(&detail, inner.w) {
            ui.text(Rect::new(inner.x, y, inner.w, small.line_height), &line, &small, t.text_dim, Align::Left);
            y += small.line_height + 2;
        }
        let bw = (inner.w - 12) / 2;
        let wait = Rect::new(inner.x, inner.bottom() - 52, bw, 52);
        let close = Rect::new(wait.right() + 12, wait.y, bw, 52);
        let waited = ui.button(wait, "Wait");
        let closed = ui.button_colored(close, "Close app", t.danger, Color::WHITE);
        if closed {
            self.hang = None;
            self.stop(tag, sys, None);
        } else if waited {
            self.hang = None;
        }
    }

    fn draw_notice(&mut self, canvas: &mut Canvas, body: Rect, now: u64) {
        let Some((msg, until)) = &self.notice else { return };
        if now >= *until {
            self.notice = None;
            return;
        }
        let f = self.fonts.small;
        let w = (f.measure(msg) + 40).min(body.w - 24);
        let r = Rect::new(body.x + (body.w - w) / 2, body.bottom() - 64, w, 44);
        canvas.fill_round_rect(r, 22, Color::hex(0x2b3446));
        let ty = r.y + (r.h - f.line_height) / 2;
        let old = canvas.set_clip(r.inset(12));
        f.draw(canvas, Point::new(r.x + 20, ty), msg, self.theme.text);
        canvas.restore_clip(old);
    }
}

/// Wraps the OS `System` to intercept requests aimed at the desktop.
struct SysProxy<'a> {
    inner: &'a mut dyn System,
    launch: Option<String>,
    /// Built-in apps to stop after the frame (by tag).
    close: Vec<u8>,
    /// Built-in app usage as of the last check.
    usage: Rc<Vec<AppUsage>>,
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
    fn uptime_us(&self) -> u64 {
        self.inner.uptime_us()
    }
    fn log(&mut self, msg: &str) {
        self.inner.log(msg)
    }
    fn set_heap_owner(&mut self, owner: u8) -> u8 {
        self.inner.set_heap_owner(owner)
    }
    fn heap_owner_usage(&self, owner: u8) -> (usize, usize) {
        self.inner.heap_owner_usage(owner)
    }
    fn clear_heap_owner(&mut self, owner: u8) {
        self.inner.clear_heap_owner(owner)
    }
    fn running_apps(&self) -> Vec<AppUsage> {
        let mut list = self.usage.as_ref().clone();
        list.extend(self.inner.running_apps());
        list
    }
    fn close_app(&mut self, app: AppHandle) {
        match app {
            AppHandle::Builtin(tag) => self.close.push(tag),
            AppHandle::User(_) => self.inner.close_app(app),
        }
    }
    fn config_remove(&mut self, key: &str) {
        self.inner.config_remove(key)
    }
    fn fill_random(&mut self, buf: &mut [u8]) -> bool {
        self.inner.fill_random(buf)
    }
    fn lock_now(&mut self) {
        self.lock = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsys::TestSys;

    struct Rig {
        d: Desktop,
        sys: TestSys,
        buf: Vec<Color>,
        input: Input,
    }

    impl Rig {
        fn new() -> Self {
            let sys = TestSys::default();
            sys.advance(10_000);
            Self { d: Desktop::new(), sys, buf: alloc::vec![Color::BLACK; 480 * 800], input: Input::default() }
        }

        fn frame(&mut self) {
            let mut canvas = Canvas::new(&mut self.buf, 480, 800);
            self.d.frame(&mut canvas, &self.input, &mut self.sys);
            self.input.pressed = false;
            self.input.released = false;
            self.input.delta = Point::default();
            self.input.keys.clear();
        }

        fn touch(&mut self, x: i32, y: i32, down: bool) {
            self.sys.advance(16);
            let p = Point::new(x, y);
            if down && !self.input.down {
                self.input.pressed = true;
                self.input.press_pos = p;
            }
            if !down && self.input.down {
                self.input.released = true;
            }
            self.input.delta = Point::new(p.x - self.input.pos.x, p.y - self.input.pos.y);
            self.input.pos = p;
            self.input.down = down;
            self.input.fingers = down as u8;
            self.frame();
        }

        fn swipe(&mut self, from: (i32, i32), to: (i32, i32), hold_ms: u64) {
            self.touch(from.0, from.1, true);
            for i in 1..=8 {
                self.touch(from.0 + (to.0 - from.0) * i / 8, from.1 + (to.1 - from.1) * i / 8, true);
            }
            for _ in 0..hold_ms / 16 {
                self.touch(to.0, to.1, true);
            }
            self.touch(to.0, to.1, false);
        }

        fn tap(&mut self, x: i32, y: i32) {
            self.touch(x, y, true);
            self.touch(x, y, false);
        }

        fn launch(&mut self, id: &str) {
            self.d.launch(id, &mut self.sys);
            self.frame();
        }

        fn state_of(&self, name: &str) -> Option<AppState> {
            self.d.running.iter().find(|r| r.name() == name).map(|r| r.state)
        }

        /// Let animations and the once-a-second checks run.
        fn idle(&mut self, ms: u64) {
            for _ in 0..ms / 100 {
                self.sys.advance(100);
                self.frame();
            }
        }
    }

    #[test]
    fn home_swipe_keeps_the_app_running() {
        let mut r = Rig::new();
        r.launch("calculator");
        assert_eq!(r.d.running_app(), Some("calculator"));
        r.swipe((240, 795), (240, 600), 0);
        assert_eq!(r.d.running_app(), None);
        assert_eq!(r.state_of("Calculator"), Some(AppState::Suspended));
        assert!(r.d.running[0].thumb.is_some(), "snapshot taken when the gesture started");
        // Launching it again resumes the same instance.
        let tag = r.d.running[0].tag;
        r.launch("calculator");
        assert_eq!(r.d.running.len(), 1);
        assert_eq!(r.d.running[0].tag, tag);
        assert_eq!(r.state_of("Calculator"), Some(AppState::Foreground));
    }

    #[test]
    fn swipe_and_hold_opens_the_switcher_and_back_returns() {
        let mut r = Rig::new();
        r.launch("calculator");
        r.launch("clock");
        r.swipe((240, 795), (240, 600), 400);
        assert!(r.d.switcher.is_some());
        assert_eq!(r.state_of("Clock"), Some(AppState::Suspended));
        // Back closes the switcher and returns to the app it was opened from.
        r.swipe((3, 400), (200, 400), 0);
        assert!(r.d.switcher.is_none());
        assert_eq!(r.d.running_app(), Some("clock"));
    }

    #[test]
    fn switcher_card_tap_switches_and_close_all_empties() {
        let mut r = Rig::new();
        r.launch("calculator");
        r.launch("clock");
        r.swipe((240, 795), (240, 600), 400);
        r.idle(300);
        // Most recent first, two 176 px cards with a 24 px gap, centred:
        // Clock at 52..228, Calculator at 252..428.
        r.tap(340, 400);
        assert_eq!(r.d.running_app(), Some("calculator"));

        r.swipe((240, 795), (240, 600), 400);
        r.idle(300);
        // "Close all" sits at the right of the switcher header.
        r.tap(400, STATUS_H + 32);
        assert!(r.d.running.is_empty());
        assert!(r.d.switcher.is_none());
    }

    #[test]
    fn switcher_swipe_up_closes_a_card() {
        let mut r = Rig::new();
        r.launch("calculator");
        r.launch("clock");
        r.swipe((240, 795), (240, 600), 400);
        r.idle(300);
        r.swipe((140, 420), (140, 200), 0);
        assert_eq!(r.d.running.len(), 1);
        assert_eq!(r.state_of("Calculator"), Some(AppState::Suspended));
    }

    #[test]
    fn back_at_the_root_goes_home() {
        let mut r = Rig::new();
        r.launch("settings");
        r.swipe((2, 400), (200, 400), 0);
        assert_eq!(r.d.running_app(), None, "Settings shows its list: nothing to go back to");
        assert_eq!(r.state_of("Settings"), Some(AppState::Suspended));
        // Forward with nothing to go forward to does nothing.
        r.launch("settings");
        r.swipe((478, 400), (280, 400), 0);
        assert_eq!(r.d.running_app(), Some("settings"));
    }

    #[test]
    fn escape_works_like_back() {
        let mut r = Rig::new();
        r.launch("calculator");
        r.input.keys.push(Key::Escape);
        r.frame();
        assert_eq!(r.d.running_app(), None);
    }

    #[test]
    fn edge_taps_reach_the_app() {
        let mut r = Rig::new();
        // The first launcher icon reaches into the left edge zone.
        r.tap(10, STATUS_H + 40 + 50);
        r.frame();
        assert_eq!(r.d.running_app(), Some("calculator"));
    }

    #[test]
    fn lock_screen_takes_precedence_over_gestures() {
        let mut r = Rig::new();
        r.launch("calculator");
        r.sys.config_set(p4os_sdk::config::LOCK_ENABLED, "1");
        r.sys.config_set(p4os_sdk::config::LOCK_PIN, "any-hash");
        r.d.lock_now(&mut r.sys);
        r.frame();
        assert!(r.d.is_locked());
        r.swipe((240, 795), (240, 600), 0);
        r.swipe((240, 795), (240, 600), 400);
        r.swipe((2, 400), (200, 400), 0);
        assert!(r.d.is_locked());
        assert!(r.d.switcher.is_none());
        assert_eq!(r.d.running_app(), Some("calculator"), "nothing reached the app manager");
    }

    #[test]
    fn background_work_continues_without_frames() {
        let mut r = Rig::new();
        r.launch("calculator");
        // Nothing to do in the background: no tick requested.
        assert_eq!(r.d.background_step(&mut r.sys), None);
        r.sys.heap[r.d.running[0].tag as usize] = (2048, 0);
        r.sys.advance(1100);
        r.d.background_step(&mut r.sys);
        assert_eq!(r.d.app_usage()[0].sram, 2048, "resource checks still run");
    }

    #[test]
    fn gestures_can_be_disabled() {
        let mut r = Rig::new();
        r.launch("calculator");
        r.d.set_gestures_enabled(false);
        r.swipe((240, 795), (240, 600), 0);
        assert_eq!(r.d.running_app(), Some("calculator"));
        r.d.set_gestures_enabled(true);
        r.swipe((240, 795), (240, 600), 0);
        assert_eq!(r.d.running_app(), None);
    }

    #[test]
    fn app_limit_stops_the_least_recently_used() {
        let mut r = Rig::new();
        for id in ["calculator", "clock", "paint", "game2048", "terminal", "monitor"] {
            r.launch(id);
            r.sys.advance(100);
        }
        assert_eq!(r.d.running.len(), 6);
        r.launch("settings");
        assert_eq!(r.d.running.len(), 6);
        assert_eq!(r.state_of("Calculator"), None);
        assert!(r.sys.log.iter().any(|l| l.contains("Closed Calculator")), "{:?}", r.sys.log);
        assert_eq!(r.d.running_app(), Some("settings"));
    }

    #[test]
    fn over_budget_background_app_is_trimmed_then_stopped() {
        let mut r = Rig::new();
        r.launch("photos");
        let tag = r.d.running[0].tag;
        r.launch("calculator");
        r.sys.heap[tag as usize] = (0, r.d.limits.app_psram + 1);
        r.idle(1100);
        assert!(r.sys.log.iter().any(|l| l.contains("asking Photos to free memory")), "{:?}", r.sys.log);
        assert_eq!(r.state_of("Photos"), Some(AppState::Suspended));
        r.idle(2100);
        assert_eq!(r.state_of("Photos"), None);
        assert!(r.sys.log.iter().any(|l| l.contains("Closed Photos: it used too much memory")));
        assert_eq!(r.sys.heap[tag as usize], (0, 0), "attribution cleared");
        assert_eq!(r.d.running_app(), Some("calculator"));
    }

    #[test]
    fn memory_pressure_never_stops_the_foreground_app() {
        let mut r = Rig::new();
        r.launch("clock");
        r.launch("calculator");
        r.sys.free = (1024, 1 << 30);
        r.idle(5000);
        assert_eq!(r.state_of("Clock"), None);
        assert_eq!(r.state_of("Calculator"), Some(AppState::Foreground));
    }

    struct Demo;
    impl App for Demo {
        fn update(&mut self, ui: &mut Ui, area: Rect, _sys: &mut dyn System) {
            ui.fill(area, Color::BLACK);
        }
    }
    fn demo_icon(c: &mut Canvas, r: Rect) {
        c.fill_rect(r, Color::WHITE);
    }
    fn demo_create() -> Box<dyn App> {
        Box::new(Demo)
    }
    static DEMOS: [AppInfo; 2] = [
        AppInfo { id: "demo-a", name: "Demo A", icon: demo_icon, create: demo_create, background: Capabilities::NONE },
        AppInfo { id: "demo-b", name: "Demo B", icon: demo_icon, create: demo_create, background: Capabilities::NONE },
    ];

    fn with_examples() -> Rig {
        let mut r = Rig::new();
        r.d.register_group("Demos", &DEMOS);
        r.frame();
        r
    }

    #[test]
    fn examples_have_their_own_page() {
        let d = Desktop::new();
        let pages = d.catalogue.pages(12);
        let ex = pages.iter().find(|p| p.group == "Examples").expect("an Examples page");
        let ids: Vec<&str> = ex.apps.iter().map(|a| a.id).collect();
        assert_eq!(ids, ["example-hello", "example-laps", "example-weather"]);
        assert!(pages.iter().filter(|p| p.group == launcher::MAIN).all(|p| p.apps.iter().all(|a| !a.id.starts_with("example"))));
    }

    #[test]
    fn page_swipe_moves_between_pages_and_icons_still_tap() {
        let mut r = with_examples();
        assert_eq!(r.d.home_page(), 0);
        let last = r.d.catalogue.pages(12).len() - 1;
        for page in 1..=last {
            r.swipe((400, 400), (80, 400), 0);
            r.idle(300);
            assert_eq!(r.d.home_page(), page);
        }
        // The first slot of the Examples page.
        r.tap(80, STATUS_H + 40 + 50);
        r.frame();
        assert_eq!(r.d.running_app(), Some("demo-a"));
        // The page is kept while the app runs and after going home.
        r.swipe((240, 795), (240, 600), 0);
        r.idle(300);
        assert_eq!(r.d.home_page(), last);
        r.swipe((80, 400), (400, 400), 0);
        assert_eq!(r.d.home_page(), last - 1);
    }

    #[test]
    fn short_slow_page_drag_snaps_back_and_does_not_tap() {
        let mut r = with_examples();
        r.touch(300, 200, true);
        for x in (220..300).rev().step_by(10) {
            r.sys.advance(40);
            r.touch(x, 200, true);
        }
        r.touch(220, 200, false);
        r.idle(400);
        assert_eq!(r.d.home_page(), 0);
        assert_eq!(r.d.running_app(), None, "a drag is not a tap");
    }

    #[test]
    fn edge_back_and_forward_turn_home_pages() {
        let mut r = with_examples();
        r.swipe((2, 400), (200, 400), 0);
        assert_eq!(r.d.home_page(), 0, "Back on the first page does nothing");
        r.swipe((478, 400), (280, 400), 0);
        assert_eq!(r.d.home_page(), 1, "Forward goes to the next page");
        r.swipe((2, 400), (200, 400), 0);
        assert_eq!(r.d.home_page(), 0, "Back goes to the previous page");
        assert_eq!(r.d.running_app(), None);
    }

    #[test]
    fn usage_lists_running_apps() {
        let mut r = Rig::new();
        r.launch("calculator");
        r.sys.heap[r.d.running[0].tag as usize] = (4096, 0);
        r.idle(1100);
        let u = r.d.app_usage();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].name, "Calculator");
        assert_eq!(u[0].sram, 4096);
        assert_eq!(u[0].state, AppState::Foreground);
    }
}
