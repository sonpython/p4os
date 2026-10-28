//! File manager: browse the file system from `/`, with a breadcrumb bar,
//! back/up navigation and an info panel for files that hands off to the
//! Photos, Notes or Music app.
//!
//! The path helpers, size formatting and file-type icons here are shared
//! with the Photos and Notes apps.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use p4os_sdk::gui::font::Font;
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, DirEntry, FsError, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "files", name: "Files", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Files::new())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x5b8def), Color::hex(0x2f5fc4));
    draw_folder(c, r.inset(r.w / 5));
}

// ---------------------------------------------------------------------------
// Shared helpers (also used by Photos and Notes)

/// Join a directory and an entry name into an absolute path.
pub fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// Parent directory of an absolute path; the root is its own parent.
pub fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// Lower-case extension of a file name, without the dot ("" when none).
pub fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Folder,
    Image,
    Text,
    Audio,
    Other,
}

impl FileKind {
    pub fn of(entry: &DirEntry) -> Self {
        if entry.is_dir {
            return FileKind::Folder;
        }
        match extension(&entry.name).as_str() {
            "jpg" | "jpeg" | "bmp" => FileKind::Image,
            "txt" | "md" | "log" | "csv" => FileKind::Text,
            "mp3" | "wav" => FileKind::Audio,
            _ => FileKind::Other,
        }
    }

    fn color(self) -> Color {
        match self {
            FileKind::Folder => Color::hex(0xf5b841),
            FileKind::Image => Color::hex(0x2ecc71),
            FileKind::Text => Color::hex(0x5b8def),
            FileKind::Audio => Color::hex(0xe056a0),
            FileKind::Other => Color::hex(0x8d97a8),
        }
    }
}

/// Human-readable type description for the list and the info panel.
pub fn type_label(entry: &DirEntry) -> String {
    let ext = extension(&entry.name);
    match (FileKind::of(entry), ext.as_str()) {
        (FileKind::Folder, _) => "Folder".to_string(),
        (FileKind::Image, "bmp") => "BMP image".to_string(),
        (FileKind::Image, _) => "JPEG image".to_string(),
        (FileKind::Text, _) => "Text document".to_string(),
        (FileKind::Audio, e) => format!("{} audio", e.to_ascii_uppercase()),
        (_, "") => "File".to_string(),
        (_, e) => format!("{} file", e.to_ascii_uppercase()),
    }
}

/// "512 B", "1.5 KB", "12 KB", "3.4 MB" (binary units).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut unit = 1024u64;
    let mut i = 0;
    while i + 1 < UNITS.len() && bytes >= unit * 1024 {
        unit *= 1024;
        i += 1;
    }
    let tenths = bytes * 10 / unit;
    if tenths < 100 {
        format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[i])
    } else {
        format!("{} {}", bytes / unit, UNITS[i])
    }
}

/// Folders first, then case-insensitive by name; hidden (dot) files dropped.
pub fn sort_entries(entries: &mut Vec<DirEntry>) {
    entries.retain(|e| !e.name.starts_with('.'));
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
}

/// Title and hint shown when a folder cannot be listed.
pub fn fs_error_text(e: FsError) -> (&'static str, &'static str) {
    match e {
        FsError::NoMedium => ("Insert SD card", "No card was detected. Insert one and tap Retry."),
        FsError::NotFound => ("Not found", "This folder does not exist."),
        FsError::NotADirectory => ("Not a folder", "This path is a file, not a folder."),
        FsError::Io => ("Card read error", "The card could not be read. Try reinserting it."),
        FsError::Unsupported => ("Not supported", "This storage does not support the operation."),
    }
}

/// Shorten `text` with a trailing "…" so it fits in `width` pixels.
pub fn ellipsize(font: &Font, text: &str, width: i32) -> String {
    if font.measure(text) <= width {
        return text.to_string();
    }
    let budget = width - font.char_width('…');
    let mut out = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let cw = font.char_width(ch);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Draw a folder glyph filling `r`.
pub fn draw_folder(c: &mut Canvas, r: Rect) {
    let col = FileKind::Folder.color();
    let tab = Rect::new(r.x, r.y + r.h / 8, r.w * 2 / 5, r.h / 4);
    c.fill_round_rect(tab, (r.w / 12).max(2), col.darken(40));
    let body = Rect::new(r.x, r.y + r.h / 4, r.w, r.h * 5 / 8);
    c.fill_round_rect(body, (r.w / 10).max(2), col);
}

/// Draw a document glyph tinted by its kind, filling `r`.
pub fn draw_file(c: &mut Canvas, r: Rect, kind: FileKind) {
    let w = r.h * 3 / 4;
    let page = Rect::new(r.x + (r.w - w) / 2, r.y, w, r.h);
    let fold = w / 3;
    let paper = Color::hex(0xe8ecf3);
    // Page outline with the top-right corner cut off, then the folded flap.
    c.fill_rect(Rect::new(page.x, page.y, page.w - fold, page.h), paper);
    c.fill_rect(Rect::new(page.right() - fold, page.y + fold, fold, page.h - fold), paper);
    let (a, b, d) = (Point::new(page.right() - fold, page.y), Point::new(page.right(), page.y + fold), Point::new(page.right() - fold, page.y + fold));
    c.fill_triangle([a, b, d], Color::hex(0xb7bfcc));
    let band = Rect::new(page.x, page.bottom() - r.h / 3, page.w, r.h / 4);
    c.fill_rect(band, kind.color());
}

pub fn draw_entry_icon(c: &mut Canvas, r: Rect, kind: FileKind) {
    if kind == FileKind::Folder { draw_folder(c, r) } else { draw_file(c, r, kind) }
}

// ---------------------------------------------------------------------------
// The app

const ROW_H: i32 = 60;
const BAR_H: i32 = 56;

struct Files {
    path: String,
    /// Paths visited before the current one (for Back).
    history: Vec<String>,
    /// Listing of `path`; `None` means it still has to be read.
    entries: Option<Result<Vec<DirEntry>, FsError>>,
    scroll: i32,
    /// File whose info panel is open.
    info: Option<DirEntry>,
    /// State changed after this frame was drawn; draw again right away.
    redraw: bool,
}

impl Files {
    fn new() -> Self {
        Self { path: "/".to_string(), history: Vec::new(), entries: None, scroll: 0, info: None, redraw: false }
    }

    fn go(&mut self, path: String) {
        if path != self.path {
            let old = core::mem::replace(&mut self.path, path);
            self.history.push(old);
        }
        self.reload();
    }

    fn back(&mut self) {
        if let Some(p) = self.history.pop() {
            self.path = p;
            self.reload();
        }
    }

    fn reload(&mut self) {
        self.entries = None;
        self.scroll = 0;
        self.info = None;
        self.redraw = true;
    }
}

/// Breadcrumb segments of `path` as (label, target path); the root is "Root".
fn crumbs(path: &str) -> Vec<(String, String)> {
    let mut out = alloc::vec![("Root".to_string(), "/".to_string())];
    let mut acc = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        out.push((part.to_string(), acc.clone()));
    }
    out
}

impl App for Files {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, t.bg);
        self.redraw = false;
        if self.entries.is_none() {
            let mut res = sys.fs().list(&self.path);
            if let Ok(list) = &mut res {
                sort_entries(list);
            }
            self.entries = Some(res);
        }

        let modal = self.info.is_some();
        let inner = area.inset(12);
        let (bar, rest) = inner.split_top(BAR_H);
        self.toolbar(ui, bar, modal);
        let body = Rect::new(rest.x, rest.y + 10, rest.w, rest.h - 10);

        let mut open = None;
        match self.entries.as_ref() {
            Some(Ok(list)) if list.is_empty() => {
                let f = ui.fonts.large;
                ui.text(body, "This folder is empty", &f, t.text_dim, Align::Center);
            }
            Some(Ok(list)) => open = list_view(ui, body, list, &mut self.scroll, !modal),
            Some(Err(e)) => {
                if error_view(ui, body, *e) && !modal {
                    self.reload();
                }
            }
            None => {}
        }
        if let Some(i) = open {
            let entry = self.entries.as_ref().and_then(|r| r.as_ref().ok()).map(|l| l[i].clone());
            if let Some(e) = entry {
                if e.is_dir {
                    let next = join_path(&self.path, &e.name);
                    self.go(next);
                } else {
                    self.info = Some(e);
                    self.redraw = true;
                }
            }
        }

        if let Some(entry) = self.info.clone() {
            match info_panel(ui, area, &self.path, &entry) {
                Some(PanelAction::Close) => {
                    self.info = None;
                    self.redraw = true;
                }
                Some(PanelAction::Launch(id)) => sys.launch(id),
                None => {}
            }
        }
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        self.redraw.then_some(0)
    }
}

impl Files {
    fn toolbar(&mut self, ui: &mut Ui, bar: Rect, inert: bool) {
        let t = *ui.theme;
        let back_r = Rect::new(bar.x, bar.y, BAR_H, BAR_H);
        let up_r = Rect::new(back_r.right() + 8, bar.y, BAR_H, BAR_H);
        let reload_r = Rect::new(bar.right() - BAR_H, bar.y, BAR_H, BAR_H);
        let can_back = !self.history.is_empty();
        let can_up = self.path != "/";

        let tool = |ui: &mut Ui, r: Rect, enabled: bool, draw: fn(&mut Canvas, Rect, Color)| -> bool {
            ui.canvas.fill_round_rect(r, t.radius, t.surface);
            if inert {
                draw(ui.canvas, r, t.text_dim);
                return false;
            }
            let col = if enabled { t.text } else { t.text_dim.darken(60) };
            ui.icon_button(r, |c, r, _| draw(c, r, col)) && enabled
        };
        if tool(ui, back_r, can_back, draw_back_arrow) {
            self.back();
        }
        if tool(ui, up_r, can_up, draw_up_arrow) {
            let p = parent_path(&self.path);
            self.go(p);
        }
        if tool(ui, reload_r, true, draw_reload) {
            self.reload();
        }

        // Breadcrumbs: drop leading segments when they do not fit.
        let crumb_area = Rect::new(up_r.right() + 12, bar.y, reload_r.x - up_r.right() - 24, BAR_H);
        ui.canvas.fill_round_rect(crumb_area, t.radius, t.surface);
        let f = ui.fonts.body;
        let all = crumbs(&self.path);
        let sep_w = 24;
        let widths: Vec<i32> = all.iter().map(|(l, _)| f.measure(l) + 28).collect();
        let avail = crumb_area.w - 16;
        let mut first = 0;
        while first + 1 < all.len() && widths[first..].iter().sum::<i32>() + sep_w * (all.len() - first) as i32 > avail {
            first += 1;
        }
        let mut x = crumb_area.x + 8;
        if first > 0 {
            ui.text(Rect::new(x, bar.y, 20, BAR_H), "…", &f, t.text_dim, Align::Left);
            x += 20;
        }
        let mut target = None;
        for (i, (label, path)) in all.iter().enumerate().skip(first) {
            if i > first || first > 0 {
                draw_chevron(ui.canvas, Rect::new(x, bar.y, sep_w, BAR_H), t.text_dim);
                x += sep_w;
            }
            let w = widths[i].min(crumb_area.right() - 8 - x);
            let r = Rect::new(x, bar.y + 6, w, BAR_H - 12);
            let last = i + 1 == all.len();
            if !inert && !last && ui.interact(r).1 {
                target = Some(path.clone());
            }
            if last {
                ui.canvas.fill_round_rect(r, 10, t.surface_alt);
            }
            let col = if last { t.text } else { t.accent };
            let text = ellipsize(&f, label, w - 20);
            ui.text(r, &text, &f, col, Align::Center);
            x += w;
        }
        if let Some(p) = target {
            self.go(p);
        }
    }
}

/// The scrollable directory listing. Returns the index of a tapped row.
fn list_view(ui: &mut Ui, r: Rect, list: &[DirEntry], scroll: &mut i32, interactive: bool) -> Option<usize> {
    let t = *ui.theme;
    let (header, rows) = r.split_top(30);
    let (small, body) = (ui.fonts.small, ui.fonts.body);
    let size_w = 110;
    // The type column only fits in wide (landscape) layouts.
    let type_w = if r.w >= 800 { 170 } else { 0 };
    let name_x = header.x + 72;
    let type_x = header.right() - 24 - size_w - type_w;
    ui.text(Rect::new(name_x, header.y, 200, header.h), "Name", &small, t.text_dim, Align::Left);
    if type_w > 0 {
        ui.text(Rect::new(type_x, header.y, type_w, header.h), "Type", &small, t.text_dim, Align::Left);
    }
    let size_r = Rect::new(header.right() - 24 - size_w, header.y, size_w, header.h);
    ui.text(size_r, "Size", &small, t.text_dim, Align::Right);

    ui.panel(rows);
    let view = rows.inset(4);
    let off = if interactive {
        ui.scroll(view, list.len() as i32 * ROW_H, scroll)
    } else {
        *scroll
    };
    let old = ui.canvas.set_clip(view.intersect(&ui.canvas.clip()));
    let mut tapped = None;
    for (i, e) in list.iter().enumerate().skip((off / ROW_H) as usize) {
        let y = view.y + i as i32 * ROW_H - off;
        if y > view.bottom() {
            break;
        }
        let row = Rect::new(view.x, y, view.w - 10, ROW_H);
        if interactive {
            let (held, clicked) = ui.interact(row.intersect(&view));
            if held {
                ui.canvas.fill_round_rect(row.inset(2), 8, t.surface_alt);
            }
            if clicked {
                tapped = Some(i);
            }
        }
        let kind = FileKind::of(e);
        draw_entry_icon(ui.canvas, Rect::new(row.x + 16, row.y + 14, 36, 32), kind);
        let name_w = type_x - name_x - 16;
        let name = ellipsize(&body, &e.name, name_w);
        ui.text(Rect::new(name_x - 12, row.y, name_w, ROW_H), &name, &body, t.text, Align::Left);
        if type_w > 0 {
            let ty = type_label(e);
            ui.text(Rect::new(type_x - 12, row.y, type_w, ROW_H), &ty, &small, t.text_dim, Align::Left);
        }
        if !e.is_dir {
            let s = format_size(e.size);
            ui.text(Rect::new(row.right() - 14 - size_w, row.y, size_w, ROW_H), &s, &body, t.text_dim, Align::Right);
        }
        if i + 1 < list.len() {
            ui.canvas.fill_rect(Rect::new(row.x + 64, row.bottom() - 1, row.w - 78, 1), t.border);
        }
    }
    ui.canvas.restore_clip(old);
    tapped
}

/// Error state with a Retry button. Returns true when Retry is tapped.
pub fn error_view(ui: &mut Ui, r: Rect, e: FsError) -> bool {
    let t = *ui.theme;
    let (title, hint) = fs_error_text(e);
    let c = r.center();
    let card = Rect::new(c.x - 70, c.y - 130, 140, 100);
    if e == FsError::NoMedium {
        // A stylised SD card.
        let sd = Rect::new(card.x + 35, card.y + 5, 70, 90);
        ui.canvas.fill_round_rect(sd, 8, t.surface_alt);
        ui.canvas.fill_triangle([Point::new(sd.right() - 22, sd.y), Point::new(sd.right() + 1, sd.y), Point::new(sd.right() + 1, sd.y + 22)], t.bg);
        for i in 0..4 {
            ui.canvas.fill_rect(Rect::new(sd.x + 10 + i * 12, sd.y + 10, 7, 18), t.text_dim);
        }
    } else {
        ui.canvas.fill_circle(Point::new(c.x, card.y + 50), 40.0, t.surface_alt);
        let f = ui.fonts.title;
        ui.text(Rect::new(c.x - 40, card.y + 10, 80, 80), "!", &f, t.danger, Align::Center);
    }
    let (title_f, body_f) = (ui.fonts.title, ui.fonts.body);
    ui.text(Rect::new(r.x, c.y - 20, r.w, 44), title, &title_f, t.text, Align::Center);
    ui.text(Rect::new(r.x, c.y + 24, r.w, 30), hint, &body_f, t.text_dim, Align::Center);
    ui.button_primary(Rect::new(c.x - 90, c.y + 70, 180, 52), "Retry")
}

enum PanelAction {
    Close,
    Launch(&'static str),
}

/// Modal with file details and buttons that open the matching app.
fn info_panel(ui: &mut Ui, area: Rect, dir: &str, e: &DirEntry) -> Option<PanelAction> {
    let t = *ui.theme;
    ui.canvas.fill_rect_alpha(area, Color::BLACK, 150);
    let c = area.center();
    let pw = (area.w - 32).min(640);
    let p = Rect::new(c.x - pw / 2, c.y - 190, pw, 380);
    ui.canvas.fill_round_rect(p, 18, t.surface);
    ui.canvas.stroke_round_rect(p, 18, 1, t.border);

    let kind = FileKind::of(e);
    draw_entry_icon(ui.canvas, Rect::new(p.x + 32, p.y + 32, 72, 72), kind);
    let (large, body, small) = (ui.fonts.large, ui.fonts.body, ui.fonts.small);
    let tx = p.x + 128;
    let tw = p.right() - 32 - tx;
    let name = ellipsize(&large, &e.name, tw);
    ui.text(Rect::new(tx, p.y + 34, tw, 34), &name, &large, t.text, Align::Left);
    ui.text(Rect::new(tx, p.y + 72, tw, 26), &type_label(e), &body, t.text_dim, Align::Left);

    let rows = [("Size", format!("{}  ({} bytes)", format_size(e.size), e.size)), ("Location", dir.to_string())];
    for (i, (k, v)) in rows.iter().enumerate() {
        let y = p.y + 130 + i as i32 * 36;
        ui.text(Rect::new(p.x + 32, y, 110, 30), k, &body, t.text_dim, Align::Left);
        let v = ellipsize(&body, v, p.w - 174);
        ui.text(Rect::new(p.x + 142, y, p.w - 174, 30), &v, &body, t.text, Align::Left);
    }

    let (label, app) = match kind {
        FileKind::Image => ("Open in Photos", Some("photos")),
        FileKind::Text => ("Open in Notes", Some("notes")),
        FileKind::Audio => ("Play in Music", Some("music")),
        _ => ("", None),
    };
    let hint = if app.is_some() {
        "The app opens on its own screen; pick the file there."
    } else {
        "No installed app opens this type of file."
    };
    ui.text(Rect::new(p.x + 32, p.y + 216, p.w - 64, 26), hint, &small, t.text_dim, Align::Left);

    let btn_y = p.bottom() - 32 - 56;
    let close = Rect::new(p.right() - 32 - 140, btn_y, 140, 56);
    let mut action = None;
    if let Some(id) = app {
        if ui.button_primary(Rect::new(p.x + 32, btn_y, (close.x - 16 - p.x - 32).min(260), 56), label) {
            action = Some(PanelAction::Launch(id));
        }
    }
    if ui.button(close, "Close") {
        action = Some(PanelAction::Close);
    }
    // Tapping outside the panel dismisses it.
    if ui.input.tapped_in(area) && !p.contains(ui.input.pos) {
        action = Some(PanelAction::Close);
    }
    action
}

fn draw_back_arrow(c: &mut Canvas, r: Rect, col: Color) {
    let m = r.center();
    c.line(Point::new(m.x - 12, m.y), Point::new(m.x + 12, m.y), 3.0, col);
    c.line(Point::new(m.x - 12, m.y), Point::new(m.x - 2, m.y - 10), 3.0, col);
    c.line(Point::new(m.x - 12, m.y), Point::new(m.x - 2, m.y + 10), 3.0, col);
}

fn draw_up_arrow(c: &mut Canvas, r: Rect, col: Color) {
    let m = r.center();
    c.line(Point::new(m.x, m.y - 12), Point::new(m.x, m.y + 12), 3.0, col);
    c.line(Point::new(m.x, m.y - 12), Point::new(m.x - 10, m.y - 2), 3.0, col);
    c.line(Point::new(m.x, m.y - 12), Point::new(m.x + 10, m.y - 2), 3.0, col);
}

fn draw_reload(c: &mut Canvas, r: Rect, col: Color) {
    // A 300-degree arc with an arrow head where it ends.
    let m = r.center();
    let rad = 12.0f32;
    let at = |deg: f32| {
        let a = deg.to_radians();
        Point::new(m.x + (rad * libm::cosf(a)) as i32, m.y - (rad * libm::sinf(a)) as i32)
    };
    let mut prev = at(60.0);
    for step in 1..=20 {
        let p = at(60.0 + step as f32 * 15.0);
        c.line(prev, p, 3.0, col);
        prev = p;
    }
    let tip = at(60.0);
    c.fill_triangle([Point::new(tip.x - 2, tip.y - 8), Point::new(tip.x + 8, tip.y + 2), Point::new(tip.x - 4, tip.y + 6)], col);
}

fn draw_chevron(c: &mut Canvas, r: Rect, col: Color) {
    let m = r.center();
    c.line(Point::new(m.x - 4, m.y - 8), Point::new(m.x + 4, m.y), 2.0, col);
    c.line(Point::new(m.x + 4, m.y), Point::new(m.x - 4, m.y + 8), 2.0, col);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> DirEntry {
        DirEntry { name: name.to_string(), is_dir, size: 0 }
    }

    #[test]
    fn paths() {
        assert_eq!(join_path("/", "sd"), "/sd");
        assert_eq!(join_path("/sd", "Pictures"), "/sd/Pictures");
        assert_eq!(parent_path("/sd/Pictures"), "/sd");
        assert_eq!(parent_path("/sd"), "/");
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path("/sd/a/"), "/sd");
    }

    #[test]
    fn extensions_and_kinds() {
        assert_eq!(extension("Photo.JPG"), "jpg");
        assert_eq!(extension("archive.tar.gz"), "gz");
        assert_eq!(extension("README"), "");
        assert_eq!(extension(".hidden"), "");
        assert_eq!(FileKind::of(&entry("a.jpeg", false)), FileKind::Image);
        assert_eq!(FileKind::of(&entry("a.bmp", false)), FileKind::Image);
        assert_eq!(FileKind::of(&entry("a.txt", false)), FileKind::Text);
        assert_eq!(FileKind::of(&entry("a.mp3", false)), FileKind::Audio);
        assert_eq!(FileKind::of(&entry("a.jpg", true)), FileKind::Folder);
        assert_eq!(type_label(&entry("x.bin", false)), "BIN file");
        assert_eq!(type_label(&entry("song.wav", false)), "WAV audio");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(12 * 1024), "12 KB");
        assert_eq!(format_size(3 * 1024 * 1024 + 400 * 1024), "3.3 MB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn sorting_hides_dotfiles_and_puts_folders_first() {
        let mut v = alloc::vec![entry("b.txt", false), entry("Zed", true), entry(".Trash", true), entry("a.txt", false), entry("alpha", true)];
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zed", "a.txt", "b.txt"]);
    }

    #[test]
    fn breadcrumbs() {
        let c = crumbs("/sd/Pictures");
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], ("Root".to_string(), "/".to_string()));
        assert_eq!(c[2], ("Pictures".to_string(), "/sd/Pictures".to_string()));
        assert_eq!(crumbs("/").len(), 1);
    }

    #[test]
    fn ellipsize_fits() {
        let fonts = p4os_sdk::gui::font::builtin::load();
        let f = fonts.body;
        assert_eq!(ellipsize(&f, "short", 500), "short");
        let long = ellipsize(&f, "a very long file name that will not fit.txt", 120);
        assert!(long.ends_with('…'));
        assert!(f.measure(&long) <= 120);
    }

    #[test]
    fn navigation_history() {
        let mut app = Files::new();
        app.go("/sd".to_string());
        app.go("/sd/Pictures".to_string());
        assert_eq!(app.history, ["/", "/sd"]);
        app.back();
        assert_eq!(app.path, "/sd");
        app.back();
        assert_eq!(app.path, "/");
        app.back();
        assert_eq!(app.path, "/");
    }
}
