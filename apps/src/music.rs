//! Music player: lists the audio files on the SD card (`/sd/Music`, or the
//! card root when that folder has none), plays them through the OS audio
//! thread and advances to the next track when one finishes.
//!
//! Track lengths come from the file headers: WAV from the `data` chunk size,
//! MP3 from a Xing/Info/VBRI frame count when present, otherwise estimated
//! from the file size and the first frame's bitrate.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use p4os_sdk::gui::font::Font;
use p4os_sdk::gui::ui::{Align, Key};
use p4os_sdk::{App, AppInfo, Canvas, Color, FileReader, FsError, PlaybackState, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "music", name: "Music", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Music::default())
}

const MUSIC_DIR: &str = "/sd/Music";
const SD_ROOT: &str = "/sd";
/// Poll period while a track plays (elapsed time, end-of-track detection).
const TICK_MS: u32 = 250;
/// "Previous" restarts the current track when it has played this long.
const RESTART_THRESHOLD_MS: u64 = 3000;
const ROW_H: i32 = 64;
/// Height of the now-playing panel when it is stacked above the library.
const NOW_PLAYING_H: i32 = 470;

const ART_TOP: Color = Color::hex(0xff6b9d);
const ART_BOTTOM: Color = Color::hex(0x7b3fe4);

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, ART_TOP, ART_BOTTOM);
    draw_note(c, r.inset(r.w / 5), Color::WHITE);
}

/// Two beamed eighth notes filling `r`.
fn draw_note(c: &mut Canvas, r: Rect, col: Color) {
    let s = r.w.min(r.h);
    let (x0, y0) = (r.x + (r.w - s) / 2, r.y + (r.h - s) / 2);
    let head = s as f32 * 0.14;
    let stem = (s / 14).max(2);
    let (lx, rx) = (x0 + s * 30 / 100, x0 + s * 78 / 100);
    let (ly, ry) = (y0 + s * 78 / 100, y0 + s * 68 / 100);
    let (ltop, rtop) = (y0 + s * 22 / 100, y0 + s * 12 / 100);
    c.fill_circle(Point::new(lx - head as i32 + stem / 2, ly), head, col);
    c.fill_circle(Point::new(rx - head as i32 + stem / 2, ry), head, col);
    c.fill_rect(Rect::new(lx, ltop, stem, ly - ltop), col);
    c.fill_rect(Rect::new(rx, rtop, stem, ry - rtop), col);
    // Slanted beam joining the stem tops.
    let beam = s / 9;
    c.fill_triangle([Point::new(lx, ltop), Point::new(rx + stem, rtop), Point::new(rx + stem, rtop + beam)], col);
    c.fill_triangle([Point::new(lx, ltop), Point::new(rx + stem, rtop + beam), Point::new(lx, ltop + beam)], col);
}

// ---------------------------------------------------------------------------
// Track metadata (pure, unit tested)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Wav,
    Mp3,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrackInfo {
    pub format: Format,
    pub sample_rate: u32,
    pub channels: u8,
    /// Nominal bitrate in kbit/s (first frame for MP3, PCM rate for WAV).
    pub kbps: u32,
    /// True when the length comes from a frame count or exact data size
    /// rather than a size/bitrate estimate.
    pub exact: bool,
    /// Variable bitrate MP3 (Xing tag); `kbps` is then the average.
    pub vbr: bool,
    pub duration_ms: Option<u64>,
}

/// Parse a RIFF/WAVE header. `head` is the start of the file and
/// `file_size` its full length (used when the data chunk size is bogus,
/// as written by some streaming encoders).
pub fn probe_wav(head: &[u8], file_size: u64) -> Option<TrackInfo> {
    if head.len() < 12 || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u32, u16)> = None; // channels, rate, bits
    while pos + 8 <= head.len() {
        let id = &head[pos..pos + 4];
        let len = u32::from_le_bytes(head[pos + 4..pos + 8].try_into().ok()?) as u64;
        let body = pos + 8;
        if id == b"fmt " {
            let f = head.get(body..body + 16)?;
            let channels = u16::from_le_bytes([f[2], f[3]]);
            let rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
            let bits = u16::from_le_bytes([f[14], f[15]]);
            fmt = Some((channels, rate, bits));
        } else if id == b"data" {
            let (channels, rate, bits) = fmt?;
            let frame_bytes = channels as u64 * bits as u64 / 8;
            if rate == 0 || frame_bytes == 0 {
                return None;
            }
            let available = file_size.saturating_sub(body as u64);
            let data = if len == 0 || len > available { available } else { len };
            return Some(TrackInfo {
                format: Format::Wav,
                sample_rate: rate,
                channels: channels as u8,
                kbps: (rate as u64 * frame_bytes * 8 / 1000) as u32,
                exact: true,
                vbr: false,
                duration_ms: Some(data / frame_bytes * 1000 / rate as u64),
            });
        }
        pos = body + len as usize + (len as usize & 1);
    }
    None
}

/// Length of an ID3v2 tag at the start of `head` (header included), or 0.
pub fn id3v2_len(head: &[u8]) -> usize {
    if head.len() < 10 || &head[0..3] != b"ID3" {
        return 0;
    }
    let size = head[6..10].iter().fold(0usize, |acc, b| (acc << 7) | (*b as usize & 0x7f));
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

struct FrameHeader {
    mpeg1: bool,
    mono: bool,
    kbps: u32,
    sample_rate: u32,
}

fn parse_frame_header(h: &[u8]) -> Option<FrameHeader> {
    if h.len() < 4 || h[0] != 0xff || h[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 0: MPEG2.5, 2: MPEG2, 3: MPEG1
    let layer = (h[1] >> 1) & 3; // 1: Layer III
    let br_idx = (h[2] >> 4) as usize;
    let sr_idx = ((h[2] >> 2) & 3) as usize;
    if version == 1 || layer != 1 || br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    const BR_V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const BR_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    const SR_V1: [u32; 3] = [44_100, 48_000, 32_000];
    let mpeg1 = version == 3;
    let sample_rate = match version {
        3 => SR_V1[sr_idx],
        2 => SR_V1[sr_idx] / 2,
        _ => SR_V1[sr_idx] / 4,
    };
    Some(FrameHeader {
        mpeg1,
        mono: h[3] >> 6 == 3,
        kbps: if mpeg1 { BR_V1[br_idx] } else { BR_V2[br_idx] },
        sample_rate,
    })
}

fn be32(b: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(..4)?.try_into().ok()?))
}

/// Parse the first MPEG audio frame in `audio` (the bytes right after any
/// ID3v2 tag). `audio_len` is the number of audio bytes in the whole file,
/// used for the constant-bitrate estimate.
pub fn probe_mp3(audio: &[u8], audio_len: u64) -> Option<TrackInfo> {
    let start = (0..audio.len().saturating_sub(4)).find(|&i| parse_frame_header(&audio[i..]).is_some())?;
    let f = &audio[start..];
    let h = parse_frame_header(f)?;
    let samples_per_frame: u64 = if h.mpeg1 { 1152 } else { 576 };
    let side_info = match (h.mpeg1, h.mono) {
        (true, false) => 32,
        (true, true) => 17,
        (false, false) => 17,
        (false, true) => 9,
    };

    // Xing/Info (LAME and most encoders) or VBRI (Fraunhofer) frame counts.
    // "Info" is the same tag written for constant-bitrate files.
    let mut frames = None;
    let mut vbr = false;
    let x = 4 + side_info;
    if let Some(tag) = f.get(x..x + 4) {
        if (tag == b"Xing" || tag == b"Info") && be32(&f[x + 4..]).is_some_and(|flags| flags & 1 != 0) {
            frames = be32(f.get(x + 8..).unwrap_or(&[]));
            vbr = tag == b"Xing";
        }
    }
    if frames.is_none() && f.get(36..40) == Some(b"VBRI") {
        frames = be32(f.get(50..).unwrap_or(&[]));
        vbr = true;
    }

    let bytes = audio_len.saturating_sub(start as u64);
    let (duration_ms, exact) = match frames {
        Some(n) if n > 0 => (n as u64 * samples_per_frame * 1000 / h.sample_rate as u64, true),
        _ => (bytes * 8 / h.kbps as u64, false),
    };
    let kbps = if vbr && duration_ms > 0 { (bytes * 8 / duration_ms) as u32 } else { h.kbps };
    Some(TrackInfo {
        format: Format::Mp3,
        sample_rate: h.sample_rate,
        channels: if h.mono { 1 } else { 2 },
        kbps,
        exact,
        vbr,
        duration_ms: Some(duration_ms),
    })
}

/// "m:ss", or "h:mm:ss" from one hour up.
pub fn format_time(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 { format!("{}:{:02}:{:02}", h, m, s) } else { format!("{}:{:02}", m, s) }
}

/// Track after `cur` in a list of `len`, or `None` at the end.
pub fn next_index(cur: usize, len: usize) -> Option<usize> {
    (cur + 1 < len).then_some(cur + 1)
}

/// What "previous" does: restart the current track once it has played a
/// few seconds (or when it is the first), otherwise go back one.
pub fn prev_index(cur: usize, elapsed_ms: u64) -> usize {
    if elapsed_ms > RESTART_THRESHOLD_MS || cur == 0 { cur } else { cur - 1 }
}

/// File name without its extension, used as the track title.
pub fn title_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// Cut `text` with an ellipsis so it fits in `width` pixels.
fn ellipsize(font: &Font, text: &str, width: i32) -> String {
    if font.measure(text) <= width {
        return text.to_string();
    }
    let budget = width - font.measure("…");
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
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// File access

/// Read up to `buf.len()` bytes, stopping early only at end of file.
fn read_full(r: &mut dyn FileReader, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    n
}

const PROBE_LEN: usize = 4096;
/// Larger tags (embedded cover art) are not skipped while scanning, so the
/// library stays quick to load; such tracks show no length.
const MAX_ID3_SKIP: usize = 512 * 1024;

fn probe(name: &str, mut r: Box<dyn FileReader>) -> Option<TrackInfo> {
    let size = r.size();
    let mut head = vec![0u8; PROBE_LEN];
    let n = read_full(&mut *r, &mut head);
    head.truncate(n);
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".wav") {
        return probe_wav(&head, size);
    }
    let tag = id3v2_len(&head);
    if tag <= head.len() {
        return probe_mp3(&head[tag..], size - tag as u64);
    }
    if tag > MAX_ID3_SKIP {
        return None;
    }
    // Skip the rest of the tag, then read the first audio frames.
    let mut skip = tag - head.len();
    let mut scratch = vec![0u8; 8192];
    while skip > 0 {
        let want = skip.min(scratch.len());
        let got = read_full(&mut *r, &mut scratch[..want]);
        if got == 0 {
            return None;
        }
        skip -= got;
    }
    head.resize(PROBE_LEN, 0);
    let n = read_full(&mut *r, &mut head);
    probe_mp3(&head[..n], size.saturating_sub(tag as u64))
}

struct Track {
    name: String,
    path: String,
    info: Option<TrackInfo>,
}

// ---------------------------------------------------------------------------
// The app

#[derive(Default)]
struct Music {
    scanned: bool,
    folder: String,
    tracks: Vec<Track>,
    /// Shown instead of the library when scanning failed.
    scan_error: Option<&'static str>,
    current: Option<usize>,
    /// A play session is running: playback was started by us and should
    /// auto-advance when the track ends.
    active: bool,
    /// The audio thread has reported Playing/Paused since the last `play`,
    /// so a later Stopped means the track really ended.
    seen_playing: bool,
    state: Option<PlaybackState>,
    play_error: Option<String>,
    list_offset: i32,
    volume: f32,
}

impl App for Music {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        if !self.scanned {
            self.scan(sys);
            self.volume = sys.audio().volume();
        }
        self.poll(sys);

        for k in ui.input.keys.clone() {
            match k {
                Key::Char(' ') => self.toggle_play(sys),
                Key::Left => self.prev(sys),
                Key::Right => self.next(sys),
                _ => {}
            }
        }

        let t = *ui.theme;
        ui.fill(area, t.bg);
        // Portrait: now-playing on top, library below. Landscape: library on
        // the left, now-playing beside it.
        let outer = area.inset(16);
        let (lib, now) = if outer.w > outer.h {
            let (lib, rest) = outer.split_left(outer.w * 2 / 5);
            (lib, Rect::new(rest.x + 16, rest.y, rest.w - 16, rest.h))
        } else {
            let (now, rest) = outer.split_top(NOW_PLAYING_H.min(outer.h * 3 / 5));
            (Rect::new(rest.x, rest.y + 16, rest.w, rest.h - 16), now)
        };
        self.library(ui, lib, sys);
        self.now_playing(ui, now, sys);

        self.state = Some(sys.audio().state());
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        (self.active && self.state != Some(PlaybackState::Paused)).then_some(TICK_MS)
    }

    fn on_close(&mut self, sys: &mut dyn System) {
        // The app object is dropped on close, so nothing could advance the
        // playlist afterwards: stop rather than leave a lone track playing.
        if self.active {
            sys.audio().stop();
        }
    }
}

impl Music {
    fn scan(&mut self, sys: &mut dyn System) {
        self.scanned = true;
        self.scan_error = None;
        self.tracks.clear();
        let mut result = Err(FsError::NotFound);
        for dir in [MUSIC_DIR, SD_ROOT] {
            result = sys.fs().list(dir).map(|entries| (dir, entries));
            match &result {
                Ok((_, e)) if e.iter().any(|e| !e.is_dir && p4os_media::audio::is_audio_file(&e.name)) => break,
                Err(FsError::NoMedium) => break,
                _ => {}
            }
        }
        let (dir, mut entries) = match result {
            Ok(r) => r,
            Err(FsError::NoMedium) => {
                self.scan_error = Some("No SD card inserted");
                return;
            }
            Err(_) => {
                self.scan_error = Some("Can't read the SD card");
                return;
            }
        };
        self.folder = dir.to_string();
        entries.retain(|e| !e.is_dir && p4os_media::audio::is_audio_file(&e.name));
        entries.sort_by_key(|e| e.name.to_lowercase());
        for e in entries {
            let path = format!("{}/{}", dir, e.name);
            let info = sys.fs().open(&path).ok().and_then(|r| probe(&e.name, r));
            self.tracks.push(Track { name: e.name, path, info });
        }
        // Keep pointing at the same file after a rescan, if it still exists.
        if self.current.is_some_and(|i| i >= self.tracks.len()) {
            self.current = None;
        }
    }

    /// Track playback state; advance when a track finishes on its own.
    fn poll(&mut self, sys: &mut dyn System) {
        if !self.active {
            return;
        }
        match sys.audio().state() {
            PlaybackState::Playing | PlaybackState::Paused => self.seen_playing = true,
            PlaybackState::Stopped if self.seen_playing => {
                let next = self.current.and_then(|c| next_index(c, self.tracks.len()));
                match next {
                    Some(i) => self.play_from(i, sys),
                    None => self.active = false,
                }
            }
            PlaybackState::Stopped => {}
        }
    }

    /// Start track `i`; if it can't be decoded, try the following ones.
    fn play_from(&mut self, i: usize, sys: &mut dyn System) {
        let mut i = i;
        loop {
            if self.play(i, sys) {
                return;
            }
            match next_index(i, self.tracks.len()) {
                Some(n) => i = n,
                None => {
                    self.active = false;
                    return;
                }
            }
        }
    }

    fn play(&mut self, i: usize, sys: &mut dyn System) -> bool {
        let Some(track) = self.tracks.get(i) else { return false };
        self.current = Some(i);
        let source = sys.fs().open(&track.path).ok().and_then(|r| p4os_media::audio::open(&track.name, r));
        match source {
            Some(src) => {
                sys.audio().play(src);
                self.active = true;
                self.seen_playing = false;
                self.play_error = None;
                true
            }
            None => {
                self.play_error = Some(format!("Can't play \"{}\"", track.name));
                false
            }
        }
    }

    fn toggle_play(&mut self, sys: &mut dyn System) {
        let state = sys.audio().state();
        match state {
            PlaybackState::Playing if self.active => sys.audio().pause(true),
            PlaybackState::Paused if self.active => sys.audio().pause(false),
            _ if !self.tracks.is_empty() => {
                let i = self.current.unwrap_or(0);
                self.play(i, sys);
            }
            _ => {}
        }
    }

    fn next(&mut self, sys: &mut dyn System) {
        let n = match self.current {
            Some(c) => next_index(c, self.tracks.len()),
            None => (!self.tracks.is_empty()).then_some(0),
        };
        if let Some(n) = n {
            self.play_from(n, sys);
        }
    }

    fn prev(&mut self, sys: &mut dyn System) {
        if self.tracks.is_empty() {
            return;
        }
        let p = prev_index(self.current.unwrap_or(0), self.elapsed_ms(sys));
        self.play(p, sys);
    }

    fn sample_rate(&self) -> u32 {
        self.current.and_then(|i| self.tracks[i].info).map_or(44_100, |i| i.sample_rate)
    }

    fn elapsed_ms(&self, sys: &mut dyn System) -> u64 {
        if !self.active {
            return 0;
        }
        sys.audio().position_frames() * 1000 / self.sample_rate().max(1) as u64
    }

    // -- drawing ------------------------------------------------------------

    fn library(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let (bold, body, small) = (ui.fonts.bold, ui.fonts.body, ui.fonts.small);
        ui.panel(r);
        let (head, list) = r.split_top(72);
        let refresh = Rect::new(head.right() - 64, head.y + 12, 52, 52);
        ui.text(Rect::new(head.x + 20, head.y + 12, head.w - 100, 30), "Library", &bold, t.text, Align::Left);
        let sub = if self.scan_error.is_some() {
            String::new()
        } else {
            let n = self.tracks.len();
            format!("{} · {} track{}", self.folder, n, if n == 1 { "" } else { "s" })
        };
        ui.text(Rect::new(head.x + 20, head.y + 42, head.w - 100, 20), &sub, &small, t.text_dim, Align::Left);
        if ui.icon_button(refresh, |c, r, col| draw_refresh(c, r, col, t.surface)) {
            self.scan(sys);
        }
        ui.fill(Rect::new(list.x, list.y, list.w, 1), t.border);

        let list = Rect::new(list.x + 6, list.y + 6, list.w - 12, list.h - 12);
        if let Some(msg) = self.scan_error {
            ui.text(list, msg, &body, t.text_dim, Align::Center);
            return;
        }
        if self.tracks.is_empty() {
            let (a, b) = list.split_top(list.h / 2);
            ui.text(Rect::new(a.x, a.bottom() - 30, a.w, 30), "No audio files found", &body, t.text, Align::Center);
            ui.text(Rect::new(b.x, b.y, b.w, 24), "Copy .mp3 or .wav files to /sd/Music", &small, t.text_dim, Align::Center);
            return;
        }

        let off = ui.scroll(list, self.tracks.len() as i32 * ROW_H, &mut self.list_offset);
        let old = ui.canvas.set_clip(list);
        let playing = self.active && self.state == Some(PlaybackState::Playing);
        let mut tapped = None;
        for (i, track) in self.tracks.iter().enumerate() {
            let row = Rect::new(list.x, list.y + i as i32 * ROW_H - off, list.w - 10, ROW_H);
            if row.bottom() < list.y || row.y > list.bottom() {
                continue;
            }
            let is_cur = self.current == Some(i);
            if is_cur {
                ui.canvas.fill_round_rect(row.inset(2), 10, t.accent.darken(90));
            }
            if ui.input.tapped_in(row) && list.contains(ui.input.pos) {
                tapped = Some(i);
            }
            // Leading column: track number, or an equaliser on the current one.
            let lead = Rect::new(row.x + 8, row.y, 40, row.h);
            if is_cur && self.active {
                draw_bars(ui.canvas, lead, t.accent, playing);
            } else {
                let num = format!("{}", i + 1);
                ui.text(lead, &num, &body, t.text_dim, Align::Center);
            }
            let text_x = lead.right() + 10;
            let dur = track.info.and_then(|i| i.duration_ms).map(format_time).unwrap_or_default();
            let dur_w = small.measure(&dur);
            let title_w = row.right() - 14 - text_x;
            let title = ellipsize(&body, title_of(&track.name), title_w);
            ui.text(Rect::new(text_x, row.y + 9, title_w, 26), &title, &body, t.text, Align::Left);
            let fmt = match track.info {
                Some(i) => format_label(&i),
                None => ext_label(&track.name).to_string(),
            };
            ui.text(Rect::new(text_x, row.y + 36, title_w - dur_w - 8, 20), &fmt, &small, t.text_dim, Align::Left);
            ui.text(Rect::new(row.right() - 14 - dur_w, row.y + 36, dur_w, 20), &dur, &small, t.text_dim, Align::Right);
            if !is_cur {
                ui.canvas.fill_rect(Rect::new(text_x, row.bottom() - 1, row.right() - 14 - text_x, 1), t.border);
            }
        }
        ui.canvas.restore_clip(old);
        if let Some(i) = tapped {
            self.play(i, sys);
        }
    }

    fn now_playing(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let (body, small, title_f) = (ui.fonts.body, ui.fonts.small, ui.fonts.title);
        ui.panel(r);
        let inner = r.inset(24);

        // Artwork and track details.
        let art = Rect::new(inner.x, inner.y, 176, 176);
        crate::icons::tile(ui.canvas, art, ART_TOP, ART_BOTTOM);
        draw_note(ui.canvas, art.inset(40), Color::WHITE);
        let info_x = art.right() + 24;
        let info_w = inner.right() - info_x;
        let track = self.current.and_then(|i| self.tracks.get(i));
        let state = sys.audio().state();
        let label = match (self.active, state) {
            (true, PlaybackState::Playing) => "NOW PLAYING",
            (true, PlaybackState::Paused) => "PAUSED",
            _ if track.is_some() => "STOPPED",
            _ => "NOTHING PLAYING",
        };
        ui.text(Rect::new(info_x, inner.y + 4, info_w, 20), label, &small, t.accent, Align::Left);
        let title = track.map_or("Choose a track", |tr| title_of(&tr.name));
        let mut lines = title_f.wrap(title, info_w);
        if lines.len() > 2 {
            let rest = lines[1..].join(" ");
            lines.truncate(1);
            lines.push(ellipsize(&title_f, &rest, info_w));
        }
        let mut y = inner.y + 30;
        for line in &lines {
            ui.text(Rect::new(info_x, y, info_w, 40), line, &title_f, t.text, Align::Left);
            y += 40;
        }
        if let Some(tr) = track {
            let detail = match tr.info {
                Some(i) => format!("{} · {}", format_label(&i), channel_label(i.channels)),
                None => ext_label(&tr.name).to_string(),
            };
            ui.text(Rect::new(info_x, y + 6, info_w, 24), &detail, &body, t.text_dim, Align::Left);
            let pos = format!("Track {} of {}", self.current.unwrap_or(0) + 1, self.tracks.len());
            ui.text(Rect::new(info_x, y + 34, info_w, 22), &pos, &small, t.text_dim, Align::Left);
        }
        if let Some(err) = &self.play_error {
            let e = ellipsize(&small, err, info_w);
            ui.text(Rect::new(info_x, art.bottom() - 22, info_w, 22), &e, &small, t.danger, Align::Left);
        }

        // Progress and times.
        let elapsed = self.elapsed_ms(sys);
        let total = track.and_then(|tr| tr.info).and_then(|i| i.duration_ms);
        let estimated = track.and_then(|tr| tr.info).is_some_and(|i| !i.exact);
        let bar = Rect::new(inner.x, art.bottom() + 28, inner.w, 8);
        let frac = match total {
            Some(tot) if tot > 0 => elapsed as f32 / tot as f32,
            _ => 0.0,
        };
        ui.progress(bar, frac);
        let times = Rect::new(inner.x, bar.bottom() + 6, inner.w, 22);
        ui.text(times, &format_time(elapsed), &small, t.text_dim, Align::Left);
        if let Some(tot) = total {
            let s = if estimated { format!("~{}", format_time(tot)) } else { format_time(tot) };
            ui.text(times, &s, &small, t.text_dim, Align::Right);
        }

        // Transport controls, centred in the space between times and volume.
        let (controls, vol) = Rect::new(inner.x, times.bottom(), inner.w, inner.bottom() - times.bottom()).split_top(
            inner.bottom() - times.bottom() - 56,
        );
        let cy = controls.y + controls.h / 2;
        let cx = controls.x + controls.w / 2;
        let big = Rect::new(cx - 48, cy - 48, 96, 96);
        let prev = Rect::new(big.x - 40 - 72, cy - 36, 72, 72);
        let next = Rect::new(big.right() + 40, cy - 36, 72, 72);
        let is_playing = self.active && state == PlaybackState::Playing;
        let (held, clicked) = ui.interact(big);
        let face = if held { t.accent.lighten(40) } else { t.accent };
        ui.canvas.fill_circle(big.center(), 48.0, face);
        if is_playing {
            draw_pause(ui.canvas, big.inset(30), t.accent_text);
        } else {
            draw_play(ui.canvas, big.inset(30), t.accent_text);
        }
        if clicked {
            self.toggle_play(sys);
        }
        if ui.icon_button(prev, |c, r, col| draw_skip(c, r.inset(20), col, false)) {
            self.prev(sys);
        }
        if ui.icon_button(next, |c, r, col| draw_skip(c, r.inset(20), col, true)) {
            self.next(sys);
        }

        // Volume.
        let vol = Rect::new(vol.x, vol.bottom() - 52, vol.w, 52);
        draw_speaker(ui.canvas, Rect::new(vol.x, vol.y + 12, 28, 28), t.text_dim, self.volume);
        let pct = format!("{}%", libm::roundf(self.volume * 100.0) as i32);
        let slider = Rect::new(vol.x + 52, vol.y, vol.w - 52 - 72, vol.h);
        if ui.slider(slider, &mut self.volume) {
            sys.audio().set_volume(self.volume);
        }
        ui.text(Rect::new(slider.right() + 16, vol.y, 56, vol.h), &pct, &body, t.text_dim, Align::Right);
    }
}

fn ext_label(name: &str) -> &'static str {
    if name.to_ascii_lowercase().ends_with(".wav") { "WAV" } else { "MP3" }
}

/// "MP3 · 128 kbps · 44.1 kHz", "MP3 · VBR 84 kbps · 44.1 kHz" or "WAV · 44.1 kHz".
fn format_label(i: &TrackInfo) -> String {
    let khz = i.sample_rate / 100;
    let rate = if khz % 10 == 0 { format!("{} kHz", khz / 10) } else { format!("{}.{} kHz", khz / 10, khz % 10) };
    match i.format {
        Format::Mp3 if i.vbr => format!("MP3 · VBR {} kbps · {}", i.kbps, rate),
        Format::Mp3 => format!("MP3 · {} kbps · {}", i.kbps, rate),
        Format::Wav => format!("WAV · {}", rate),
    }
}

fn channel_label(ch: u8) -> &'static str {
    if ch == 1 { "Mono" } else { "Stereo" }
}

// -- icons --------------------------------------------------------------------

fn draw_play(c: &mut Canvas, r: Rect, col: Color) {
    // Nudge right so the triangle looks optically centred.
    let x = r.x + r.w / 8;
    c.fill_triangle([Point::new(x, r.y), Point::new(x, r.bottom()), Point::new(x + r.w, r.y + r.h / 2)], col);
}

fn draw_pause(c: &mut Canvas, r: Rect, col: Color) {
    let w = r.w / 3;
    c.fill_round_rect(Rect::new(r.x + 2, r.y, w, r.h), 3, col);
    c.fill_round_rect(Rect::new(r.right() - 2 - w, r.y, w, r.h), 3, col);
}

/// Skip-to-next (`forward`) or skip-to-previous icon: triangle plus bar.
fn draw_skip(c: &mut Canvas, r: Rect, col: Color, forward: bool) {
    let bar = (r.w / 7).max(3);
    let (tip, base, bar_x) = if forward {
        (r.right() - bar, r.x, r.right() - bar)
    } else {
        (r.x + bar, r.right(), r.x)
    };
    c.fill_triangle([Point::new(base, r.y), Point::new(base, r.bottom()), Point::new(tip, r.y + r.h / 2)], col);
    c.fill_rect(Rect::new(bar_x, r.y, bar, r.h), col);
}

fn draw_speaker(c: &mut Canvas, r: Rect, col: Color, volume: f32) {
    let body = Rect::new(r.x, r.y + r.h * 3 / 10, r.w / 4, r.h * 4 / 10);
    c.fill_rect(body, col);
    let cone_x = body.right() + r.w / 4;
    c.fill_triangle([Point::new(body.x + 1, r.y + r.h / 2), Point::new(cone_x, r.y), Point::new(cone_x, r.bottom())], col);
    // Sound waves: one per third of the volume range.
    let cy = r.y + r.h / 2;
    for k in 0..3 {
        if volume > k as f32 / 3.0 + 0.01 {
            let x = cone_x + 4 + k * 6;
            let h = r.h / 5 + k * r.h / 6;
            c.fill_rect(Rect::new(x, cy - h / 2, 3, h), col);
        }
    }
}

/// Small equaliser marking the current track; flat bars while paused.
fn draw_bars(c: &mut Canvas, r: Rect, col: Color, animated: bool) {
    const H: [i32; 3] = [14, 22, 10];
    let base = r.y + r.h / 2 + 12;
    for (k, h) in H.iter().enumerate() {
        let h = if animated { *h } else { 6 };
        c.fill_rect(Rect::new(r.x + 9 + k as i32 * 8, base - h, 5, h), col);
    }
}

fn draw_refresh(c: &mut Canvas, r: Rect, col: Color, bg: Color) {
    let center = r.center();
    let rad = r.w as f32 / 5.0;
    c.stroke_circle(center, rad, 3.0, col);
    // Gap with an arrow head at the top right of the ring.
    let gap = Rect::new(center.x, center.y - rad as i32 - 3, rad as i32 + 4, rad as i32);
    c.fill_rect(gap, bg);
    let tip = Point::new(center.x + 1, center.y - rad as i32);
    c.fill_triangle([Point::new(tip.x + 9, tip.y), Point::new(tip.x, tip.y - 7), Point::new(tip.x, tip.y + 7)], col);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_header(channels: u16, rate: u32, data_len: u32, extra_chunk: bool) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        h.extend_from_slice(&(channels * 2).to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        if extra_chunk {
            h.extend_from_slice(b"LIST");
            h.extend_from_slice(&5u32.to_le_bytes());
            h.extend_from_slice(b"abcde\0"); // odd length plus pad byte
        }
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        h
    }

    #[test]
    fn time_formatting() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(999), "0:00");
        assert_eq!(format_time(61_000), "1:01");
        assert_eq!(format_time(10 * 60_000 + 5_000), "10:05");
        assert_eq!(format_time(3_600_000 + 2 * 60_000 + 3_000), "1:02:03");
    }

    #[test]
    fn playlist_navigation() {
        assert_eq!(next_index(0, 3), Some(1));
        assert_eq!(next_index(2, 3), None);
        assert_eq!(next_index(0, 0), None);
        assert_eq!(prev_index(2, 500), 1);
        assert_eq!(prev_index(2, 10_000), 2);
        assert_eq!(prev_index(0, 500), 0);
    }

    #[test]
    fn titles_drop_extension() {
        assert_eq!(title_of("Bell Tones.wav"), "Bell Tones");
        assert_eq!(title_of("a.b.mp3"), "a.b");
        assert_eq!(title_of(".hidden"), ".hidden");
    }

    #[test]
    fn wav_duration_from_data_chunk() {
        // 2 s of 44.1 kHz stereo 16-bit.
        let h = wav_header(2, 44_100, 44_100 * 4 * 2, true);
        let i = probe_wav(&h, h.len() as u64 + 44_100 * 8).unwrap();
        assert_eq!((i.format, i.sample_rate, i.channels, i.duration_ms), (Format::Wav, 44_100, 2, Some(2000)));
        // Streaming writers leave the size at 0: fall back to the file size.
        let h = wav_header(1, 8000, 0, false);
        let i = probe_wav(&h, h.len() as u64 + 8000 * 2 * 3).unwrap();
        assert_eq!(i.duration_ms, Some(3000));
        assert!(probe_wav(b"RIFF\0\0\0\0AVI ", 100).is_none());
    }

    #[test]
    fn id3_tag_length() {
        let mut h = *b"ID3\x04\x00\x00\x00\x00\x02\x01";
        assert_eq!(id3v2_len(&h), 10 + 257);
        h[5] = 0x10; // footer present
        assert_eq!(id3v2_len(&h), 10 + 257 + 10);
        assert_eq!(id3v2_len(b"\xff\xfb\x90\x00"), 0);
    }

    #[test]
    fn mp3_cbr_estimate() {
        // MPEG1 Layer III, 128 kbps, 44.1 kHz, joint stereo, no Xing frame.
        let mut f = vec![0u8; 600];
        f[2..6].copy_from_slice(&[0xff, 0xfb, 0x90, 0x40]); // two junk bytes first
        let i = probe_mp3(&f, 2 + 160_000).unwrap();
        assert_eq!((i.kbps, i.sample_rate, i.channels, i.exact), (128, 44_100, 2, false));
        assert_eq!(i.duration_ms, Some(10_000));
    }

    #[test]
    fn mp3_xing_frame_count() {
        let mut f = vec![0u8; 600];
        f[..4].copy_from_slice(&[0xff, 0xfb, 0x50, 0xc4]); // 64 kbps mono
        f[21..25].copy_from_slice(b"Xing"); // 4 + 17 bytes side info
        f[25..29].copy_from_slice(&1u32.to_be_bytes());
        f[29..33].copy_from_slice(&383u32.to_be_bytes());
        let i = probe_mp3(&f, 1_000_000).unwrap();
        assert_eq!((i.channels, i.exact), (1, true));
        assert_eq!(i.duration_ms, Some(383 * 1152 * 1000 / 44_100));
    }

    struct SliceReader(&'static [u8], usize);

    impl FileReader for SliceReader {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, FsError> {
            let n = buf.len().min(self.0.len() - self.1).min(1000);
            buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
            self.1 += n;
            Ok(n)
        }
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
    }

    /// The real test tracks shipped in sim/sdcard/Music (lengths checked
    /// against ffprobe: 20.0 s, 16.3 s and 12.0 s).
    #[test]
    fn probes_sample_library() {
        let cases: [(&str, &'static [u8], u64); 4] = [
            ("Morning Chords.wav", include_bytes!("../../sim/sdcard/Music/Morning Chords.wav"), 20_000),
            ("Morning Chords.mp3", include_bytes!("../../sim/sdcard/Music/Morning Chords.mp3"), 20_000),
            ("Arpeggio in A Minor.mp3", include_bytes!("../../sim/sdcard/Music/Arpeggio in A Minor.mp3"), 16_300),
            ("Bell Tones (VBR).mp3", include_bytes!("../../sim/sdcard/Music/Bell Tones (VBR).mp3"), 12_000),
        ];
        for (name, data, expect) in cases {
            let info = probe(name, Box::new(SliceReader(data, 0))).unwrap();
            let got = info.duration_ms.unwrap();
            assert!(got.abs_diff(expect) < 150, "{name}: {got} ms, expected ~{expect}");
            assert_eq!(info.sample_rate, 44_100);
        }
        let vbr = probe("v.mp3", Box::new(SliceReader(cases[3].1, 0))).unwrap();
        assert!(vbr.vbr && vbr.exact && (70..100).contains(&vbr.kbps), "{vbr:?}");
    }

    #[test]
    fn ellipsis_fits_width() {
        let fonts = p4os_sdk::gui::font::builtin::load();
        let f = fonts.body;
        assert_eq!(ellipsize(&f, "Short", 200), "Short");
        let s = ellipsize(&f, "A very long track title that does not fit", 150);
        assert!(s.ends_with('…') && f.measure(&s) <= 150);
    }
}
