//! Screenshots of the frame on screen, as the user sees it (landscape
//! frames are rotated back upright).
//!
//! `screenshot [fast|raw]` streams the frame over the serial console in
//! the framed protocol of `p4os_media::shot` (decoded by
//! `scripts/screenshot.py`); `fast` is a lossless RLE, `raw` the plain
//! RGB565 pixels. `screenshot sd [NAME]` saves a PNG to
//! /sd/Pictures/Screenshots. The BOOT button does the same, with a short
//! white flash as feedback.
//!
//! A frame showing a typed passcode (lock screen keypad) is never captured.

use crate::display::{self, FRAME_LANDSCAPE, FRAME_SECURE};
use crate::{gpio, kprintln, sched, systimer};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use p4os_media::shot::{self, Encoding, FORMAT_RGB565, Header};
use p4os_sdk::FileSystem;

const DIR: &str = "/sd/Pictures/Screenshots";
const MAX_NAME: usize = 48;
/// Give up streaming when the host stops reading for this long.
const STALL_MS: u32 = 1000;

pub struct Capture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u16>,
}

/// Copy the frame on screen. Flips wait meanwhile, so it is never torn.
pub fn capture() -> Result<Capture, &'static str> {
    let n = display::WIDTH * display::HEIGHT;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(n).map_err(|_| "not enough memory")?;
    pixels.resize(n, 0u16);
    let landscape = display::with_front(|front, info| {
        if info & FRAME_SECURE != 0 {
            return Err("refused while a passcode is being typed");
        }
        let landscape = info & FRAME_LANDSCAPE != 0;
        if landscape {
            unrotate(front, &mut pixels);
        } else {
            pixels.copy_from_slice(front);
        }
        Ok(landscape)
    })
    .ok_or("display is not running")??;
    let (width, height) = if landscape { (display::HEIGHT, display::WIDTH) } else { (display::WIDTH, display::HEIGHT) };
    Ok(Capture { width, height, pixels })
}

/// Inverse of the GUI's landscape blit: physical (px, py) holds logical
/// (py, WIDTH - 1 - px). Walks the panel in 32x32 tiles for the caches.
fn unrotate(phys: &[u16], out: &mut [u16]) {
    let (pw, ph) = (display::WIDTH, display::HEIGHT);
    let lw = ph;
    const T: usize = 32;
    for ty in (0..ph).step_by(T) {
        for tx in (0..pw).step_by(T) {
            for py in ty..(ty + T).min(ph) {
                let row = &phys[py * pw..(py + 1) * pw];
                for px in tx..(tx + T).min(pw) {
                    out[(pw - 1 - px) * lw + py] = row[px];
                }
            }
        }
    }
}

fn ms_since(t: u64) -> u64 {
    (systimer::now_ticks() - t) / (systimer::TICKS_PER_US * 1000)
}

/// `screenshot ...` from the shell. `serial` is false for the Terminal
/// app, whose output is captured into a string and cannot carry a frame.
pub fn command(args: &str, serial: bool) {
    match args {
        "" | "fast" => stream(Encoding::Rle, serial),
        "raw" => stream(Encoding::Raw, serial),
        "sd" => report_save(None),
        a if a.starts_with("sd ") => report_save(Some(a[3..].trim())),
        _ => kprintln!("usage: screenshot [fast|raw] | screenshot sd [NAME]"),
    }
}

fn stream(encoding: Encoding, serial: bool) {
    if !serial {
        kprintln!("screenshot: streaming needs the serial console; use: screenshot sd");
        return;
    }
    let t0 = systimer::now_ticks();
    let cap = match capture() {
        Ok(c) => c,
        Err(e) => {
            kprintln!("screenshot: {}", e);
            return;
        }
    };
    let capture_ms = ms_since(t0);
    let t1 = systimer::now_ticks();
    let payload = match encoding {
        Encoding::Raw => shot::raw_bytes(&cap.pixels),
        Encoding::Rle => {
            let mut out = Vec::new();
            shot::rle_encode(&cap.pixels, cap.width, &mut out);
            out
        }
    };
    let header = Header {
        width: cap.width as u16,
        height: cap.height as u16,
        format: FORMAT_RGB565,
        encoding,
        payload_len: payload.len() as u32,
        pixels_len: (cap.pixels.len() * 2) as u32,
    };
    let (head, tail) = shot::frame_parts(&header, &payload);
    let encode_ms = ms_since(t1);
    let t2 = systimer::now_ticks();
    let sent = crate::console::binary_transfer(STALL_MS, |w| {
        // A line break first, so the frame never starts mid-line in a log.
        w.write(b"\n")?;
        w.write(&head)?;
        w.write(&payload)?;
        w.write(&tail)?;
        w.write(b"\n")
    });
    let send_ms = ms_since(t2);
    match sent {
        Ok(()) => kprintln!(
            "screenshot: {}x{} {} {} bytes, capture {} ms, encode {} ms, send {} ms",
            cap.width,
            cap.height,
            if encoding == Encoding::Rle { "rle" } else { "raw" },
            payload.len(),
            capture_ms,
            encode_ms,
            send_ms
        ),
        Err(_) => kprintln!("screenshot: host stopped reading, transfer abandoned"),
    }
}

/// A file name from the user: letters, digits, `-`, `_` and `.`; `.png`
/// is added when missing.
fn file_name(name: Option<&str>) -> Result<String, &'static str> {
    let base = match name {
        Some(n) => {
            let ok = !n.is_empty()
                && n.len() <= MAX_NAME
                && !n.starts_with('.')
                && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
            if !ok {
                return Err("name must be 1-48 letters, digits, '-', '_' or '.'");
            }
            String::from(n)
        }
        None => match crate::rtc::now() {
            Some(t) => format!("Screenshot-{:04}{:02}{:02}-{:02}{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second),
            None => format!("Screenshot-{}", crate::time::ticks()),
        },
    };
    Ok(if base.to_ascii_lowercase().ends_with(".png") { base } else { base + ".png" })
}

/// Capture the screen and save it as a PNG on the SD card. Returns the
/// path and a timing summary.
pub fn save(name: Option<&str>) -> Result<(String, String), String> {
    let file = file_name(name).map_err(String::from)?;
    let t0 = systimer::now_ticks();
    let cap = capture().map_err(String::from)?;
    let capture_ms = ms_since(t0);
    let t1 = systimer::now_ticks();
    let png = shot::png_rgb565(&cap.pixels, cap.width, cap.height);
    drop(cap.pixels);
    let encode_ms = ms_since(t1);
    let t2 = systimer::now_ticks();
    let mut fs = crate::fs::SdFs;
    // Both levels may exist already; a real failure shows up in the write.
    for dir in ["/sd/Pictures", DIR] {
        let _ = fs.create_dir(dir);
    }
    let path = format!("{}/{}", DIR, file);
    fs.write(&path, &png).map_err(|e| format!("writing {} failed: {:?}", path, e))?;
    let summary = format!(
        "{}x{}, {} bytes, capture {} ms, encode {} ms, write {} ms",
        cap.width,
        cap.height,
        png.len(),
        capture_ms,
        encode_ms,
        ms_since(t2)
    );
    Ok((path, summary))
}

fn report_save(name: Option<&str>) {
    match save(name) {
        Ok((path, summary)) => kprintln!("screenshot: saved {} ({})", path, summary),
        Err(e) => kprintln!("screenshot: {}", e),
    }
}

/// BOOT button (active low, also a strapping pin; only read here).
const BUTTON_PIN: u32 = 35;
const POLL_MS: u32 = 30;
const FLASH_MS: u32 = 120;
/// Light grey: a visible flash that does not glare at full backlight.
const FLASH_RGB565: u32 = 0xC618;

/// Watch the BOOT button: each press saves a screenshot to the SD card.
pub fn start() {
    sched::spawn("shotkey", 1, 8 * 1024, button_thread, 0);
}

fn button_thread(_: usize) {
    gpio::input(BUTTON_PIN, true);
    let mut was_down = false;
    loop {
        sched::sleep_ms(POLL_MS);
        let down = !gpio::read(BUTTON_PIN);
        if down && !was_down {
            report_save(None);
            flash();
        }
        was_down = down;
    }
}

/// Brief solid fill as capture feedback, unless `fill` is in use.
fn flash() {
    let ov = &display::OVERRIDE;
    if ov.compare_exchange(0, 0x1_0000 | FLASH_RGB565, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        sched::sleep_ms(FLASH_MS);
        let _ = ov.compare_exchange(0x1_0000 | FLASH_RGB565, 0, Ordering::Relaxed, Ordering::Relaxed);
    }
}
