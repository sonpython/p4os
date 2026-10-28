//! The GUI thread: runs the desktop from `p4os-apps`, feeds it input, and
//! pushes frames to the panel.
//!
//! The desktop draws into a logical buffer in the current orientation
//! (portrait 600x1024 by default). `present` copies it into the panel's
//! native 1024x600 back buffer, rotating 90 degrees in portrait, and flips.
//!
//! The thread also owns the screen's power state: after the inactivity
//! timeouts from `power` it dims the backlight, then turns the panel off
//! (`display::sleep`) and only polls the touch panel, slowly, until a
//! touch wakes it. The waking touch is not passed on to the apps.

use crate::display::{self, Framebuffer};
use crate::power::{self, Screen};
use crate::{console, heap, kprintln, sched, systimer, time};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use p4os_apps::desktop::Desktop;
use p4os_sdk::gui::ui::{Input, Key};
use p4os_sdk::config;
use p4os_sdk::*;

const IDLE_POLL_MS: u32 = 16;
/// Input keeps the CPU at full speed this long, so the UI responds at once.
const INPUT_BOOST_MS: u32 = 1000;
/// Dimmed backlight as a share of the user's level.
const DIM_FACTOR: f32 = 0.25;

pub fn start() {
    sched::spawn("gui", 3, 48 * 1024, run, 0);
}

fn run(_: usize) {
    let Some((front, back)) = display::init() else {
        kprintln!("gui: display init failed");
        return;
    };
    let mut buffers = [back, front];
    let mut logical = vec![Color::BLACK; display::WIDTH * display::HEIGHT];
    let mut desktop = Desktop::new();
    let mut sys = KernelSystem::restored();
    let mut input = Input::default();
    let mut next_frame_at = 0u64;
    let mut screen = Screen::On;
    let mut last_input = now_ms();
    // Set by a touch that woke the screen: input is dropped until release.
    let mut swallow = false;
    let watch = crate::wdt::watch("gui", 15_000);

    loop {
        watch.checkin();
        let forced = display::OVERRIDE.load(core::sync::atomic::Ordering::Relaxed);
        if forced != 0 {
            // The camera colour checks need a lit panel.
            last_input = now_ms();
            set_screen(&mut screen, Screen::On, &mut sys, &mut desktop);
            buffers[0].pixels().fill(forced as u16);
            display::present(&buffers[0]);
            buffers.swap(0, 1);
            sched::sleep_ms(50);
            next_frame_at = 0;
            continue;
        }
        if LOCK_REQUEST.swap(false, core::sync::atomic::Ordering::Relaxed) {
            desktop.lock_now(&mut sys);
            next_frame_at = 0;
        }
        // Injected taps (`tap X Y`, UI tests) wake the screen and are still
        // delivered; a real touch or key on a dark screen only wakes it.
        let injected = poll_injected(&mut input);
        let had_input = injected | poll_input(&mut input) | (!injected && poll_touch(&mut input, sys.orientation));
        let now = now_ms();
        if let Some(req) = power::take_screen_request() {
            if req == Screen::On {
                last_input = now;
            }
            if set_screen(&mut screen, req, &mut sys, &mut desktop) {
                next_frame_at = 0;
            }
        }
        if had_input {
            last_input = now;
            power::boost(INPUT_BOOST_MS);
            if screen == Screen::Off && !injected {
                swallow = true;
            }
            if set_screen(&mut screen, Screen::On, &mut sys, &mut desktop) {
                next_frame_at = 0;
            }
        }
        if swallow {
            swallow = input.down;
            input.pressed = false;
            input.released = false;
            input.delta = Point::default();
            input.keys.clear();
        }
        let idle = now.saturating_sub(last_input);
        if power::off_after_ms().is_some_and(|t| idle >= t) {
            set_screen(&mut screen, Screen::Off, &mut sys, &mut desktop);
        } else if screen == Screen::On && power::dim_after_ms().is_some_and(|t| idle >= t) {
            set_screen(&mut screen, Screen::Dim, &mut sys, &mut desktop);
        }
        if screen == Screen::Off {
            sched::sleep_ms(power::touch_poll_off_ms());
            continue;
        }
        sys.save_brightness_if_settled(now);
        if !had_input && now < next_frame_at {
            sched::sleep_ms(IDLE_POLL_MS);
            continue;
        }
        let (w, h) = sys.orientation.size();
        let started = systimer::now_ticks();
        let next = {
            let mut canvas = Canvas::new(&mut logical, w, h);
            desktop.frame(&mut canvas, &input, &mut sys)
        };
        input.pressed = false;
        input.released = false;
        input.delta = Point::default();
        input.keys.clear();

        let render_done = systimer::now_ticks();
        blit(&logical, sys.orientation, &mut buffers[0]);
        display::present(&buffers[0]);
        buffers.swap(0, 1);
        sys.last_frame_us = ((render_done - started) / systimer::TICKS_PER_US) as u32;
        sys.last_present_us = ((systimer::now_ticks() - render_done) / systimer::TICKS_PER_US) as u32;
        next_frame_at = now + next.unwrap_or(1000) as u64;
    }
}

/// Move the screen to state `to`: panel sleep/wake and backlight level,
/// then tell the desktop (lock screen) when it turned off or woke from off.
/// Returns whether anything changed (the next frame must be redrawn).
fn set_screen(screen: &mut Screen, to: Screen, sys: &mut KernelSystem, desktop: &mut Desktop) -> bool {
    if *screen == to {
        return false;
    }
    let user_level = sys.user_brightness;
    match to {
        Screen::Off => {
            crate::backlight::set(0.0);
            display::sleep();
        }
        Screen::On | Screen::Dim => {
            // Wake the panel before lighting it, so stale pixels never show.
            display::wake();
            let level = if to == Screen::Dim { (user_level * DIM_FACTOR).max(config::MIN_BRIGHTNESS) } else { user_level };
            crate::backlight::set(level.min(user_level));
        }
    }
    kprintln!("gui: screen {:?}", to);
    let was_off = *screen == Screen::Off;
    *screen = to;
    power::set_screen(to);
    if to == Screen::Off {
        desktop.on_screen_off(sys);
    } else if was_off {
        desktop.on_wake(sys);
    }
    true
}

fn now_ms() -> u64 {
    systimer::now_ticks() / (systimer::TICKS_PER_US * 1000)
}

/// Copy the logical frame into the panel buffer. Landscape frames are
/// rotated 90 degrees clockwise in 32x32 tiles to stay cache friendly.
fn blit(src: &[Color], o: Orientation, fb: &mut Framebuffer) {
    let dst = fb.pixels();
    let (pw, ph) = (display::WIDTH, display::HEIGHT);
    match o {
        Orientation::Portrait => {
            for (d, s) in dst.iter_mut().zip(src.iter()) {
                *d = s.0;
            }
        }
        Orientation::Landscape => {
            // Logical (x, y) in landscape -> physical (WIDTH - 1 - y, x).
            let lw = ph;
            const T: usize = 32;
            for ty in (0..pw).step_by(T) {
                for tx in (0..lw).step_by(T) {
                    for ly in ty..(ty + T).min(pw) {
                        let row = &src[ly * lw..];
                        for lx in tx..(tx + T).min(lw) {
                            dst[lx * pw + (pw - 1 - ly)] = row[lx].0;
                        }
                    }
                }
            }
        }
    }
}

/// Set by the shell's `lock now`; the GUI thread locks on its next pass.
static LOCK_REQUEST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn request_lock() {
    LOCK_REQUEST.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// A tap requested from the shell (`tap X Y`, logical coordinates), used
/// for automated UI tests: pressed on one frame, released on the next.
static INJECTED_TAP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static INJECT_STAGE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub fn inject_tap(x: u16, y: u16) {
    use core::sync::atomic::Ordering;
    INJECTED_TAP.store(((x as u32) << 16) | y as u32, Ordering::Relaxed);
    INJECT_STAGE.store(1, Ordering::Release);
}

fn poll_injected(input: &mut Input) -> bool {
    use core::sync::atomic::Ordering;
    let v = INJECTED_TAP.load(Ordering::Relaxed);
    let pos = Point::new((v >> 16) as i32, (v & 0xFFFF) as i32);
    match INJECT_STAGE.load(Ordering::Acquire) {
        1 => {
            input.pressed = true;
            input.down = true;
            input.press_pos = pos;
            input.pos = pos;
            INJECT_STAGE.store(2, Ordering::Release);
            true
        }
        2 => {
            input.down = false;
            input.released = true;
            INJECT_STAGE.store(0, Ordering::Release);
            true
        }
        _ => false,
    }
}

/// Read the GT911 and convert panel coordinates into the logical screen.
fn poll_touch(input: &mut Input, o: Orientation) -> bool {
    let Ok(sample) = crate::touch::poll() else { return false };
    match sample {
        Some(p) => {
            let (px, py) = (p.x, p.y);
            let pos = match o {
                Orientation::Portrait => Point::new(px, py),
                // Inverse of the landscape blit: physical (WIDTH - 1 - ly, lx).
                Orientation::Landscape => Point::new(py, display::WIDTH as i32 - 1 - px),
            };
            if !input.down {
                input.pressed = true;
                // Touch positions are not logged: on the passcode keypad they
                // would spell out the passcode on the serial console.
                input.press_pos = pos;
            }
            input.delta = Point::new(pos.x - input.pos.x, pos.y - input.pos.y);
            input.pos = pos;
            input.down = true;
            true
        }
        None if input.down => {
            input.down = false;
            input.released = true;
            true
        }
        None => false,
    }
}

/// Keys typed on the USB console drive the GUI until the touch panel
/// works: arrows, Enter, Backspace, Escape (home) and printable text.
fn poll_input(input: &mut Input) -> bool {
    let mut any = false;
    let mut esc = false;
    while let Some(b) = console::read_byte() {
        any = true;
        if esc {
            // Second byte of an escape sequence: '[' then the arrow letter.
            match b {
                b'[' => continue,
                b'A' => input.keys.push(Key::Up),
                b'B' => input.keys.push(Key::Down),
                b'C' => input.keys.push(Key::Right),
                b'D' => input.keys.push(Key::Left),
                _ => input.keys.push(Key::Escape),
            }
            esc = false;
            continue;
        }
        match b {
            0x1B => esc = true,
            b'\r' | b'\n' => input.keys.push(Key::Enter),
            0x08 | 0x7F => input.keys.push(Key::Backspace),
            0x20..=0x7E => input.keys.push(Key::Char(b as char)),
            _ => {}
        }
    }
    if esc {
        input.keys.push(Key::Escape);
    }
    any
}

/// A brightness change is saved once the slider has rested this long, so
/// dragging it does not rewrite flash on every frame.
const BRIGHTNESS_SAVE_DELAY_MS: u64 = 1500;

/// OS services for apps, backed by the kernel.
#[derive(Default)]
struct KernelSystem {
    orientation: Orientation,
    fs: crate::fs::SdFs,
    audio: crate::audio::Speaker,
    net: crate::net::WifiNet,
    last_frame_us: u32,
    last_present_us: u32,
    /// Backlight level chosen by the user; dimming lowers the actual one.
    user_brightness: f32,
    /// Brightness set by an app but not saved yet, and when (ms).
    unsaved_brightness: Option<(f32, u64)>,
}

impl KernelSystem {
    /// Apply the saved orientation and brightness. Runs after the display
    /// (and so the backlight) is initialised.
    fn restored() -> Self {
        let mut sys = Self::default();
        if let Some(o) = crate::config::get(config::UI_ORIENTATION).and_then(|v| config::parse_orientation(&v)) {
            sys.orientation = o;
        }
        if let Some(b) = crate::config::get(config::UI_BRIGHTNESS).and_then(|v| config::parse_brightness(&v)) {
            crate::backlight::set(b);
        }
        sys.user_brightness = crate::backlight::level();
        kprintln!("gui: {:?}, brightness {:.0}%", sys.orientation, sys.user_brightness * 100.0);
        sys
    }

    fn save_brightness_if_settled(&mut self, now_ms: u64) {
        if let Some((level, t)) = self.unsaved_brightness
            && now_ms.saturating_sub(t) >= BRIGHTNESS_SAVE_DELAY_MS
        {
            self.unsaved_brightness = None;
            self.config_set(config::UI_BRIGHTNESS, &config::brightness_value(level));
        }
    }
}

impl System for KernelSystem {
    fn uptime_ms(&self) -> u64 {
        time::ticks() as u64
    }

    fn now(&self) -> Option<DateTime> {
        crate::rtc::now()
    }

    fn set_time(&mut self, t: DateTime) {
        crate::rtc::set(t);
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
        let (heap_used, heap_total) = heap::usage();
        let (psram_used, psram_total) = heap::psram_usage();
        let threads = sched::snapshot()
            .unwrap_or([None; sched::MAX_THREADS])
            .into_iter()
            .flatten()
            .map(|t| ThreadStat {
                name: t.name.into(),
                priority: t.priority,
                state: format!("{:?}", t.state),
                cpu_ms: t.run_ticks,
                stack_used: t.stack_used as u32,
                stack_size: t.stack_size as u32,
                core: t.cpu.or(t.last_cpu).map(|c| c as u8),
            })
            .collect();
        let core_loads: alloc::vec::Vec<f32> = (0..crate::irq::HART_COUNT).map(sched::cpu_load).collect();
        SystemStats {
            uptime_ms: time::ticks() as u64,
            cpu_mhz: crate::clock::cpu_mhz(),
            cpu_load: core_loads.iter().sum::<f32>() / core_loads.len() as f32,
            core_loads,
            heap_used,
            heap_total,
            psram_used,
            psram_total,
            threads,
            battery_mv: crate::battery::millivolts(),
            last_crash: crate::fault::last_crash(),
            chip: format!("ESP32-P4 v{}.{}, frame {} us + present {} us", crate::efuse::chip_revision().0, crate::efuse::chip_revision().1, self.last_frame_us, self.last_present_us),
        }
    }

    fn decode_image(&mut self, data: &[u8], max_w: i32, max_h: i32) -> Option<Image> {
        p4os_media::image::decode(data, max_w, max_h)
    }

    fn brightness(&self) -> f32 {
        self.user_brightness
    }

    fn set_brightness(&mut self, value: f32) {
        self.user_brightness = value;
        crate::backlight::set(value);
        self.unsaved_brightness = Some((value, now_ms()));
    }

    fn run_command(&mut self, line: &str) -> String {
        // Terminal commands may legitimately run for long (format, bench).
        crate::wdt::busy(|| crate::shell::capture(line))
    }

    fn launch(&mut self, _app_id: &str) {}

    fn orientation(&self) -> Orientation {
        self.orientation
    }

    fn set_orientation(&mut self, o: Orientation) {
        if o != self.orientation {
            self.orientation = o;
            self.config_set(config::UI_ORIENTATION, config::orientation_value(o));
        }
    }

    fn wifi_connect(&mut self, ssid: &str, password: &str) {
        if let Err(e) = config::check_wifi(ssid, password) {
            kprintln!("wifi: {}", e);
            return;
        }
        let saved = crate::config::update(|e| {
            e.insert(config::WIFI_SSID.into(), ssid.into());
            e.insert(config::WIFI_PASSWORD.into(), password.into());
        });
        if let Err(e) = saved {
            kprintln!("config: saving Wi-Fi network failed: {:?}", e);
        }
        crate::wifi_auto::request(ssid, password);
    }

    fn config_get(&self, key: &str) -> Option<String> {
        if config::is_secret(key) {
            return None;
        }
        crate::config::get(key)
    }

    fn config_set(&mut self, key: &str, value: &str) {
        if let Err(e) = crate::config::set(key, value) {
            kprintln!("config: saving {} failed: {:?}", key, e);
        } else if key.starts_with("power.") {
            power::load_config();
        }
    }

    fn config_remove(&mut self, key: &str) {
        if let Err(e) = crate::config::update(|e| {
            e.remove(key);
        }) {
            kprintln!("config: removing {} failed: {:?}", key, e);
        }
    }

    fn fill_random(&mut self, buf: &mut [u8]) -> bool {
        crate::rng::fill(buf);
        true
    }
}

