//! The player screen: video (or storyboard / audio visual), title,
//! progress, play/pause, seek ±10 s, quality/mode switch, stats and errors.
//! Portrait shows the video letterboxed at the top; landscape takes the
//! whole screen with controls that appear on a tap.
//!
//! Apps open it with [`play`]; the console with `yt play ID`.

use crate::error::PlayError;
use crate::innertube::Quality;
use crate::player::{self, Phase, Player, Presentation};
use crate::remote::{self, Command, Mode};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use p4os_sdk::gui::ui::{Align, Key};
use p4os_sdk::{App, AppInfo, Canvas, Color, Orientation, Point, Rect, System, Ui};

pub const APP_ID: &str = "ytplayer";
pub const INFO: AppInfo = AppInfo { id: APP_ID, name: "YT Player", icon, create };

/// Default video quality, chosen from the board benchmark: 144p decodes at
/// ~70 fps on one core, 240p at ~21 fps (below the 25-30 fps of the streams).
pub const DEFAULT_MODE: Mode = Mode::Video(Quality::P144);

const SEEK_STEP_MS: i64 = 10_000;
/// Longest wait between GUI frames while video plays, and the shortest.
const VIDEO_TICK_MS: u32 = 40;
const MIN_TICK_MS: u32 = 4;
const AUDIO_TICK_MS: u32 = 100;
const IDLE_TICK_MS: u32 = 250;
/// Tick while the screen is locked or off: the queues hold seconds of data.
const BACKGROUND_TICK_MS: u32 = 100;
/// Landscape controls stay visible this long after a tap.
const OVERLAY_MS: u64 = 4000;

const YT_RED: Color = Color::hex(0xff0033);
const VIDEO_BG: Color = Color::BLACK;
const NOTICE: Color = Color::hex(0xf5c451);

/// Open the player for `video_id` (an id or a YouTube URL) from any app:
/// the desktop switches to the player screen on the next frame.
pub fn play(sys: &mut dyn System, video_id: &str) {
    play_mode(sys, video_id, DEFAULT_MODE);
}

pub fn play_mode(sys: &mut dyn System, video_id: &str, mode: Mode) {
    let id = player::parse_video_id(video_id).unwrap_or_else(|| String::from(video_id));
    remote::post(Command::Play { video_id: id, mode });
    sys.launch(APP_ID);
}

fn icon(c: &mut Canvas, r: Rect) {
    c.fill_round_rect(r, r.w / 5, YT_RED);
    let s = r.w / 4;
    let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
    c.fill_triangle([Point::new(cx - s * 2 / 3, cy - s), Point::new(cx - s * 2 / 3, cy + s), Point::new(cx + s, cy)], Color::WHITE);
}

fn create() -> Box<dyn App> {
    Box::new(PlayerApp::default())
}

/// The player as a standalone app (launcher entry `YT Player`).
#[derive(Default)]
pub struct PlayerApp {
    view: PlayerView,
}

impl App for PlayerApp {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        if self.view.update(ui, area, sys) == ViewEvent::Close {
            sys.launch("");
        }
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        Some(self.view.tick_interval_ms())
    }

    fn on_close(&mut self, sys: &mut dyn System) {
        self.view.close(sys);
    }

    fn fullscreen(&self) -> bool {
        self.view.fullscreen()
    }

    fn background(&mut self, sys: &mut dyn System) -> Option<u32> {
        self.view.background(sys)
    }

    fn keep_awake(&self) -> bool {
        self.view.keep_awake()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewEvent {
    None,
    /// The user asked to leave the player.
    Close,
}

/// The player screen, embeddable in another app: forward `update`,
/// `tick_interval_ms`, `fullscreen` and `close`.
#[derive(Default)]
pub struct PlayerView {
    player: Option<Player>,
    entry: String,
    /// Remembered so `fullscreen` (called before `update`) knows it.
    landscape: bool,
    overlay_until: u64,
    /// Nearest-neighbour column map for (source width, destination width).
    xmap: (usize, usize, Vec<u16>),
    row: Vec<Color>,
    /// Progress bar drag in progress: fraction under the finger.
    scrub: Option<f32>,
}

impl PlayerView {
    /// Start playing `video_id`, replacing whatever played before.
    pub fn open(&mut self, video_id: &str, mode: Mode, sys: &mut dyn System) {
        self.close(sys);
        self.player = Some(Player::new(video_id, mode, sys));
        self.overlay_until = sys.uptime_ms() + OVERLAY_MS;
    }

    pub fn close(&mut self, sys: &mut dyn System) {
        if let Some(mut p) = self.player.take() {
            p.close(sys);
        }
    }

    pub fn fullscreen(&self) -> bool {
        self.landscape && self.player.is_some()
    }

    /// Keep playing without drawing (screen locked or off). Audio carries
    /// on; video pictures are consumed but not shown.
    pub fn background(&mut self, sys: &mut dyn System) -> Option<u32> {
        self.handle_commands(sys);
        let p = self.player.as_mut()?;
        p.tick(sys);
        p.is_active().then_some(BACKGROUND_TICK_MS)
    }

    /// Video is playing: the screen should stay on.
    pub fn keep_awake(&self) -> bool {
        self.player.as_ref().is_some_and(|p| {
            p.phase == Phase::Playing && !p.paused && matches!(p.presentation(), Some(Presentation::Video { .. }))
        })
    }

    pub fn tick_interval_ms(&self) -> u32 {
        match &self.player {
            Some(p) if p.is_active() => match p.presentation() {
                // Redraw when the next picture is due, not on a fixed beat:
                // every GUI frame repaints the whole screen.
                Some(Presentation::Video { .. }) if !p.paused => match p.next_frame_in_ms() {
                    Some(ms) => (ms as u32).clamp(MIN_TICK_MS, VIDEO_TICK_MS),
                    None => VIDEO_TICK_MS,
                },
                _ => AUDIO_TICK_MS,
            },
            _ => IDLE_TICK_MS,
        }
    }

    fn handle_commands(&mut self, sys: &mut dyn System) {
        while let Some(cmd) = remote::take() {
            match cmd {
                Command::Play { video_id, mode } => self.open(&video_id, mode, sys),
                Command::Seek(s) => {
                    if let Some(p) = &mut self.player {
                        p.seek_by(s as i64 * 1000, sys);
                    }
                }
                Command::TogglePause => {
                    if let Some(p) = &mut self.player {
                        p.toggle_pause(sys);
                    }
                }
                Command::Stop => self.close(sys),
            }
        }
    }

    pub fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) -> ViewEvent {
        self.landscape = sys.orientation() == Orientation::Landscape;
        self.handle_commands(sys);
        if let Some(p) = &mut self.player {
            p.tick(sys);
        }
        for k in &ui.input.keys {
            if let Some(p) = &mut self.player {
                match k {
                    Key::Char(' ') => p.toggle_pause(sys),
                    Key::Left => p.seek_by(-SEEK_STEP_MS, sys),
                    Key::Right => p.seek_by(SEEK_STEP_MS, sys),
                    _ => {}
                }
            }
        }
        if self.player.is_none() {
            ui.fill(area, ui.theme.bg);
            self.entry_screen(ui, area, sys);
            return ViewEvent::None;
        }
        let t = sys.uptime_ms();
        let ev = if self.landscape { self.landscape_screen(ui, area, sys) } else { self.portrait_screen(ui, area, sys) };
        if let Some(p) = &mut self.player {
            p.stats.draw_ms += sys.uptime_ms() - t;
            p.stats.draws += 1;
        }
        ev
    }

    /// No video yet (opened from the launcher): ask for an id or URL.
    fn entry_screen(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let r = area.inset(20);
        let (bold, small) = (ui.fonts.bold, ui.fonts.small);
        let (text, dim) = (ui.theme.text, ui.theme.text_dim);
        ui.text(Rect::new(r.x, r.y, r.w, 40), "Play a YouTube video", &bold, text, Align::Left);
        ui.text(Rect::new(r.x, r.y + 40, r.w, 28), "Video id or link (youtube.com/watch?v=..., youtu.be/...)", &small, dim, Align::Left);
        let field = Rect::new(r.x, r.y + 76, r.w, 56);
        let mut submit = ui.text_field(field, &mut self.entry, "dQw4w9WgXcQ");
        let half = (r.w - 12) / 2;
        submit |= ui.button_primary(Rect::new(r.x, field.bottom() + 16, half, 60), "Play");
        let audio = ui.button(Rect::new(r.x + half + 12, field.bottom() + 16, half, 60), "Audio only");
        if submit || audio {
            let mode = if audio { Mode::Audio } else { DEFAULT_MODE };
            match player::parse_video_id(&self.entry) {
                Some(id) => self.open(&id, mode, sys),
                None => self.entry.clear(),
            }
        }
        ui.text(Rect::new(r.x, field.bottom() + 92, r.w, 28), "Personal use; no Google account is used.", &small, dim, Align::Left);
    }

    fn portrait_screen(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) -> ViewEvent {
        let video = Rect::new(area.x, area.y, area.w, area.w * 9 / 16);
        // Each pixel is written once: the video area paints itself.
        ui.fill(Rect::new(area.x, video.bottom(), area.w, area.bottom() - video.bottom()), ui.theme.bg);
        self.draw_media(ui, video, sys);
        let p = self.player.as_ref().expect("checked by update");
        let mut y = video.bottom() + 10;
        let r = Rect::new(area.x + 14, y, area.w - 28, area.bottom() - y);
        let (bold, small, body) = (ui.fonts.bold, ui.fonts.small, ui.fonts.body);
        let (text, dim) = (ui.theme.text, ui.theme.text_dim);
        let title = p.info.as_ref().map_or_else(|| format!("Loading {}...", p.video_id), |i| i.title.clone());
        for line in bold.wrap(&title, r.w).into_iter().take(2) {
            ui.text(Rect::new(r.x, y, r.w, 30), &line, &bold, text, Align::Left);
            y += 30;
        }
        if let Some(i) = &p.info {
            ui.text(Rect::new(r.x, y, r.w, 24), &i.author, &small, dim, Align::Left);
        }
        y += 30;
        let ev = self.transport(ui, Rect::new(r.x, y, r.w, 150), sys);
        y += 160;
        y = self.mode_row(ui, Rect::new(r.x, y, r.w, 52), sys) + 10;
        let p = self.player.as_ref().expect("checked by update");
        ui.text(Rect::new(r.x, y, r.w, 24), &indicator(p), &body, text, Align::Left);
        y += 30;
        if let Some(n) = &p.notice {
            for line in small.wrap(n, r.w).into_iter().take(2) {
                ui.text(Rect::new(r.x, y, r.w, 22), &line, &small, NOTICE, Align::Left);
                y += 22;
            }
        }
        if let Phase::Failed(e) = &p.phase {
            let e = e.clone();
            self.error_box(ui, Rect::new(r.x, y + 4, r.w, (r.bottom() - y - 8).min(190)), &e, sys);
        }
        ev
    }

    fn landscape_screen(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) -> ViewEvent {
        let now = sys.uptime_ms();
        self.draw_media(ui, area, sys);
        // A tap that is not on a visible control toggles the controls.
        let shown = now < self.overlay_until;
        if ui.input.tapped_in(area) && !shown {
            self.overlay_until = now + OVERLAY_MS;
            return ViewEvent::None;
        }
        let failed = self.player.as_ref().and_then(|p| if let Phase::Failed(e) = &p.phase { Some(e.clone()) } else { None });
        if !shown && failed.is_none() && !self.player.as_ref().is_some_and(|p| p.buffering || p.paused) {
            return ViewEvent::None;
        }
        if ui.input.pressed || ui.input.down {
            self.overlay_until = now + OVERLAY_MS;
        }
        let top = Rect::new(area.x, area.y, area.w, 56);
        ui.canvas.fill_rect_alpha(top, Color::BLACK, 150);
        let mut ev = ViewEvent::None;
        if ui.button(Rect::new(top.x + 8, top.y + 6, 110, 44), "Back") {
            ev = ViewEvent::Close;
        }
        if ui.button(Rect::new(top.right() - 128, top.y + 6, 120, 44), "Portrait") {
            sys.set_orientation(Orientation::Portrait);
        }
        let p = self.player.as_ref().expect("checked by update");
        let title = p.info.as_ref().map_or_else(|| p.video_id.clone(), |i| i.title.clone());
        let bold = ui.fonts.bold;
        let text = ui.theme.text;
        ui.text(Rect::new(top.x + 130, top.y, top.w - 270, top.h), &title, &bold, text, Align::Center);
        let bottom = Rect::new(area.x, area.bottom() - 150, area.w, 150);
        ui.canvas.fill_rect_alpha(bottom, Color::BLACK, 150);
        let inner = Rect::new(bottom.x + 20, bottom.y + 6, bottom.w - 40, 140);
        if self.transport(ui, inner, sys) == ViewEvent::Close {
            ev = ViewEvent::Close;
        }
        let p = self.player.as_ref().expect("checked by update");
        let small = ui.fonts.small;
        ui.text(Rect::new(inner.x, inner.bottom() - 22, inner.w, 22), &indicator(p), &small, text, Align::Right);
        if let Some(e) = failed {
            let r = Rect::new(area.x + 100, area.y + 90, area.w - 200, 190);
            self.error_box(ui, r, &e, sys);
        }
        ev
    }

    /// Progress bar with times, then the seek / play-pause buttons.
    fn transport(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) -> ViewEvent {
        let now = sys.uptime_ms();
        let p = self.player.as_mut().expect("checked by update");
        let dur = p.duration_ms().max(1);
        let pos = p.clock_ms(now).min(dur);
        let small = ui.fonts.small;
        let dim = ui.theme.text_dim;
        // Progress: drag to choose a position, seek on release.
        let bar = Rect::new(r.x, r.y, r.w, 36);
        let i = ui.input;
        if i.down && bar.contains(i.press_pos) {
            self.scrub = Some(((i.pos.x - bar.x) as f32 / bar.w as f32).clamp(0.0, 1.0));
        } else if let Some(f) = self.scrub.take() {
            p.seek_to((f * dur as f32) as u64, sys);
        }
        let frac = self.scrub.unwrap_or(pos as f32 / dur as f32);
        ui.progress(Rect::new(bar.x, bar.y + 14, bar.w, 8), frac);
        ui.canvas.fill_circle(Point::new(bar.x + (bar.w as f32 * frac) as i32, bar.y + 18), 9.0, Color::WHITE);
        let shown_pos = self.scrub.map_or(pos, |f| (f * dur as f32) as u64);
        ui.text(Rect::new(r.x, bar.bottom(), 120, 22), &clock_text(shown_pos), &small, dim, Align::Left);
        ui.text(Rect::new(r.right() - 120, bar.bottom(), 120, 22), &clock_text(dur), &small, dim, Align::Right);
        let by = bar.bottom() + 28;
        let bw = 120;
        let gap = (r.w - 3 * bw) / 2;
        let back = Rect::new(r.x, by, bw, 64);
        let play = Rect::new(r.x + bw + gap, by, bw, 64);
        let fwd = Rect::new(r.x + 2 * (bw + gap), by, bw, 64);
        let playing = !p.paused && p.phase == Phase::Playing;
        if ui.icon_button(back, |c, b, col| seek_icon(c, b, col, false)) {
            p.seek_by(-SEEK_STEP_MS, sys);
        }
        let t = *ui.theme;
        ui.canvas.fill_round_rect(play, 32, t.accent);
        if ui.icon_button(play, |c, b, _| play_icon(c, b, playing)) {
            p.toggle_pause(sys);
        }
        if ui.icon_button(fwd, |c, b, col| seek_icon(c, b, col, true)) {
            p.seek_by(SEEK_STEP_MS, sys);
        }
        ViewEvent::None
    }

    /// 144p / 240p / Audio buttons; returns the y below the row.
    fn mode_row(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) -> i32 {
        let p = self.player.as_mut().expect("checked by update");
        let options = [("144p", Mode::Video(Quality::P144)), ("240p", Mode::Video(Quality::P240)), ("Audio", Mode::Audio)];
        let w = (r.w - 2 * 10) / 3;
        for (i, (label, mode)) in options.iter().enumerate() {
            let b = Rect::new(r.x + i as i32 * (w + 10), r.y, w, r.h);
            let t = *ui.theme;
            let (bg, fg) = if p.mode == *mode { (t.accent, t.accent_text) } else { (t.surface_alt, t.text) };
            if ui.button_colored(b, label, bg, fg) {
                p.set_mode(*mode, sys);
            }
        }
        r.bottom()
    }

    fn error_box(&mut self, ui: &mut Ui, r: Rect, e: &PlayError, sys: &mut dyn System) {
        let t = *ui.theme;
        ui.canvas.fill_round_rect(r, 12, t.surface);
        ui.canvas.stroke_round_rect(r, 12, 2, t.danger);
        let inner = r.inset(14);
        let (bold, small) = (ui.fonts.bold, ui.fonts.small);
        ui.text(Rect::new(inner.x, inner.y, inner.w, 28), e.headline(), &bold, t.danger, Align::Left);
        let mut y = inner.y + 32;
        let detail = format!("{}: {}", e.code(), e.detail);
        for line in small.wrap(&detail, inner.w).into_iter().take(3) {
            ui.text(Rect::new(inner.x, y, inner.w, 22), &line, &small, t.text, Align::Left);
            y += 22;
        }
        let retry = Rect::new(inner.x, inner.bottom() - 48, 140, 48);
        if ui.button(retry, "Retry")
            && let Some(p) = &self.player
        {
            let (id, mode) = (p.video_id.clone(), p.mode);
            self.open(&id, mode, sys);
        }
    }

    /// The video frame, storyboard thumbnail or audio visual in `r`.
    fn draw_media(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let now = sys.uptime_ms();
        let Some(p) = self.player.as_mut() else { return ui.fill(r, VIDEO_BG) };
        let busy = match &p.phase {
            Phase::LookingUp => Some("Looking up the video..."),
            Phase::Opening => Some("Opening streams..."),
            Phase::Playing if p.buffering => Some("Buffering..."),
            _ => None,
        };
        let mut drew = false;
        if let Some(f) = p.frame() {
            let (w, h) = (f.width, f.height);
            let dst = fit(r, w as i32, h as i32);
            fill_around(ui.canvas, r, dst, VIDEO_BG);
            blit_scaled(ui.canvas, &mut self.xmap, &mut self.row, dst, &f.pixels, w, h);
            drew = true;
        } else if let Some((img, cell)) = p.thumbnail(now) {
            let dst = fit(r, cell.2, cell.3);
            fill_around(ui.canvas, r, dst, VIDEO_BG);
            blit_cell(ui.canvas, &mut self.xmap, &mut self.row, dst, &img.pixels, img.width as usize, cell);
            drew = true;
        } else {
            ui.fill(r, VIDEO_BG);
        }
        let p = self.player.as_ref().expect("checked above");
        if !drew && matches!(p.presentation(), Some(Presentation::Audio { .. })) {
            audio_visual(ui.canvas, r, p.clock_ms(now));
        }
        let small = ui.fonts.small;
        if let Some(msg) = busy {
            let b = Rect::new(r.x + r.w / 2 - 120, r.y + r.h / 2 - 20, 240, 40);
            ui.canvas.fill_round_rect(b, 20, Color::hex(0x202020));
            ui.text(b, msg, &small, Color::WHITE, Align::Center);
        }
        if matches!(p.presentation(), Some(Presentation::Audio { .. })) {
            let b = Rect::new(r.x + 10, r.y + 10, 110, 28);
            ui.canvas.fill_round_rect(b, 8, Color::hex(0x202020));
            ui.text(b, "Audio only", &small, Color::WHITE, Align::Center);
        }
    }
}

/// "H.264 144p + Opus, 24.8 fps" and similar.
fn indicator(p: &Player) -> String {
    match p.presentation() {
        Some(Presentation::Video { height, with_audio }) => {
            let audio = if with_audio { " + Opus" } else { ", no sound" };
            format!("H.264 {height}p{audio} | {:.1} fps | dropped {}", p.stats.fps, p.stats.dropped_display)
        }
        Some(Presentation::Audio { storyboard: true }) => "Opus audio + storyboard pictures".into(),
        Some(Presentation::Audio { storyboard: false }) => "Opus audio only".into(),
        None => "".into(),
    }
}

fn clock_text(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// Fill the parts of `outer` that `inner` (inside it) leaves uncovered.
fn fill_around(c: &mut Canvas, outer: Rect, inner: Rect, col: Color) {
    c.fill_rect(Rect::new(outer.x, outer.y, outer.w, inner.y - outer.y), col);
    c.fill_rect(Rect::new(outer.x, inner.bottom(), outer.w, outer.bottom() - inner.bottom()), col);
    c.fill_rect(Rect::new(outer.x, inner.y, inner.x - outer.x, inner.h), col);
    c.fill_rect(Rect::new(inner.right(), inner.y, outer.right() - inner.right(), inner.h), col);
}

/// The largest rectangle of aspect `w:h` centred in `r`.
fn fit(r: Rect, w: i32, h: i32) -> Rect {
    if w <= 0 || h <= 0 {
        return r;
    }
    let (mut dw, mut dh) = (r.w, r.w * h / w);
    if dh > r.h {
        dh = r.h;
        dw = r.h * w / h;
    }
    Rect::new(r.x + (r.w - dw) / 2, r.y + (r.h - dh) / 2, dw, dh)
}

fn column_map<'a>(cache: &'a mut (usize, usize, Vec<u16>), sw: usize, dw: usize) -> &'a [u16] {
    if cache.0 != sw || cache.1 != dw {
        *cache = (sw, dw, (0..dw).map(|x| (x * sw / dw) as u16).collect());
    }
    &cache.2
}

/// Nearest-neighbour scale of a `w` x `h` image into `dst`.
fn blit_scaled(c: &mut Canvas, cache: &mut (usize, usize, Vec<u16>), row: &mut Vec<Color>, dst: Rect, img: &[Color], w: usize, h: usize) {
    blit_cell(c, cache, row, dst, img, w, (0, 0, w as i32, h as i32));
}

/// Nearest-neighbour scale of the `cell` (x, y, w, h) of an image with
/// `stride` pixels per row into `dst`. Rows that repeat are built once.
fn blit_cell(c: &mut Canvas, cache: &mut (usize, usize, Vec<u16>), row: &mut Vec<Color>, dst: Rect, img: &[Color], stride: usize, cell: (i32, i32, i32, i32)) {
    if dst.is_empty() || cell.2 <= 0 || cell.3 <= 0 {
        return;
    }
    let (cx, cy, cw, ch) = (cell.0 as usize, cell.1 as usize, cell.2 as usize, cell.3 as usize);
    if (cy + ch - 1) * stride + cx + cw > img.len() {
        return;
    }
    let map = column_map(cache, cw, dst.w as usize);
    row.resize(dst.w as usize, Color(0));
    let mut last = usize::MAX;
    for y in 0..dst.h {
        let sy = cy + y as usize * ch / dst.h as usize;
        if sy != last {
            let src = &img[sy * stride + cx..sy * stride + cx + cw];
            for (d, &m) in row.iter_mut().zip(map.iter()) {
                *d = src[m as usize];
            }
            last = sy;
        }
        c.blit(Point::new(dst.x, dst.y + y), row, dst.w, 1);
    }
}

/// Pulsing bars for audio without pictures.
fn audio_visual(c: &mut Canvas, r: Rect, t_ms: u64) {
    c.fill_gradient(r, Color::hex(0x3a0d16), Color::hex(0x0b0f18));
    let bars = 24;
    let bw = r.w / (bars * 2);
    for i in 0..bars {
        let phase = (t_ms as f32 / 180.0) + i as f32 * 0.7;
        let h = ((libm::sinf(phase) * 0.5 + 0.5) * 0.6 + 0.1) * r.h as f32;
        let x = r.x + bw / 2 + i * 2 * bw;
        c.fill_rect(Rect::new(x, r.bottom() - h as i32 - 10, bw, h as i32), YT_RED);
    }
}

fn play_icon(c: &mut Canvas, r: Rect, playing: bool) {
    let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
    if playing {
        c.fill_rect(Rect::new(cx - 12, cy - 14, 8, 28), Color::WHITE);
        c.fill_rect(Rect::new(cx + 4, cy - 14, 8, 28), Color::WHITE);
    } else {
        c.fill_triangle([Point::new(cx - 9, cy - 15), Point::new(cx - 9, cy + 15), Point::new(cx + 15, cy)], Color::WHITE);
    }
}

/// Double chevron pointing back or forward.
fn seek_icon(c: &mut Canvas, r: Rect, col: Color, forward: bool) {
    let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
    let d = if forward { 1 } else { -1 };
    for k in 0..2 {
        let x = cx + d * (k * 12 + 2);
        c.line(Point::new(x - d * 10, cy - 12), Point::new(x, cy), 3.0, col);
        c.line(Point::new(x, cy), Point::new(x - d * 10, cy + 12), 3.0, col);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_letterboxed() {
        assert_eq!(fit(Rect::new(0, 0, 480, 270), 256, 144), Rect::new(0, 0, 480, 270));
        assert_eq!(fit(Rect::new(0, 0, 800, 480), 426, 240), Rect::new(0, 15, 800, 450));
        assert_eq!(fit(Rect::new(0, 0, 480, 270), 100, 100), Rect::new(105, 0, 270, 270));
    }

    #[test]
    fn scales_with_nearest_neighbour() {
        let img: Vec<Color> = (0..4).map(Color).collect();
        let mut buf = alloc::vec![Color(9); 16];
        let mut c = Canvas::new(&mut buf, 4, 4);
        let (mut cache, mut row) = ((0, 0, Vec::new()), Vec::new());
        blit_scaled(&mut c, &mut cache, &mut row, Rect::new(0, 0, 4, 4), &img, 2, 2);
        let px: Vec<u16> = buf.iter().map(|c| c.0).collect();
        assert_eq!(px, [0, 0, 1, 1, 0, 0, 1, 1, 2, 2, 3, 3, 2, 2, 3, 3]);
    }

    #[test]
    fn formats_times() {
        assert_eq!(clock_text(0), "0:00");
        assert_eq!(clock_text(213_040), "3:33");
        assert_eq!(clock_text(3_723_000), "1:02:03");
    }
}
