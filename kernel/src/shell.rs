//! Minimal line-oriented debug shell on the kernel console.

use crate::sched::{self, State};
use crate::sync::Mutex;
use crate::{clock, console, heap, kprint, kprintln, time};

const MAX_LINE: usize = 64;

pub fn run(_: usize) {
    let mut line = [0u8; MAX_LINE];
    let mut len = 0;
    kprint!("p4os> ");
    loop {
        let byte = console::read_byte_blocking();
        match byte {
            b'\r' | b'\n' => {
                kprintln!();
                execute(core::str::from_utf8(&line[..len]).unwrap_or(""));
                len = 0;
                kprint!("p4os> ");
            }
            0x08 | 0x7F if len > 0 => {
                len -= 1;
                kprint!("\x08 \x08");
            }
            0x20..=0x7E if len < MAX_LINE => {
                line[len] = byte;
                len += 1;
                console::write_bytes(&[byte]);
            }
            _ => {}
        }
    }
}

/// Run a command and return its output instead of printing it.
pub fn capture(cmd: &str) -> alloc::string::String {
    console::capture_start();
    execute(cmd);
    console::capture_end()
}

fn execute(cmd: &str) {
    match cmd.trim() {
        "" => {}
        "help" => kprintln!("commands: help uptime clock irq ps mem demo membench lcd hwscan"),
        "membench" => membench(),
        "lcd" => crate::display::diagnose(),
        "hwscan" => hwscan(),
        "sd" => sd_probe(crate::sdmmc::DEFAULT_BUS),
        "audio" => crate::audio::diagnose(),
        "c6" => crate::c6::probe(),
        c if c.starts_with("get ") => http_get(c[4..].trim()),
        c if c.starts_with("wifi connect ") => {
            let mut it = c["wifi connect ".len()..].splitn(2, ' ');
            match (it.next(), it.next()) {
                (Some(ssid), Some(pass)) => match crate::wifi::connect(ssid, pass) {
                    Ok(()) => kprintln!("wifi: up"),
                    Err(e) => kprintln!("wifi: {}", e),
                },
                _ => kprintln!("usage: wifi connect SSID PASSWORD"),
            }
        }
        c if c.starts_with("tone") => {
            use p4os_sdk::Audio;
            let freq = c[4..].trim().parse().unwrap_or(1000);
            crate::audio::Speaker.play(alloc::boxed::Box::new(crate::audio::Tone::new(freq, 3000)));
        }
        c if c == "ls" || c.starts_with("ls ") => ls(c[2..].trim()),

        "sd slow1" => sd_probe(crate::sdmmc::BusConfig { four_bit: false, clock: (10, 20) }),
        "sd slow4" => sd_probe(crate::sdmmc::BusConfig { four_bit: true, clock: (10, 20) }),
        "sd fast1" => sd_probe(crate::sdmmc::BusConfig { four_bit: false, clock: (8, 0) }),
        c if c.starts_with("fill ") => fill(&c[5..]),
        c if c.starts_with("tap ") => {
            let mut it = c[4..].split_whitespace().map(|v| v.parse::<u16>());
            match (it.next(), it.next()) {
                (Some(Ok(x)), Some(Ok(y))) => crate::gui::inject_tap(x, y),
                _ => kprintln!("usage: tap X Y"),
            }
        }
        c if c.starts_with("bl ") => match c[3..].trim().parse::<u32>() {
            Ok(pct) => crate::backlight::set(pct as f32 / 100.0),
            Err(_) => kprintln!("usage: bl 0..100"),
        },
        "vpg on" => crate::display::test_pattern(true),
        "vpg off" => crate::display::test_pattern(false),
        "ps" => dump_threads(),
        "mem" => {
            let (used, total) = heap::usage();
            let (pused, ptotal) = heap::psram_usage();
            kprintln!("sram: {} / {} bytes used, largest free block {}", used, total, heap::largest_free());
            kprintln!("psram: {} / {} bytes used", pused, ptotal);
            let (speak, ppeak) = heap::take_peaks();
            kprintln!("peak since last mem: sram {} psram {}", speak, ppeak);
        }
        "demo" => demo(),
        "uptime" => kprintln!("{} ms", time::ticks()),
        "clock" => kprintln!("{}, cpu ~{} MHz", clock::dividers(), crate::measure_cpu_mhz()),
        "irq" => kprintln!("tick {} Hz, worst alarm latency {:.2} us", time::TICK_HZ, time::max_latency_us()),
        other => kprintln!("unknown command: {}", other),
    }
}

pub fn dump_threads() {
    kprintln!(" id name         prio state          cpu ms  stack used");
    for t in sched::snapshot().into_iter().flatten() {
        let state = match t.state {
            State::Ready if t.current => "running",
            State::Ready => "ready",
            State::Sleeping { .. } => "sleeping",
            State::Blocked => "blocked",
            State::Exited => "exited",
        };
        kprintln!(
            "{:>3} {:<12} {:>4} {:<14} {:>6}  {:>5}/{}",
            t.id, t.name, t.priority, state, t.run_ticks, t.stack_used, t.stack_size
        );
    }
}

static COUNTER: Mutex<u32> = Mutex::new(0);
const DEMO_ITERATIONS: u32 = 200_000;

/// Two equal-priority threads hammer a mutex while a third does float math,
/// exercising preemption, blocking and FP context switching together.
fn demo() {
    *COUNTER.lock() = 0;
    let start = time::ticks();
    sched::spawn("worker-a", 1, 2048, worker, 0);
    sched::spawn("worker-b", 1, 2048, worker, 1);
    sched::spawn("float", 1, 2048, float_worker, 0);
    while *COUNTER.lock() < 2 * DEMO_ITERATIONS {
        sched::sleep_ms(20);
    }
    kprintln!("demo: counter = {} after {} ms", *COUNTER.lock(), time::ticks() - start);
}

fn worker(_: usize) {
    for _ in 0..DEMO_ITERATIONS {
        *COUNTER.lock() += 1;
    }
}

fn float_worker(_: usize) {
    let mut x = 1.0f32;
    for i in 0..2_000_000u32 {
        x = x * 1.000_000_1 + (i & 7) as f32 * 1e-7;
    }
    let expected_close = (x - 1.6).abs() < 1.0;
    kprintln!("float: x = {} ({})", x, if expected_close { "plausible" } else { "SUSPICIOUS" });
}

/// Sequential write/read/copy throughput of a 1 MiB block in SRAM-backed
/// heap vs PSRAM, through the caches (what a framebuffer sees).
fn membench() {
    use alloc::vec;
    const LEN: usize = 1 << 20;
    let mut psram = vec![0u32; LEN / 4];
    let mut psram2 = vec![0u32; LEN / 4];
    let addr = psram.as_ptr() as usize;
    let mb_per_s = |ticks: u64| (LEN as u64 * crate::systimer::TICKS_PER_US) / ticks.max(1);
    let t = crate::systimer::now_ticks();
    for (i, w) in psram.iter_mut().enumerate() {
        unsafe { core::ptr::write_volatile(w, i as u32) };
    }
    crate::psram::cache_writeback(addr, LEN);
    let write = mb_per_s(crate::systimer::now_ticks() - t);
    crate::psram::cache_invalidate(addr, LEN);
    let t = crate::systimer::now_ticks();
    let mut sum = 0u32;
    for w in psram.iter() {
        sum = sum.wrapping_add(unsafe { core::ptr::read_volatile(w) });
    }
    let read = mb_per_s(crate::systimer::now_ticks() - t);
    let t = crate::systimer::now_ticks();
    psram2.copy_from_slice(&psram);
    crate::psram::cache_writeback(psram2.as_ptr() as usize, LEN);
    let copy = mb_per_s(crate::systimer::now_ticks() - t);
    kprintln!("1 MiB at {:#x}: write {} MB/s, read {} MB/s, copy {} MB/s (sum {:#x})", addr, write, read, copy, sum);
}

/// Probe every device the OS is expected to manage and report what answers.
fn hwscan() {
    use crate::{gpio, i2c, touch};
    kprintln!("I2C1 (SDA=GPIO7, SCL=GPIO8):");
    let mut found = 0;
    for addr in 0x08u8..0x78 {
        if i2c::probe(addr) {
            found += 1;
            let name = match addr {
                0x18 => "ES8311 audio codec",
                0x5D | 0x14 => "GT911 touch controller",
                0x36 => "camera sensor (OV02C10/OV5647 class)",
                0x30 | 0x32 => "camera sensor (SC2336 class)",
                0x3C | 0x3D => "camera sensor or OLED",
                0x68 | 0x69 => "IMU (MPU/ICM/BMI class)",
                0x6A | 0x6B => "IMU (LSM6/QMI8658 class)",
                0x19 | 0x1E => "accelerometer/magnetometer",
                0x51 => "RTC (PCF8563 class)",
                0x40 | 0x41 | 0x44 | 0x45 => "power/temperature/humidity sensor",
                0x76 | 0x77 => "pressure sensor (BMP/BME class)",
                _ => "unknown device",
            };
            kprintln!("  {:#04x}  {}", addr, name);
        }
    }
    kprintln!("  {} device(s) answered", found);
    if let Some(id) = touch::product_id() {
        kprintln!("GT911 product id: {:?}", core::str::from_utf8(&id).unwrap_or("?"));
    }
    let mut chip = [0u8; 2];
    if i2c::write_read(0x18, &[0xFD], &mut chip[..1]).is_ok() && i2c::write_read(0x18, &[0xFE], &mut chip[1..]).is_ok() {
        kprintln!("ES8311 chip id: {:02x}{:02x}", chip[0], chip[1]);
    }
    gpio::input(35, true);
    kprintln!("BOOT button (GPIO35): {}", if gpio::read(35) { "released" } else { "pressed" });
}

/// `fill RRGGBB` paints the whole screen one colour; `fill off` returns to
/// the desktop.
fn fill(arg: &str) {
    use core::sync::atomic::Ordering;
    let arg = arg.trim();
    if arg == "off" {
        crate::display::OVERRIDE.store(0, Ordering::Relaxed);
        return;
    }
    match u32::from_str_radix(arg, 16) {
        Ok(rgb) if arg.len() == 6 => {
            let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
            let c565 = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3);
            crate::display::OVERRIDE.store(0x1_0000 | c565, Ordering::Relaxed);
        }
        _ => kprintln!("usage: fill RRGGBB | fill off"),
    }
}

/// Initialise the SD card and dump the partition table from sector 0.
fn sd_probe(bus: crate::sdmmc::BusConfig) {
    match crate::sdmmc::init_with(bus) {
        Ok(blocks) => {
            let mut mbr = alloc::vec![0u8; 512];
            match crate::sdmmc::read_blocks(0, &mut mbr) {
                Ok(()) => {
                    kprintln!("sector 0 signature {:02x}{:02x}, first bytes {:02x?}", mbr[510], mbr[511], &mbr[..16]);
                    for p in 0..4 {
                        let e = &mbr[446 + p * 16..462 + p * 16];
                        let lba = u32::from_le_bytes(e[8..12].try_into().unwrap());
                        let len = u32::from_le_bytes(e[12..16].try_into().unwrap());
                        if e[4] != 0 {
                            kprintln!("  partition {}: type {:#04x} start {} size {} MiB", p, e[4], lba, len / 2048);
                        }
                    }
                }
                Err(e) => kprintln!("sd read failed: {:?}", e),
            }
            kprintln!("capacity {} blocks", blocks);
        }
        Err(e) => kprintln!("sd init failed: {:?}", e),
    }
}

fn ls(path: &str) {
    use p4os_sdk::FileSystem;
    let path = if path.is_empty() { "/sd" } else { path };
    match crate::fs::SdFs.list(path) {
        Ok(entries) => {
            for e in entries {
                if e.is_dir {
                    kprintln!("  {}/", e.name);
                } else {
                    kprintln!("  {:<40} {:>10}", e.name, e.size);
                }
            }
        }
        Err(e) => kprintln!("ls {}: {:?}", path, e),
    }
}

/// Fetch a URL through the network service and print a summary.
fn http_get(url: &str) {
    let id = crate::net::fetch(url);
    let start = crate::time::ticks();
    loop {
        match crate::net::poll_fetch(id) {
            // Requests queue until DHCP is up; do not hang the shell forever.
            p4os_sdk::FetchState::Pending if crate::time::ticks().wrapping_sub(start) > 60_000 => {
                kprintln!("fetch still pending after 60 s (is Wi-Fi up?)");
                return;
            }
            p4os_sdk::FetchState::Pending => crate::sched::sleep_ms(50),
            p4os_sdk::FetchState::Done { status, content_type, body } => {
                let preview = alloc::string::String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned();
                kprintln!("HTTP {} {} ({} bytes, {} ms)\n{}", status, content_type, body.len(), crate::time::ticks() - start, preview);
                return;
            }
            p4os_sdk::FetchState::Failed(e) => {
                kprintln!("fetch failed: {}", e);
                return;
            }
        }
    }
}
