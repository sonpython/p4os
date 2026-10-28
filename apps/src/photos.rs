//! Photo gallery: a grid of thumbnails for the JPEG/BMP files in
//! `/sd/Pictures` (or the card root), and a full-screen viewer with swipe,
//! arrow-button and arrow-key navigation.
//!
//! Decoding is the expensive part, so it happens at most once per frame:
//! thumbnails load one by one on ticks, and a full-size image is decoded on
//! the frame after its placeholder has been shown.

use crate::files::{self, ellipsize, error_view, extension, format_size, join_path};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use p4os_sdk::gui::ui::{Align, Key};
use p4os_sdk::{App, AppInfo, Canvas, Color, FsError, Image, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "photos", name: "Photos", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Photos::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0xfdcb6e), Color::hex(0xe17055));
    let p = r.inset(r.w / 5);
    c.fill_round_rect(p, r.w / 14, Color::hex(0x74b9ff));
    c.fill_circle(Point::new(p.right() - p.w / 4, p.y + p.h / 4), p.w as f32 / 9.0, Color::hex(0xffeaa7));
    let base = p.bottom() - 1;
    c.fill_triangle([Point::new(p.x, base), Point::new(p.x + p.w * 2 / 5, p.y + p.h * 2 / 5), Point::new(p.x + p.w * 4 / 5, base)], Color::hex(0x00b894));
    c.fill_triangle([Point::new(p.x + p.w * 2 / 5, base), Point::new(p.x + p.w * 7 / 10, p.y + p.h * 3 / 5), Point::new(p.right(), base)], Color::hex(0x55efc4));
}

const PICTURES: &str = "/sd/Pictures";
const ROOT: &str = "/sd";
/// Cached thumbnails are square at this size and scaled into grid cells.
const THUMB: i32 = 192;
const TARGET_CELL: i32 = 180;
const GAP: i32 = 12;
const CAPTION_H: i32 = 30;
const BAR_H: i32 = 64;

pub fn is_image(name: &str) -> bool {
    matches!(extension(name).as_str(), "jpg" | "jpeg" | "bmp")
}

struct Photo {
    path: String,
    name: String,
    size: u64,
}

enum Thumb {
    Ready(Vec<Color>),
    Failed,
}

/// Full-size image decoded for one photo at one view size.
struct Full {
    index: usize,
    w: i32,
    h: i32,
    image: Option<Image>,
}

#[derive(Default)]
struct Photos {
    /// Folder the photos came from; `None` until scanned.
    dir: Option<Result<String, FsError>>,
    photos: Vec<Photo>,
    thumbs: Vec<Option<Thumb>>,
    scroll: i32,
    /// Photo shown full screen.
    view: Option<usize>,
    full: Option<Full>,
    /// The loading placeholder for `view` has been drawn; decode next frame.
    full_requested: bool,
    /// State changed after this frame was drawn; draw again right away.
    redraw: bool,
}

impl Photos {
    fn scan(&mut self, sys: &mut dyn System) {
        self.photos.clear();
        self.thumbs.clear();
        self.scroll = 0;
        let mut found = Err(FsError::NotFound);
        for dir in [PICTURES, ROOT] {
            match sys.fs().list(dir) {
                Ok(mut list) => {
                    files::sort_entries(&mut list);
                    let images: Vec<Photo> = list
                        .into_iter()
                        .filter(|e| !e.is_dir && is_image(&e.name))
                        .map(|e| Photo { path: join_path(dir, &e.name), name: e.name, size: e.size })
                        .collect();
                    found = Ok(dir.to_string());
                    if !images.is_empty() {
                        self.photos = images;
                        break;
                    }
                }
                // Without a card there is no point trying the fallback.
                Err(FsError::NoMedium) => {
                    found = Err(FsError::NoMedium);
                    break;
                }
                Err(e) => found = Err(e),
            }
        }
        self.thumbs = self.photos.iter().map(|_| None).collect();
        self.dir = Some(found);
    }

    fn open(&mut self, index: usize) {
        self.view = Some(index);
        self.full_requested = false;
    }

    fn step(&mut self, delta: i32) {
        if let Some(i) = self.view {
            self.open(wrap_index(i, delta, self.photos.len()));
        }
    }

    fn decode(&self, sys: &mut dyn System, index: usize, max_w: i32, max_h: i32) -> Option<Image> {
        let data = sys.fs().read(&self.photos[index].path).ok()?;
        sys.decode_image(&data, max_w, max_h)
    }

    /// Decode one missing thumbnail, preferring those on screen.
    fn load_one_thumb(&mut self, sys: &mut dyn System, visible: core::ops::Range<usize>) {
        let Some(i) = next_missing(&self.thumbs, visible) else { return };
        // Decode at up to twice the thumb size; the integer down-scaler
        // then still leaves enough pixels for the crop.
        let thumb = match self.decode(sys, i, THUMB * 2, THUMB * 2) {
            Some(img) if img.width > 0 && img.height > 0 => Thumb::Ready(cover(&img, THUMB, THUMB)),
            _ => Thumb::Failed,
        };
        self.thumbs[i] = Some(thumb);
    }
}

impl App for Photos {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        self.redraw = false;
        if self.dir.is_none() {
            self.scan(sys);
        }
        if self.view.is_some() {
            self.viewer(ui, area, sys);
            return;
        }
        ui.fill(area, t.bg);
        let dir = match self.dir.as_ref() {
            Some(Ok(d)) => d.clone(),
            Some(Err(e)) => {
                if error_view(ui, area, *e) {
                    self.dir = None;
                }
                return;
            }
            None => return,
        };
        if self.photos.is_empty() {
            let (f, s) = (ui.fonts.title, ui.fonts.body);
            let c = area.center();
            ui.text(Rect::new(area.x, c.y - 40, area.w, 44), "No photos", &f, t.text, Align::Center);
            let hint = "Copy .jpg or .bmp files to Pictures on the SD card.";
            ui.text(Rect::new(area.x, c.y + 10, area.w, 30), hint, &s, t.text_dim, Align::Center);
            if ui.button_primary(Rect::new(c.x - 90, c.y + 60, 180, 52), "Refresh") {
                self.dir = None;
            }
            return;
        }
        let visible = self.grid(ui, area, &dir);
        self.load_one_thumb(sys, visible);
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        let thumbs_pending = self.view.is_none() && self.thumbs.iter().any(|t| t.is_none());
        let full_pending = self.view.is_some() && self.full.as_ref().is_none_or(|f| Some(f.index) != self.view);
        if self.redraw {
            Some(0)
        } else {
            (thumbs_pending || full_pending).then_some(16)
        }
    }
}

impl Photos {
    /// Draw the thumbnail grid; returns the range of visible photo indices.
    fn grid(&mut self, ui: &mut Ui, area: Rect, dir: &str) -> core::ops::Range<usize> {
        let t = *ui.theme;
        let inner = area.inset(16);
        let (header, body) = inner.split_top(BAR_H);
        let (title_f, small) = (ui.fonts.title, ui.fonts.small);
        let name = dir.rsplit('/').next().unwrap_or(dir);
        let title = if dir == ROOT { "SD card" } else { name };
        ui.text(Rect::new(header.x, header.y, header.w, 40), title, &title_f, t.text, Align::Left);
        let loaded = self.thumbs.iter().filter(|t| t.is_some()).count();
        let mut sub = format!("{} photos  ·  {}", self.photos.len(), dir);
        if loaded < self.photos.len() {
            sub.push_str(&format!("  ·  loading {}/{}", loaded, self.photos.len()));
        }
        ui.text(Rect::new(header.x, header.y + 38, header.w - 70, 22), &sub, &small, t.text_dim, Align::Left);
        let refresh = Rect::new(header.right() - 56, header.y, 56, 56);
        ui.canvas.fill_round_rect(refresh, t.radius, t.surface);
        if ui.button(refresh, "↻") {
            self.dir = None;
            self.redraw = true;
            return 0..0;
        }

        let cols = grid_columns(body.w);
        let cell = (body.w - GAP * (cols - 1)) / cols;
        let row_h = cell + CAPTION_H + GAP;
        let rows = (self.photos.len() as i32 + cols - 1) / cols;
        let view = Rect::new(body.x, body.y + 8, body.w, body.h - 8);
        let off = ui.scroll(view, rows * row_h, &mut self.scroll);
        let first_row = off / row_h;
        let last_row = (off + view.h) / row_h;
        let visible = (first_row * cols) as usize..(((last_row + 1) * cols) as usize).min(self.photos.len());

        let old = ui.canvas.set_clip(view.intersect(&ui.canvas.clip()));
        let mut tapped = None;
        for i in visible.clone() {
            let (col, row) = (i as i32 % cols, i as i32 / cols);
            let r = Rect::new(view.x + col * (cell + GAP), view.y + row * row_h - off, cell, cell);
            let (held, clicked) = ui.interact(r.intersect(&view));
            if clicked {
                tapped = Some(i);
            }
            match &self.thumbs[i] {
                Some(Thumb::Ready(px)) => ui.canvas.blit_scaled(r, px, THUMB, THUMB),
                Some(Thumb::Failed) => {
                    ui.canvas.fill_rect(r, t.surface);
                    ui.text(r, "Cannot open", &small, t.danger, Align::Center);
                }
                None => {
                    ui.canvas.fill_rect(r, t.surface);
                    files::draw_file(ui.canvas, Rect::new(r.x + r.w / 2 - 20, r.y + r.h / 2 - 26, 40, 52), files::FileKind::Image);
                }
            }
            if held {
                ui.canvas.fill_rect_alpha(r, Color::BLACK, 90);
            }
            let caption = ellipsize(&small, &self.photos[i].name, cell - 8);
            ui.text(Rect::new(r.x + 2, r.bottom() + 4, cell - 4, CAPTION_H - 6), &caption, &small, t.text_dim, Align::Left);
        }
        ui.canvas.restore_clip(old);
        if let Some(i) = tapped {
            self.open(i);
        }
        visible
    }

    fn viewer(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, Color::BLACK);
        let (top, rest) = area.split_top(BAR_H);
        let (stage, bottom) = rest.split_top(rest.h - BAR_H - 8);

        for k in &ui.input.keys {
            match k {
                Key::Left => self.step(-1),
                Key::Right => self.step(1),
                Key::Up | Key::Down => {
                    self.view = None;
                    self.redraw = true;
                }
                _ => {}
            }
        }

        // Horizontal swipe on the image moves to the neighbouring photo.
        let i = ui.input;
        let mut dx = 0;
        if stage.contains(i.press_pos) && i.is_drag() && (i.down || i.released) {
            dx = i.pos.x - i.press_pos.x;
            if i.released {
                if dx.abs() > stage.w / 6 {
                    self.step(if dx < 0 { 1 } else { -1 });
                }
                dx = 0;
            }
        }
        // Keys or a swipe may have changed (or closed) the view.
        let Some(index) = self.view else { return };

        // Decode on the frame after the placeholder was shown, so the tap
        // that opened the photo gets visible feedback first.
        let fresh = self.full.as_ref().is_some_and(|f| f.index == index && f.w == stage.w && f.h == stage.h);
        if !fresh {
            if self.full_requested {
                let image = self.decode(sys, index, stage.w, stage.h);
                self.full = Some(Full { index, w: stage.w, h: stage.h, image });
            } else {
                self.full_requested = true;
            }
        }

        let old = ui.canvas.set_clip(stage.intersect(&ui.canvas.clip()));
        let small = ui.fonts.body;
        match self.full.as_ref().filter(|f| f.index == index) {
            Some(Full { image: Some(img), .. }) => {
                let r = fit_rect(img.width, img.height, stage);
                ui.canvas.blit_scaled(Rect::new(r.x + dx, r.y, r.w, r.h), &img.pixels, img.width, img.height);
            }
            Some(Full { image: None, .. }) => {
                ui.text(stage, "This image cannot be decoded", &small, t.danger, Align::Center);
            }
            None => {
                if let Some(Thumb::Ready(px)) = &self.thumbs[index] {
                    let side = stage.w.min(stage.h);
                    let r = Rect::new(stage.x + (stage.w - side) / 2 + dx, stage.y + (stage.h - side) / 2, side, side);
                    ui.canvas.blit_scaled(r, px, THUMB, THUMB);
                    ui.canvas.fill_rect_alpha(r, Color::BLACK, 120);
                }
                ui.text(stage, "Loading…", &small, t.text, Align::Center);
            }
        }
        ui.canvas.restore_clip(old);

        // Top bar: back to the grid and the position in the album.
        let back = Rect::new(top.x + 12, top.y + 4, 140, 56);
        if ui.button(back, "← Grid") {
            self.view = None;
            self.redraw = true;
        }
        let (photo_name, photo_size) = (self.photos[index].name.clone(), self.photos[index].size);
        let body = ui.fonts.body;
        let pos = format!("{} / {}", index + 1, self.photos.len());
        ui.text(Rect::new(top.x, top.y, top.w - 20, top.h), &pos, &body, t.text, Align::Right);

        // Bottom bar: previous / caption / next.
        let prev = Rect::new(bottom.x + 12, bottom.y + 4, 72, 56);
        let next = Rect::new(bottom.right() - 84, bottom.y + 4, 72, 56);
        if ui.button(prev, "◀") {
            self.step(-1);
        }
        if ui.button(next, "▶") {
            self.step(1);
        }
        let cap_r = Rect::new(prev.right() + 12, bottom.y, next.x - prev.right() - 24, bottom.h);
        let small = ui.fonts.small;
        let name = ellipsize(&body, &photo_name, cap_r.w);
        ui.text(Rect::new(cap_r.x, cap_r.y + 6, cap_r.w, 28), &name, &body, t.text, Align::Center);
        let info = match self.full.as_ref().filter(|f| f.index == index).and_then(|f| f.image.as_ref()) {
            Some(img) => format!("{}  ·  shown at {}×{}", format_size(photo_size), img.width, img.height),
            None => format_size(photo_size),
        };
        ui.text(Rect::new(cap_r.x, cap_r.y + 36, cap_r.w, 22), &info, &small, t.text_dim, Align::Center);
    }
}

/// Grid columns for a given width: about `TARGET_CELL` px per cell, at least 2.
fn grid_columns(width: i32) -> i32 {
    ((width + GAP) / (TARGET_CELL + GAP)).max(2)
}

fn wrap_index(i: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (i as i64 + delta as i64).rem_euclid(len as i64) as usize
}

/// The first missing thumbnail in `visible`, else the first anywhere.
fn next_missing(thumbs: &[Option<Thumb>], visible: core::ops::Range<usize>) -> Option<usize> {
    let missing = |i: &usize| thumbs.get(*i).is_some_and(|t| t.is_none());
    visible.clone().find(missing).or_else(|| (0..thumbs.len()).find(missing))
}

/// Largest rectangle with the image's aspect ratio centred in `area`.
fn fit_rect(w: i32, h: i32, area: Rect) -> Rect {
    if w <= 0 || h <= 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let (fw, fh) = if w as i64 * area.h as i64 > h as i64 * area.w as i64 {
        (area.w, (h as i64 * area.w as i64 / w as i64) as i32)
    } else {
        ((w as i64 * area.h as i64 / h as i64) as i32, area.h)
    };
    Rect::new(area.x + (area.w - fw) / 2, area.y + (area.h - fh) / 2, fw, fh)
}

/// Centre-crop `img` to the aspect of `w`×`h` and resample it to that size.
fn cover(img: &Image, w: i32, h: i32) -> Vec<Color> {
    let (sw, sh) = (img.width as i64, img.height as i64);
    let (cw, ch) = if sw * h as i64 > sh * w as i64 { (sh * w as i64 / h as i64, sh) } else { (sw, sw * h as i64 / w as i64) };
    let (x0, y0) = ((sw - cw) / 2, (sh - ch) / 2);
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h as i64 {
        let sy = y0 + y * ch / h as i64;
        for x in 0..w as i64 {
            let sx = x0 + x * cw / w as i64;
            out.push(img.pixels[(sy * sw + sx) as usize]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_names() {
        assert!(is_image("a.JPG"));
        assert!(is_image("b.jpeg"));
        assert!(is_image("c.bmp"));
        assert!(!is_image("d.png"));
        assert!(!is_image("jpg"));
    }

    #[test]
    fn columns_follow_width() {
        assert_eq!(grid_columns(568), 2.max((568 + GAP) / (TARGET_CELL + GAP)));
        assert_eq!(grid_columns(568), 3);
        assert_eq!(grid_columns(992), 5);
        assert_eq!(grid_columns(100), 2);
    }

    #[test]
    fn wrapping_navigation() {
        assert_eq!(wrap_index(0, -1, 5), 4);
        assert_eq!(wrap_index(4, 1, 5), 0);
        assert_eq!(wrap_index(2, 1, 5), 3);
        assert_eq!(wrap_index(0, 1, 0), 0);
    }

    #[test]
    fn fit_keeps_aspect() {
        let area = Rect::new(0, 0, 600, 800);
        assert_eq!(fit_rect(3000, 2000, area), Rect::new(0, 200, 600, 400));
        assert_eq!(fit_rect(800, 1200, Rect::new(0, 0, 1000, 600)), Rect::new(300, 0, 400, 600));
        // Small images are scaled up to fill the area.
        assert_eq!(fit_rect(100, 100, Rect::new(10, 10, 200, 300)), Rect::new(10, 60, 200, 200));
    }

    #[test]
    fn cover_crops_centre() {
        // 4x2 image: left half black, right half white, with a red centre pair.
        let (b, w, r) = (Color::BLACK, Color::WHITE, Color::rgb(255, 0, 0));
        let img = Image { width: 4, height: 2, pixels: alloc::vec![b, r, r, w, b, r, r, w] };
        let out = cover(&img, 2, 2);
        assert_eq!(out, alloc::vec![r, r, r, r]);
        assert_eq!(cover(&img, 4, 4).len(), 16);
    }

    #[test]
    fn visible_thumbs_load_first() {
        let thumbs = alloc::vec![None, Some(Thumb::Failed), None, None];
        assert_eq!(next_missing(&thumbs, 1..3), Some(2));
        assert_eq!(next_missing(&thumbs, 1..2), Some(0));
        let done: Vec<Option<Thumb>> = alloc::vec![Some(Thumb::Failed)];
        assert_eq!(next_missing(&done, 0..1), None);
    }
}
