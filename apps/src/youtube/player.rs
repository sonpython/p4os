//! The seam for video playback.
//!
//! The app opens every video through [`open`]. On-device playback (stream
//! selection and decoding) does not exist yet, so the only implementation
//! is [`Unavailable`], which offers the video as a QR code for a phone. A
//! real player implements [`Player`] and is returned from [`open`]; nothing
//! else in the app needs to change.

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
}

/// Create the player for `video`.
pub fn open(video: &Video) -> Box<dyn Player> {
    Box::new(Unavailable::new(video))
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
