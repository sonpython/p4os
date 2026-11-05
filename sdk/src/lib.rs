//! The p4os application SDK.
//!
//! An app implements [`App`]. The OS calls [`App::update`] whenever there is
//! input or a requested tick, passing an immediate-mode [`Ui`] and a
//! [`System`] handle that exposes OS services. Long-running work (audio
//! playback, network fetches) runs in OS threads behind those services, so
//! `update` never blocks.

#![no_std]

extern crate alloc;

pub mod config;

pub use p4os_gui as gui;
pub use p4os_gui::{Canvas, Color, Point, Rect, Ui};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

pub trait App {
    /// Draw the app into `area` and react to this frame's input.
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System);

    /// Ask to be updated periodically even without input (animations,
    /// clocks, playback position). `None` means only on input. While the
    /// app is in the background this paces [`App::background_tick`].
    fn tick_interval_ms(&self) -> Option<u32> {
        None
    }

    /// The system Back gesture (swipe in from the left edge, or Escape).
    /// Return `true` if the app moved back (closed a page, a detail view,
    /// went up a folder); `false` at the app's root, which sends the user
    /// to the home screen.
    fn on_back(&mut self, _sys: &mut dyn System) -> bool {
        false
    }

    /// The system Forward gesture (swipe in from the right edge). Return
    /// `true` if it was handled.
    fn on_forward(&mut self, _sys: &mut dyn System) -> bool {
        false
    }

    /// The app left the foreground. It keeps its state, but it is no longer
    /// drawn and gets no CPU unless [`App::background_work`] asks for a
    /// capability the app declared in [`AppInfo::background`]. Save
    /// anything the user would hate to lose: a paused app may be stopped
    /// later without another chance to run.
    fn on_pause(&mut self, _sys: &mut dyn System) {}

    /// The app is back in the foreground after [`App::on_pause`].
    fn on_resume(&mut self, _sys: &mut dyn System) {}

    /// The system is short of memory: drop caches that can be rebuilt
    /// (decoded images, layouts). Apps that keep using too much memory in
    /// the background are stopped.
    fn on_low_memory(&mut self, _sys: &mut dyn System) {}

    /// The app is about to be destroyed (closed by the user, from the app
    /// switcher, or by the system). Release audio, save state.
    fn on_stop(&mut self, _sys: &mut dyn System) {}

    /// Background work the app needs right now (for example
    /// [`Capabilities::AUDIO`] while music plays). Only capabilities also
    /// declared in [`AppInfo::background`] are granted; without any, a
    /// backgrounded app is suspended.
    fn background_work(&self) -> Capabilities {
        Capabilities::NONE
    }

    /// Periodic work while the app is not drawn but has a granted
    /// capability: in the background, or in the foreground while the screen
    /// is locked or off (media keeps playing, as on a phone). Returns how
    /// soon to be called again in ms; `None` uses [`App::tick_interval_ms`].
    /// Limited by the background CPU budget. No drawing happens here.
    fn background_tick(&mut self, _sys: &mut dyn System) -> Option<u32> {
        None
    }

    /// Take the whole screen this frame, without the status bar, title bar
    /// and home bar (video playback). The system gestures still work
    /// (swipe up from the bottom edge goes home), and so does
    /// `System::launch("")` and the Escape key.
    fn fullscreen(&self) -> bool {
        false
    }

    /// Keep the screen from dimming and turning off (video playing).
    fn keep_awake(&self) -> bool {
        false
    }
}

/// Kinds of work an app may keep doing in the background.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Capabilities(u8);

impl Capabilities {
    pub const NONE: Capabilities = Capabilities(0);
    /// Keep feeding audio playback (playlist advance).
    pub const AUDIO: Capabilities = Capabilities(1 << 0);
    /// Finish a network transfer that is in progress.
    pub const NETWORK: Capabilities = Capabilities(1 << 1);

    pub const fn union(self, other: Capabilities) -> Capabilities {
        Capabilities(self.0 | other.0)
    }

    pub const fn intersect(self, other: Capabilities) -> Capabilities {
        Capabilities(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Capabilities) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Lifecycle state of a running app.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppState {
    /// On screen and receiving input.
    Foreground,
    /// Off screen, doing granted background work (ticks, no drawing).
    Background,
    /// Off screen, state kept, no CPU at all.
    Suspended,
    /// Destroyed.
    Stopped,
}

/// Identifies a running app for [`System::close_app`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppHandle {
    /// A built-in app run by the desktop (its slot in the running list).
    Builtin(u8),
    /// An isolated user-mode app (its kernel thread id).
    User(u32),
}

/// Resource use of one running app, for task managers.
#[derive(Clone, Debug)]
pub struct AppUsage {
    pub handle: AppHandle,
    pub name: String,
    pub state: AppState,
    /// Share of one CPU core used over the last second, in 1/1000.
    pub cpu_permille: u16,
    /// CPU time used since the app started.
    pub cpu_ms: u64,
    /// Heap attributed to the app, internal SRAM and PSRAM.
    pub sram: usize,
    pub psram: usize,
    /// Short status such as "audio" or "throttled"; may be empty.
    pub note: String,
    /// How long the app's last on-screen frame took to draw (its own
    /// drawing only, without compositing and display), 0 if none yet.
    pub frame_us: u32,
}

/// Static description of an installed app, used by the launcher.
pub struct AppInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// Draw the launcher icon into the given square.
    pub icon: fn(&mut Canvas, Rect),
    pub create: fn() -> Box<dyn App>,
    /// Capabilities the app may use in the background (its manifest).
    pub background: Capabilities,
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

/// An HTTP request with a method, extra headers and a body, for APIs that
/// need more than a plain GET (OAuth token exchanges, bearer tokens).
///
/// The OS adds Host, Accept-Encoding, Connection and Content-Length
/// itself; requests repeating them, or with control characters in a header,
/// fail. User-Agent defaults to p4os's and may be replaced by a header.
/// Bodies are limited to 2 KiB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn get(url: &str) -> Self {
        Self { method: "GET".into(), url: url.into(), headers: Vec::new(), body: Vec::new() }
    }

    /// A POST of an `application/x-www-form-urlencoded` body.
    pub fn post_form(url: &str, body: &str) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            headers: alloc::vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    /// A POST of `body` with the given content type (e.g. JSON).
    pub fn post(url: &str, content_type: &str, body: Vec<u8>) -> Self {
        Self { method: "POST".into(), url: url.into(), headers: alloc::vec![("Content-Type".into(), content_type.into())], body }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Ask for bytes `start..=end` of the resource (HTTP status 206).
    pub fn range(self, start: u64, end: u64) -> Self {
        let value = alloc::format!("bytes={start}-{end}");
        self.header("Range", &value)
    }

    /// Plain GETs are all [`Network::fetch`] can do.
    pub fn is_plain_get(&self) -> bool {
        self.method == "GET" && self.headers.is_empty() && self.body.is_empty()
    }
}

pub trait Network {
    /// Human-readable link state, e.g. "Ethernet 192.168.1.20".
    fn status(&self) -> String;
    fn is_up(&self) -> bool;
    /// Start an HTTP GET in the background.
    fn fetch(&mut self, url: &str) -> FetchId;
    /// Start any HTTP request in the background; poll it like a fetch.
    /// Redirects to another origin drop the Authorization header.
    ///
    /// The default supports plain GETs only; anything else is sent to an
    /// invalid URL so that it fails instead of silently losing its method,
    /// headers or body.
    fn send(&mut self, req: HttpRequest) -> FetchId {
        if req.is_plain_get() {
            self.fetch(&req.url)
        } else {
            self.fetch("unsupported:request-with-method-headers-or-body")
        }
    }
    /// Current state of a fetch. Once `Done` or `Failed` has been
    /// returned the implementation may forget the id.
    fn poll(&mut self, id: FetchId) -> FetchState;
    /// Abandon a request (the app is closing or no longer needs it): the
    /// OS stops it and frees its result. Polling the id afterwards reports
    /// an unknown request. The default does nothing.
    fn cancel(&mut self, _id: FetchId) {}
}

#[derive(Clone, Debug)]
pub struct ThreadStat {
    pub name: String,
    pub priority: u8,
    pub state: String,
    pub cpu_ms: u32,
    pub stack_used: u32,
    pub stack_size: u32,
    /// CPU core running the thread now, or the one that ran it last.
    pub core: Option<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub uptime_ms: u64,
    pub cpu_mhz: u32,
    /// Share of time spent outside the idle threads over the last second,
    /// averaged over all cores.
    pub cpu_load: f32,
    /// The same share for each CPU core.
    pub core_loads: Vec<f32>,
    pub heap_used: usize,
    pub heap_total: usize,
    pub psram_used: usize,
    pub psram_total: usize,
    pub threads: Vec<ThreadStat>,
    pub chip: String,
    /// Battery voltage in millivolts, when the board can measure it.
    pub battery_mv: Option<u32>,
    /// One-line summary of the last crash, empty if none was recorded.
    pub last_crash: String,
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

/// Services for code running on a worker thread from [`System::spawn_worker`].
pub trait Worker {
    /// Block the calling worker thread for at least `ms` milliseconds.
    fn sleep_ms(&self, ms: u32);
    /// Microseconds since boot, for timing work.
    fn now_us(&self) -> u64;
}

/// Body of a worker thread.
pub type WorkerJob = Box<dyn FnOnce(&dyn Worker) + Send>;

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
    /// Ask the desktop to switch to another app by id; an empty id goes
    /// back to the home screen.
    fn launch(&mut self, app_id: &str);
    fn orientation(&self) -> Orientation {
        Orientation::Portrait
    }
    /// Request a rotation; takes effect from the next frame.
    fn set_orientation(&mut self, _o: Orientation) {}
    /// Join a Wi-Fi network in the background and remember the credentials
    /// so the OS reconnects at boot. Check [`Network::status`] for progress.
    fn wifi_connect(&mut self, _ssid: &str, _password: &str) {}
    /// Read a persistent setting (keys in [`config`]). Secret values such
    /// as the Wi-Fi password are write-only and read back as `None`.
    fn config_get(&self, _key: &str) -> Option<String> {
        None
    }
    /// Store a persistent setting; it survives reboots.
    fn config_set(&mut self, _key: &str, _value: &str) {}
    /// Delete a persistent setting. The default stores an empty value,
    /// which readers of optional settings treat as unset.
    fn config_remove(&mut self, key: &str) {
        self.config_set(key, "");
    }
    /// Microseconds since boot, for measuring short durations.
    fn uptime_us(&self) -> u64 {
        self.uptime_ms() * 1000
    }
    /// Write a line to the system log (the serial console on the board).
    fn log(&mut self, _msg: &str) {}
    /// Attribute heap allocations made by the calling thread from now on to
    /// `owner` (0 is the system). Returns the previous owner. Used by the
    /// desktop around every call into an app.
    fn set_heap_owner(&mut self, _owner: u8) -> u8 {
        0
    }
    /// Heap bytes (SRAM, PSRAM) currently attributed to `owner`.
    fn heap_owner_usage(&self, _owner: u8) -> (usize, usize) {
        (0, 0)
    }
    /// Forget what is attributed to `owner` (its app has been destroyed).
    fn clear_heap_owner(&mut self, _owner: u8) {}
    /// Running apps with their resource use: built-in apps (filled in by
    /// the desktop) and isolated user-mode apps (from the kernel).
    fn running_apps(&self) -> Vec<AppUsage> {
        Vec::new()
    }
    /// Stop a running app.
    fn close_app(&mut self, _app: AppHandle) {}
    /// Fill `buf` from the OS random generator (salts, keys). Returns false
    /// when no secure source exists; callers must then refuse the operation
    /// rather than fall back to predictable data.
    fn fill_random(&mut self, _buf: &mut [u8]) -> bool {
        false
    }
    /// Lock the screen now if a passcode is configured. The desktop handles
    /// this; the OS-level implementation does nothing.
    fn lock_now(&mut self) {}
    /// Run `job` on its own low-priority thread, pinned to `core` when
    /// given (core 1 keeps heavy work such as video decoding off the core
    /// that runs the GUI). The thread ends when `job` returns. Fails when
    /// the OS has no free thread slot or does not support workers.
    fn spawn_worker(&mut self, _name: &'static str, _core: Option<usize>, _stack_size: usize, _job: WorkerJob) -> Result<(), String> {
        Err("worker threads are not supported here".into())
    }
}
