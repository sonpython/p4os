//! Notes: plain-text notes stored as `.txt` files in `/sd/Notes`, with a
//! multi-line editor driven by the on-screen keyboard.
//!
//! Portrait shows the list or the editor; landscape shows both side by side.
//! Unsaved edits are saved automatically when leaving a note or the app.

use crate::files::{self, ellipsize, error_view, extension, format_size, join_path};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use p4os_sdk::gui::ui::{Align, Key};
use p4os_sdk::{App, AppInfo, Canvas, Color, FsError, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "notes", name: "Notes", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Notes::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0xffe28a), Color::hex(0xf5b841));
    let page = Rect::new(r.x + r.w / 4, r.y + r.h / 5, r.w / 2, r.h * 3 / 5);
    c.fill_round_rect(page, r.w / 16, Color::WHITE);
    for i in 0..4 {
        let y = page.y + page.h / 5 + i * page.h / 6;
        let w = if i == 3 { page.w / 3 } else { page.w * 2 / 3 };
        c.fill_rect(Rect::new(page.x + page.w / 6, y, w, (r.h / 40).max(2)), Color::hex(0xc0a050));
    }
}

const NOTES_DIR: &str = "/sd/Notes";
const BAR_H: i32 = 64;
const ROW_H: i32 = 84;
/// Bytes read from each note for the list preview.
const PREVIEW_BYTES: usize = 256;
/// List width in the side-by-side (landscape) layout.
const SIDE_LIST_W: i32 = 360;

struct NoteEntry {
    name: String,
    size: u64,
    title: String,
    snippet: String,
}

struct Editor {
    path: String,
    text: String,
    /// Contents as last loaded or saved, to detect unsaved edits.
    saved: String,
    focused: bool,
    scroll: i32,
    /// Keep the caret (end of text) in view after typing.
    follow: bool,
    status: Option<(String, bool)>,
}

impl Editor {
    fn new(path: String, text: String) -> Self {
        Self { path, saved: text.clone(), text, focused: false, scroll: 0, follow: false, status: None }
    }

    fn dirty(&self) -> bool {
        self.text != self.saved
    }

    fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    fn save(&mut self, sys: &mut dyn System) -> bool {
        match sys.fs().write(&self.path, self.text.as_bytes()) {
            Ok(()) => {
                self.saved = self.text.clone();
                self.status = Some(("Saved".to_string(), false));
                true
            }
            Err(e) => {
                self.status = Some((save_error(e).to_string(), true));
                false
            }
        }
    }
}

#[derive(Default)]
struct Notes {
    /// `None` until listed; `Ok(false)` means the Notes folder is missing.
    loaded: Option<Result<bool, FsError>>,
    notes: Vec<NoteEntry>,
    scroll: i32,
    editor: Option<Editor>,
    /// State changed after this frame was drawn; draw again right away.
    redraw: bool,
}

impl Notes {
    fn load(&mut self, sys: &mut dyn System) {
        self.notes.clear();
        self.loaded = Some(match sys.fs().list(NOTES_DIR) {
            Ok(mut list) => {
                files::sort_entries(&mut list);
                for e in list.into_iter().filter(|e| !e.is_dir && extension(&e.name) == "txt") {
                    let head = read_head(sys, &join_path(NOTES_DIR, &e.name));
                    let (title, snippet) = preview(&head, &e.name);
                    self.notes.push(NoteEntry { name: e.name, size: e.size, title, snippet });
                }
                Ok(true)
            }
            Err(FsError::NotFound) => Ok(false),
            Err(e) => Err(e),
        });
    }

    /// Save pending edits and close the editor; stays open if saving fails.
    fn close_editor(&mut self, sys: &mut dyn System) -> bool {
        if let Some(ed) = self.editor.as_mut().filter(|e| e.dirty()) {
            if !ed.save(sys) {
                return false;
            }
            self.loaded = None;
        }
        self.editor = None;
        self.redraw = true;
        true
    }

    fn open_note(&mut self, sys: &mut dyn System, name: &str) {
        if !self.close_editor(sys) {
            return;
        }
        let path = join_path(NOTES_DIR, name);
        self.redraw = true;
        match sys.fs().read(&path) {
            Ok(data) => self.editor = Some(Editor::new(path, normalize(&data))),
            Err(e) => {
                let mut ed = Editor::new(path, String::new());
                ed.status = Some((format!("Could not open: {}", files::fs_error_text(e).0), true));
                self.editor = Some(ed);
            }
        }
    }

    fn new_note(&mut self, sys: &mut dyn System) {
        if !self.close_editor(sys) {
            return;
        }
        let name = next_note_name(self.notes.iter().map(|n| n.name.as_str()));
        let mut ed = Editor::new(join_path(NOTES_DIR, &name), String::new());
        ed.focused = true;
        ed.status = Some(("New note, not saved yet".to_string(), false));
        self.editor = Some(ed);
        self.redraw = true;
    }
}

impl App for Notes {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        ui.fill(area, t.bg);
        self.redraw = false;
        if self.loaded.is_none() {
            self.load(sys);
        }
        if let Some(Err(e)) = self.loaded {
            if error_view(ui, area, e) {
                self.loaded = None;
            }
            return;
        }

        let side_by_side = area.w > area.h;
        let inner = area.inset(12);
        if side_by_side {
            let (list_r, rest) = inner.split_left(SIDE_LIST_W);
            let ed_r = Rect::new(rest.x + 12, rest.y, rest.w - 12, rest.h);
            self.list_pane(ui, list_r, sys);
            if self.editor.is_some() {
                self.editor_pane(ui, ed_r, sys, false);
            } else {
                ui.panel(ed_r);
                let f = ui.fonts.large;
                ui.text(ed_r, "Select a note or tap New", &f, t.text_dim, Align::Center);
            }
        } else if self.editor.is_some() {
            self.editor_pane(ui, inner, sys, true);
        } else {
            self.list_pane(ui, inner, sys);
        }
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        self.redraw.then_some(0)
    }

    fn on_close(&mut self, sys: &mut dyn System) {
        if let Some(ed) = self.editor.as_mut() {
            if ed.dirty() {
                ed.save(sys);
            }
        }
    }
}

impl Notes {
    fn list_pane(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let (header, body) = r.split_top(BAR_H);
        let (title_f, small) = (ui.fonts.title, ui.fonts.small);
        let new_r = Rect::new(header.right() - 120, header.y + 4, 120, 56);
        ui.text(Rect::new(header.x + 4, header.y, new_r.x - header.x, 40), "Notes", &title_f, t.text, Align::Left);
        let count = match self.notes.len() {
            1 => "1 note".to_string(),
            n => format!("{n} notes"),
        };
        ui.text(Rect::new(header.x + 4, header.y + 38, new_r.x - header.x, 22), &count, &small, t.text_dim, Align::Left);
        if ui.button_primary(new_r, "+ New") {
            self.new_note(sys);
        }

        let list_r = Rect::new(body.x, body.y + 8, body.w, body.h - 8);
        ui.panel(list_r);
        if self.notes.is_empty() {
            let missing = self.loaded == Some(Ok(false));
            let (head, hint) = if missing {
                ("No Notes folder yet", "Create a Notes folder on the SD card to save notes.")
            } else {
                ("No notes yet", "Tap + New to write your first note.")
            };
            let (large, body_f) = (ui.fonts.large, ui.fonts.body);
            let c = list_r.center();
            ui.text(Rect::new(list_r.x, c.y - 40, list_r.w, 36), head, &large, t.text, Align::Center);
            let lines = body_f.wrap(hint, list_r.w - 48);
            for (i, line) in lines.iter().enumerate() {
                let y = c.y + 4 + i as i32 * body_f.line_height;
                ui.text(Rect::new(list_r.x, y, list_r.w, body_f.line_height), line, &body_f, t.text_dim, Align::Center);
            }
            return;
        }

        let view = list_r.inset(4);
        let off = ui.scroll(view, self.notes.len() as i32 * ROW_H, &mut self.scroll);
        let old = ui.canvas.set_clip(view.intersect(&ui.canvas.clip()));
        let current = self.editor.as_ref().map(|e| e.name().to_string());
        let body_f = ui.fonts.body;
        let mut tapped = None;
        for (i, n) in self.notes.iter().enumerate().skip((off / ROW_H) as usize) {
            let y = view.y + i as i32 * ROW_H - off;
            if y > view.bottom() {
                break;
            }
            let row = Rect::new(view.x, y, view.w - 10, ROW_H);
            let (held, clicked) = ui.interact(row.intersect(&view));
            if clicked {
                tapped = Some(n.name.clone());
            }
            if held || current.as_deref() == Some(n.name.as_str()) {
                ui.canvas.fill_round_rect(row.inset(3), 10, if held { t.surface_alt } else { t.accent.darken(90) });
            }
            let tw = row.w - 32;
            let title = ellipsize(&body_f, &n.title, tw);
            ui.text(Rect::new(row.x + 16, row.y + 12, tw, 28), &title, &body_f, t.text, Align::Left);
            let meta = format!("{}  ·  {}", n.name, format_size(n.size));
            let sub = if n.snippet.is_empty() { meta } else { format!("{}  ·  {}", meta, n.snippet) };
            let sub = ellipsize(&small, &sub, tw);
            ui.text(Rect::new(row.x + 16, row.y + 44, tw, 24), &sub, &small, t.text_dim, Align::Left);
            if i + 1 < self.notes.len() {
                ui.canvas.fill_rect(Rect::new(row.x + 16, row.bottom() - 1, row.w - 32, 1), t.border);
            }
        }
        ui.canvas.restore_clip(old);
        if let Some(name) = tapped {
            self.open_note(sys, &name);
        }
    }

    fn editor_pane(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System, show_back: bool) {
        let t = *ui.theme;
        let (bar, body) = r.split_top(BAR_H);
        let mut x = bar.x;
        let mut close = false;
        if show_back {
            if ui.button(Rect::new(x, bar.y + 4, 120, 56), "← List") {
                close = true;
            }
            x += 132;
        }
        let save_r = Rect::new(bar.right() - 110, bar.y + 4, 110, 56);
        let done_r = Rect::new(save_r.x - 12 - 100, bar.y + 4, 100, 56);
        let Some(ed) = self.editor.as_mut() else { return };
        let mut right = save_r.x;
        if ed.focused {
            right = done_r.x;
            if ui.button(done_r, "Done") {
                ed.focused = false;
                self.redraw = true;
            }
        }
        if ui.button_primary(save_r, "Save") {
            if ed.save(sys) {
                self.loaded = None;
            }
            self.redraw = true;
        }

        // Title and status between the buttons.
        let (body_f, small) = (ui.fonts.body, ui.fonts.small);
        let title_r = Rect::new(x + 4, bar.y + 6, right - x - 16, 28);
        let name = ellipsize(&body_f, ed.name(), title_r.w);
        ui.text(title_r, &name, &body_f, t.text, Align::Left);
        let status = match &ed.status {
            Some((_, true)) => "Not saved".to_string(),
            _ if ed.dirty() => "Unsaved changes".to_string(),
            Some((s, _)) => s.clone(),
            None => format!("{} characters", ed.text.chars().count()),
        };
        let error = ed.status.as_ref().filter(|(_, e)| *e).map(|(s, _)| s.clone());
        let status = ellipsize(&small, &status, title_r.w);
        let status_col = if error.is_some() { t.danger } else { t.text_dim };
        ui.text(Rect::new(title_r.x, bar.y + 36, title_r.w, 22), &status, &small, status_col, Align::Left);

        // Errors get a full-width banner above the text, with a way out
        // that drops the edits (leaving would otherwise retry the save).
        let mut top = body.y + 8;
        let mut discard = false;
        if let Some(msg) = error {
            let banner = Rect::new(body.x, top, body.w, 64);
            ui.canvas.fill_round_rect(banner, 10, t.danger.darken(120));
            let btn = Rect::new(banner.right() - 8 - 120, banner.y + 8, 120, 48);
            let text_r = Rect::new(banner.x + 16, banner.y, btn.x - banner.x - 24, banner.h);
            let lines = small.wrap(&msg, text_r.w);
            let lh = small.line_height;
            let y0 = text_r.y + (text_r.h - lh * lines.len().min(2) as i32) / 2;
            for (i, line) in lines.iter().take(2).enumerate() {
                ui.text(Rect::new(text_r.x, y0 + i as i32 * lh, text_r.w, lh), line, &small, t.text, Align::Left);
            }
            discard = ui.button_colored(btn, "Discard", t.danger, Color::WHITE);
            top = banner.bottom() + 8;
        }
        let edit_r = Rect::new(body.x, top, body.w, body.bottom() - top);
        text_editor(ui, edit_r, ed);

        if discard {
            self.editor = None;
            self.redraw = true;
        } else if close {
            self.close_editor(sys);
        }
    }
}

/// Multi-line text area. Tapping it takes keyboard focus; typed characters
/// and Enter append at the end (where the caret is), Backspace deletes.
fn text_editor(ui: &mut Ui, r: Rect, ed: &mut Editor) {
    let t = *ui.theme;
    if ui.interact(r).1 {
        ed.focused = true;
    }
    if ed.focused {
        ui.state.wants_keyboard = true;
        if apply_keys(&mut ed.text, &ui.input.keys) {
            ed.follow = true;
            ed.status = None;
        }
    }

    ui.canvas.fill_round_rect(r, t.radius, t.surface);
    if ed.focused {
        ui.canvas.stroke_round_rect(r, t.radius, 2, t.accent);
    }
    let inner = r.inset(16);
    let f = ui.fonts.body;
    let lh = f.line_height + 4;
    let lines = f.wrap(&ed.text, inner.w - 12);
    let content_h = lines.len() as i32 * lh;
    if ui.input.down && r.contains(ui.input.press_pos) && ui.input.is_drag() {
        ed.follow = false;
    }
    if ed.follow {
        ed.scroll = content_h - inner.h;
    }
    let off = ui.scroll(inner, content_h, &mut ed.scroll);

    let old = ui.canvas.set_clip(inner.intersect(&ui.canvas.clip()));
    if ed.text.is_empty() && !ed.focused {
        f.draw(ui.canvas, Point::new(inner.x, inner.y), "Tap here to start writing", t.text_dim);
    }
    for (i, line) in lines.iter().enumerate() {
        let y = inner.y + i as i32 * lh - off;
        if y + lh < inner.y || y > inner.bottom() {
            continue;
        }
        f.draw(ui.canvas, Point::new(inner.x, y), line, t.text);
    }
    if ed.focused {
        let last = lines.last().map_or("", |s| s.as_str());
        let cx = inner.x + f.measure(last) + 1;
        let cy = inner.y + (lines.len() as i32 - 1).max(0) * lh - off;
        ui.canvas.fill_rect(Rect::new(cx, cy + 2, 2, f.line_height - 4), t.accent);
    }
    ui.canvas.restore_clip(old);
}

/// Apply typed keys to `text`; returns true if it changed.
fn apply_keys(text: &mut String, keys: &[Key]) -> bool {
    let mut changed = false;
    for k in keys {
        match k {
            Key::Char(c) => text.push(*c),
            Key::Enter => text.push('\n'),
            Key::Backspace => {
                if text.pop().is_none() {
                    continue;
                }
            }
            _ => continue,
        }
        changed = true;
    }
    changed
}

/// Decode file bytes as text with `\n` line endings.
fn normalize(data: &[u8]) -> String {
    String::from_utf8_lossy(data).replace("\r\n", "\n").replace('\r', "\n")
}

/// Read the first bytes of a file for the list preview.
fn read_head(sys: &mut dyn System, path: &str) -> String {
    let Ok(mut f) = sys.fs().open(path) else { return String::new() };
    let mut buf = [0u8; PREVIEW_BYTES];
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    // A multi-byte character may be cut at the end; drop the broken tail.
    let s = normalize(&buf[..n]);
    s.trim_end_matches('\u{FFFD}').to_string()
}

/// Title (first non-empty line, else the file name) and a snippet of the
/// following text joined onto one line.
fn preview(text: &str, file_name: &str) -> (String, String) {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let title = lines.next().map_or_else(|| file_name.to_string(), |l| l.to_string());
    let snippet: Vec<&str> = lines.collect();
    (title, snippet.join(" "))
}

/// `note-N.txt` with N one higher than any existing `note-<n>.txt`.
fn next_note_name<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let max = names
        .filter_map(|n| n.strip_prefix("note-")?.strip_suffix(".txt")?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("note-{}.txt", max + 1)
}

fn save_error(e: FsError) -> &'static str {
    match e {
        FsError::NotFound => "No Notes folder on the SD card; create it first",
        FsError::NoMedium => "Insert SD card to save",
        _ => "Could not write to the SD card",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_names() {
        assert_eq!(next_note_name([].into_iter()), "note-1.txt");
        assert_eq!(next_note_name(["ideas.txt", "note-2.txt", "note-10.txt", "note-x.txt"].into_iter()), "note-11.txt");
    }

    #[test]
    fn keys_edit_text() {
        let mut s = String::from("ab");
        assert!(apply_keys(&mut s, &[Key::Char('c'), Key::Enter, Key::Char('d')]));
        assert_eq!(s, "abc\nd");
        assert!(apply_keys(&mut s, &[Key::Backspace, Key::Backspace]));
        assert_eq!(s, "abc");
        let mut empty = String::new();
        assert!(!apply_keys(&mut empty, &[Key::Backspace, Key::Left]));
    }

    #[test]
    fn previews() {
        let (t, s) = preview("\n  Shopping\n- milk\n\n- eggs\n", "a.txt");
        assert_eq!(t, "Shopping");
        assert_eq!(s, "- milk - eggs");
        assert_eq!(preview("", "note-1.txt"), ("note-1.txt".to_string(), String::new()));
    }

    #[test]
    fn line_endings() {
        assert_eq!(normalize(b"a\r\nb\rc\n"), "a\nb\nc\n");
    }

    #[test]
    fn dirty_tracking() {
        let mut ed = Editor::new("/sd/Notes/a.txt".to_string(), "x".to_string());
        assert!(!ed.dirty());
        ed.text.push('y');
        assert!(ed.dirty());
        assert_eq!(ed.name(), "a.txt");
    }
}
