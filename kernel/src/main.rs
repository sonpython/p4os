#![no_std]
#![no_main]

extern crate alloc;

mod app_desc;
mod audio;
mod backlight;
mod boot;
mod c6;
mod clock;
mod display;
mod console;
mod efuse;
mod fs;
mod gpio;
mod gui;
mod heap;
mod i2c;
mod irq;
mod mmio;
mod net;
mod proto;
mod psram;
mod regi2c;
mod rng;
mod rtc;
mod sched;
mod sdmmc;
mod shell;
mod soc;
mod sync;
mod systimer;
mod time;
mod touch;
mod trap;
mod wdt;
mod wifi;

use core::arch::asm;
use core::panic::PanicInfo;

fn read_mcycle() -> u32 {
    let v: u32;
    unsafe { asm!("csrr {0}, mcycle", out(reg) v) };
    v
}

/// Estimate the CPU clock by counting cycles across a systimer interval.
pub fn measure_cpu_mhz() -> u32 {
    let t0 = systimer::now_ticks();
    let c0 = read_mcycle();
    systimer::delay_us(10_000);
    let c1 = read_mcycle();
    let t1 = systimer::now_ticks();
    let us = (t1 - t0) / systimer::TICKS_PER_US;
    (c1.wrapping_sub(c0) as u64 / us) as u32
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    wdt::disable_all();

    let (major, minor) = efuse::chip_revision();
    let mac = efuse::mac();
    kprintln!();
    kprintln!("p4os {} booting", env!("CARGO_PKG_VERSION"));
    kprintln!("chip ESP32-P4 v{}.{}", major, minor);
    kprintln!(
        "mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    let hart: u32;
    unsafe { asm!("csrr {0}, mhartid", out(reg) hart) };
    kprintln!("running on hart {}", hart);
    let before = clock::dividers();
    let mhz_before = measure_cpu_mhz();
    clock::set_cpu_360mhz();
    kprintln!("clock at boot: {}, cpu ~{} MHz", before, mhz_before);
    kprintln!("clock now:     {}, cpu ~{} MHz", clock::dividers(), measure_cpu_mhz());

    heap::init();
    match psram::init() {
        Ok(size) => unsafe { heap::add_region(psram::BASE, size) },
        Err(e) => kprintln!("psram: init failed: {:?}", e),
    }
    irq::init();
    time::init();
    console::init_rx();
    i2c::init();
    if let Err(e) = fs::mount() {
        kprintln!("fs: no SD card filesystem ({})", e);
    }
    time::resync();
    irq::enable_global();

    time::sleep_ms(1000);
    kprintln!("tick check: {} ticks after 1 s sleep", time::ticks());
    sched::spawn("shell", 2, 32 * 1024, shell::run, 0);
    audio::init();
    gui::start();
    sched::start()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("\n*** kernel panic: {}", info);
    halt()
}

pub fn halt() -> ! {
    loop {
        unsafe { asm!("wfi") };
    }
}
