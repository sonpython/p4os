//! The seam for video playback.
//!
//! The app opens every video through [`open`]. Videos play on the board
//! with the `p4os-ytplay` engine ([`OnDevice`]); an id that engine cannot
//! use falls back to [`Unavailable`], which offers the video as a QR code
//! for a phone.

use super::api::{watch_url, Video};
use super::qr::Qr;
use alloc::boxed::Box;
use alloc::string::String;
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{Rect, System, Ui};

pub trait Player {
    /// Draw the player into `area` and react to input.
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System);

    /// Frame interval wanted while the player is shown (`None`: only on
    /// input), e.g. the video frame rate.
    fn tick_interval_ms(&self) -> Option<u32> {
        None
    }

    /// The player is being closed: stop audio, free buffers.
    fn close(&mut self, _sys: &mut dyn System) {}

    /// Still playing: the app asks for background audio.
    fn playing(&self) -> bool {
        false
    }

    /// Keep playing without drawing (in the background, or while the screen
    /// is locked or off; see `App::background_tick`).
    fn background(&mut self, _sys: &mut dyn System) -> Option<u32> {
        None
    }

    /// Video is playing: keep the screen on.
    fn keep_awake(&self) -> bool {
        false
    }
}

/// Create the player for `video`.
pub fn open(video: &Video) -> Box<dyn Player> {
    match p4os_ytplay::player::parse_video_id(&video.id) {
        Some(id) => Box::new(OnDevice { view: p4os_ytplay::app::PlayerView::default(), start: Some(id) }),
        None => Box::new(Unavailable::new(video)),
    }
}

/// Plays the video on the board (InnerTube streams, H.264 + Opus).
pub struct OnDevice {
    view: p4os_ytplay::app::PlayerView,
    /// Video to start on the first update (opening needs the system).
    start: Option<String>,
}

impl Player for OnDevice {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        if let Some(id) = self.start.take() {
            self.view.open(&id, p4os_ytplay::app::DEFAULT_MODE, sys);
        }
        // The app's own Back button leaves the player; the view's landscape
        // "Back" is the same request and needs no extra handling here.
        let _ = self.view.update(ui, area, sys);
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        Some(self.view.tick_interval_ms())
    }

    fn close(&mut self, sys: &mut dyn System) {
        self.view.close(sys);
    }

    fn playing(&self) -> bool {
        self.view.playing()
    }

    fn background(&mut self, sys: &mut dyn System) -> Option<u32> {
        self.view.background(sys)
    }

    fn keep_awake(&self) -> bool {
        self.view.keep_awake()
    }
}

/// Explains that playback is not available and shows the video's link.
pub struct Unavailable {
    title: String,
    link: String,
    qr: Option<Qr>,
}

impl Unavailable {
    pub fn new(video: &Video) -> Self {
        let link = watch_url(&video.id);
        Self { title: video.title.clone(), qr: Qr::encode(&link), link }
    }
}

impl Player for Unavailable {
    fn update(&mut self, ui: &mut Ui, area: Rect, _sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, t.bg);
        let r = area.inset(20);
        let wide = r.w > r.h;
        let qr_side = if wide { r.h.min(r.w / 2) - 20 } else { r.w.min(r.h / 2) };
        let (qr_rect, text) = if wide {
            (Rect::new(r.x, r.y + (r.h - qr_side) / 2, qr_side, qr_side), Rect::new(r.x + qr_side + 24, r.y, r.w - qr_side - 24, r.h))
        } else {
            let text_h = r.h - qr_side - 16;
            (Rect::new(r.x + (r.w - qr_side) / 2, r.y + text_h + 16, qr_side, qr_side), Rect::new(r.x, r.y, r.w, text_h))
        };
        let (bold, body, small) = (ui.fonts.bold, ui.fonts.body, ui.fonts.small);
        let mut y = text.y;
        ui.text(Rect::new(text.x, y, text.w, bold.line_height), "Playback not available yet", &bold, t.text, Align::Left);
        y += bold.line_height + 10;
        for line in body.wrap(&self.title, text.w).iter().take(3) {
            ui.text(Rect::new(text.x, y, text.w, body.line_height), line, &body, t.text_dim, Align::Left);
            y += body.line_height;
        }
        y += 12;
        let hint = "Videos cannot be played on this device yet. Scan the code to watch on your phone:";
        for line in small.wrap(hint, text.w) {
            ui.text(Rect::new(text.x, y, text.w, small.line_height), &line, &small, t.text_dim, Align::Left);
            y += small.line_height;
        }
        y += 8;
        ui.text(Rect::new(text.x, y, text.w, body.line_height), &self.link, &body, t.accent, Align::Left);
        if let Some(qr) = &self.qr {
            qr.draw(ui.canvas, qr_rect);
        }
    }
}
