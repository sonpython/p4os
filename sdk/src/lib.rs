//! The p4os application SDK.
//!
//! An app implements [`App`]. The OS calls [`App::update`] whenever there is
//! input or a requested tick, passing an immediate-mode [`Ui`] and a
//! [`System`] handle that exposes OS services. Long-running work (audio
//! playback, network fetches) runs in OS threads behind those services, so
//! `update` never blocks.

#![no_std]

extern crate alloc;

pub use p4os_gui as gui;
pub use p4os_gui::{Canvas, Color, Point, Rect, Ui};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

pub trait App {
    /// Draw the app into `area` and react to this frame's input.
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System);

    /// Ask to be updated periodically even without input (animations,
    /// clocks, playback position). `None` means only on input.
    fn tick_interval_ms(&self) -> Option<u32> {
        None
    }

    /// Called when the user leaves the app; release audio, etc.
    fn on_close(&mut self, _sys: &mut dyn System) {}
}

/// Static description of an installed app, used by the launcher.
pub struct AppInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// Draw the launcher icon into the given square.
    pub icon: fn(&mut Canvas, Rect),
    pub create: fn() -> Box<dyn App>,
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsError {
    NotFound,
    NotADirectory,
    NoMedium,
    Io,
    Unsupported,
}

/// Sequential reader over an open file.
pub trait FileReader: Send {
    /// Read up to `buf.len()` bytes; 0 means end of file.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, FsError>;
    fn size(&self) -> u64;
}

/// Paths are absolute and `/`-separated; `/sd` is the SD card.
pub trait FileSystem {
    fn list(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError>;
    fn read(&mut self, path: &str) -> Result<Vec<u8>, FsError>;
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError>;
    fn open(&mut self, path: &str) -> Result<Box<dyn FileReader>, FsError>;
    /// Create a directory (the parent must exist).
    fn create_dir(&mut self, _path: &str) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }
    /// Delete a file or an empty directory.
    fn remove(&mut self, _path: &str) -> Result<(), FsError> {
        Err(FsError::Unsupported)
    }
}

/// Produces interleaved stereo 16-bit PCM for the audio thread.
pub trait AudioSource: Send {
    fn sample_rate(&self) -> u32;
    /// Fill `out` with interleaved L/R samples; return the number written.
    /// Returning 0 ends the stream.
    fn fill(&mut self, out: &mut [i16]) -> usize;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

pub trait Audio {
    /// Replace whatever is playing with `source`.
    fn play(&mut self, source: Box<dyn AudioSource>);
    fn pause(&mut self, paused: bool);
    fn stop(&mut self);
    fn state(&self) -> PlaybackState;
    /// Frames (stereo sample pairs) played since `play`.
    fn position_frames(&self) -> u64;
    fn set_volume(&mut self, volume: f32);
    fn volume(&self) -> f32;
}

pub type FetchId = u32;

#[derive(Clone, Debug)]
pub enum FetchState {
    Pending,
    Done { status: u16, content_type: String, body: Vec<u8> },
    Failed(String),
}

pub trait Network {
    /// Human-readable link state, e.g. "Ethernet 192.168.1.20".
    fn status(&self) -> String;
    fn is_up(&self) -> bool;
    /// Start an HTTP GET in the background.
    fn fetch(&mut self, url: &str) -> FetchId;
    /// Current state of a fetch. Once `Done` or `Failed` has been
    /// returned the implementation may forget the id.
    fn poll(&mut self, id: FetchId) -> FetchState;
}

#[derive(Clone, Debug)]
pub struct ThreadStat {
    pub name: String,
    pub priority: u8,
    pub state: String,
    pub cpu_ms: u32,
    pub stack_used: u32,
    pub stack_size: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub uptime_ms: u64,
    pub cpu_mhz: u32,
    /// Share of time spent outside the idle thread over the last second.
    pub cpu_load: f32,
    pub heap_used: usize,
    pub heap_total: usize,
    pub psram_used: usize,
    pub psram_total: usize,
    pub threads: Vec<ThreadStat>,
    pub chip: String,
}

/// A decoded image in display pixel format.
pub struct Image {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<Color>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Screen orientation. The panel is natively 480x800 portrait; landscape
/// (800x480) is rotated in the display pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

impl Orientation {
    /// Logical screen size (width, height) in this orientation.
    pub const fn size(self) -> (i32, i32) {
        match self {
            Orientation::Portrait => (480, 800),
            Orientation::Landscape => (800, 480),
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Orientation::Portrait => Orientation::Landscape,
            Orientation::Landscape => Orientation::Portrait,
        }
    }
}

/// OS services available to apps.
pub trait System {
    fn uptime_ms(&self) -> u64;
    /// Wall-clock time when known (set from the network or by the user).
    fn now(&self) -> Option<DateTime>;
    fn set_time(&mut self, t: DateTime);
    fn fs(&mut self) -> &mut dyn FileSystem;
    fn audio(&mut self) -> &mut dyn Audio;
    fn net(&mut self) -> &mut dyn Network;
    fn stats(&self) -> SystemStats;
    /// Decode JPEG/BMP data, scaled down so it fits within max_w x max_h.
    fn decode_image(&mut self, data: &[u8], max_w: i32, max_h: i32) -> Option<Image>;
    fn brightness(&self) -> f32;
    fn set_brightness(&mut self, value: f32);
    /// Run a shell command and return its output (terminal app).
    fn run_command(&mut self, line: &str) -> String;
    /// Ask the desktop to switch to another app by id.
    fn launch(&mut self, app_id: &str);
    fn orientation(&self) -> Orientation {
        Orientation::Portrait
    }
    /// Request a rotation; takes effect from the next frame.
    fn set_orientation(&mut self, _o: Orientation) {}
}
