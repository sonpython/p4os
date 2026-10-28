//! Host-test helpers for apps: an in-memory [`System`] and a harness that
//! renders frames into a RAM canvas and feeds taps, so an app's `update`
//! can be driven from a unit test without the board or the simulator.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use p4os_sdk::gui::font::builtin::{self, Fonts};
use p4os_sdk::gui::ui::{Input, Theme, UiState};
use p4os_sdk::{
    config, App, Audio, AudioSource, Canvas, Color, DateTime, DirEntry, FetchId, FetchState, FileReader, FileSystem,
    FsError, Image, Network, Orientation, PlaybackState, Point, Rect, System, SystemStats, Ui,
};

/// Files kept in memory. `absent: true` behaves like a missing SD card.
#[derive(Default)]
pub struct MemFs {
    pub files: BTreeMap<String, Vec<u8>>,
    pub absent: bool,
}

impl FileSystem for MemFs {
    fn list(&mut self, _path: &str) -> Result<Vec<DirEntry>, FsError> {
        Err(FsError::Unsupported)
    }
    fn read(&mut self, path: &str) -> Result<Vec<u8>, FsError> {
        if self.absent {
            return Err(FsError::NoMedium);
        }
        self.files.get(path).cloned().ok_or(FsError::NotFound)
    }
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        if self.absent {
            return Err(FsError::NoMedium);
        }
        self.files.insert(path.to_string(), data.to_vec());
        Ok(())
    }
    fn open(&mut self, _path: &str) -> Result<Box<dyn FileReader>, FsError> {
        Err(FsError::Unsupported)
    }
}

/// Audio that plays nothing.
#[derive(Default)]
pub struct NoAudio;

impl Audio for NoAudio {
    fn play(&mut self, _source: Box<dyn AudioSource>) {}
    fn pause(&mut self, _paused: bool) {}
    fn stop(&mut self) {}
    fn state(&self) -> PlaybackState {
        PlaybackState::Stopped
    }
    fn position_frames(&self) -> u64 {
        0
    }
    fn set_volume(&mut self, _volume: f32) {}
    fn volume(&self) -> f32 {
        1.0
    }
}

/// A network whose responses the test sets. Like the kernel, it hands out
/// a finished result once and then forgets the request.
#[derive(Default)]
pub struct FakeNet {
    pub up: bool,
    /// URLs requested so far, in order; request ids count from 1.
    pub requested: Vec<String>,
    /// Ids passed to `cancel`.
    pub cancelled: Vec<FetchId>,
    results: BTreeMap<FetchId, FetchState>,
}

impl FakeNet {
    /// Finish the most recent request with `state`.
    pub fn complete_last(&mut self, state: FetchState) {
        let id = self.requested.len() as FetchId;
        self.results.insert(id, state);
    }
}

impl Network for FakeNet {
    fn status(&self) -> String {
        if self.up { "test network".into() } else { "offline".into() }
    }
    fn is_up(&self) -> bool {
        self.up
    }
    fn fetch(&mut self, url: &str) -> FetchId {
        self.requested.push(url.to_string());
        let id = self.requested.len() as FetchId;
        self.results.insert(id, FetchState::Pending);
        id
    }
    fn poll(&mut self, id: FetchId) -> FetchState {
        match self.results.get(&id) {
            Some(FetchState::Pending) => FetchState::Pending,
            Some(_) => self.results.remove(&id).unwrap_or(FetchState::Pending),
            None => FetchState::Failed("unknown request".into()),
        }
    }
    fn cancel(&mut self, id: FetchId) {
        self.cancelled.push(id);
        self.results.remove(&id);
    }
}

/// An in-memory [`System`]: the clock only moves when the test sets
/// `uptime`, settings live in a map and count their writes.
#[derive(Default)]
pub struct TestSystem {
    pub uptime: u64,
    pub now: Option<DateTime>,
    pub fs: MemFs,
    pub audio: NoAudio,
    pub net: FakeNet,
    pub config: BTreeMap<String, String>,
    pub config_writes: u32,
}

impl System for TestSystem {
    fn uptime_ms(&self) -> u64 {
        self.uptime
    }
    fn now(&self) -> Option<DateTime> {
        self.now
    }
    fn set_time(&mut self, t: DateTime) {
        self.now = Some(t);
    }
    fn fs(&mut self) -> &mut dyn FileSystem {
        &mut self.fs
    }
    fn audio(&mut self) -> &mut dyn Audio {
        &mut self.audio
    }
    fn net(&mut self) -> &mut dyn Network {
        &mut self.net
    }
    fn stats(&self) -> SystemStats {
        SystemStats { uptime_ms: self.uptime, ..Default::default() }
    }
    fn decode_image(&mut self, _data: &[u8], _max_w: i32, _max_h: i32) -> Option<Image> {
        None
    }
    fn brightness(&self) -> f32 {
        1.0
    }
    fn set_brightness(&mut self, _value: f32) {}
    fn run_command(&mut self, _line: &str) -> String {
        String::new()
    }
    fn launch(&mut self, _app_id: &str) {}
    fn config_get(&self, key: &str) -> Option<String> {
        if config::is_secret(key) {
            return None;
        }
        self.config.get(key).cloned()
    }
    fn config_set(&mut self, key: &str, value: &str) {
        // The kernel refuses entries the flash record cannot hold; so do we.
        if config::check_entry(key, value).is_ok() {
            self.config_writes += 1;
            self.config.insert(key.into(), value.into());
        }
    }
    fn config_remove(&mut self, key: &str) {
        if self.config.remove(key).is_some() {
            self.config_writes += 1;
        }
    }
}

/// Renders an app into a RAM framebuffer, one frame per call.
pub struct Harness {
    pub sys: TestSystem,
    pub orientation: Orientation,
    fb: Vec<Color>,
    fonts: Fonts,
    theme: Theme,
    state: UiState,
    input: Input,
}

impl Harness {
    pub fn new() -> Self {
        Self {
            sys: TestSystem::default(),
            orientation: Orientation::Portrait,
            fb: vec![Color::BLACK; 480 * 800],
            fonts: builtin::load(),
            theme: Theme::dark(),
            state: UiState::default(),
            input: Input::default(),
        }
    }

    /// Run one frame with the current input over the whole screen.
    pub fn frame(&mut self, app: &mut dyn App) {
        let (w, h) = self.orientation.size();
        let mut canvas = Canvas::new(&mut self.fb, w, h);
        let area = Rect::new(0, 0, w, h);
        let mut ui = Ui { canvas: &mut canvas, input: &self.input, theme: &self.theme, fonts: &self.fonts, state: &mut self.state };
        app.update(&mut ui, area, &mut self.sys);
        // Edge-triggered input only lasts one frame, as on the board.
        self.input.pressed = false;
        self.input.released = false;
        self.input.keys.clear();
    }

    /// Touch down and up at `p`, one frame each, then one idle frame so
    /// state changed by the tap is drawn.
    pub fn tap(&mut self, app: &mut dyn App, p: Point) {
        self.input = Input { pos: p, press_pos: p, down: true, pressed: true, ..Default::default() };
        self.frame(app);
        self.input = Input { pos: p, press_pos: p, released: true, ..Default::default() };
        self.frame(app);
        self.frame(app);
    }
}
