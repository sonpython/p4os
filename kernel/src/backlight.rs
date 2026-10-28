//! LCD backlight brightness: LEDC PWM on GPIO23, which drives the enable
//! pin of the panel's MP3202 boost converter (active high). 20 kHz with
//! 10-bit resolution, the vendor's settings (the MP3202 wants 20 kHz-1 MHz).

use crate::{mmio, soc};
use core::sync::atomic::{AtomicU32, Ordering};

const LEDC: usize = soc::HPPERIPH1 + 0x13000;
const PIN: u32 = 23;
const SIG_LEDC_CH0: u32 = 126;
const RESOLUTION_BITS: u32 = 10;
/// XTAL 40 MHz / (20 kHz * 1024) = 1.953 in 10.8 fixed point.
const CLK_DIV_X256: u32 = 500;

static LEVEL: AtomicU32 = AtomicU32::new(0);

pub fn init() {
    let clk = soc::HP_SYS_CLKRST;
    mmio::set_bits(clk + 0x20, 1 << 0); // LEDC APB clock
    mmio::modify(clk + 0x9C, 0b111, 1 << 2); // XTAL, enabled
    mmio::set_bits(clk + 0xC4, 1 << 29);
    mmio::clear_bits(clk + 0xC4, 1 << 29);
    mmio::write(LEDC + 0x170, 1 << 31); // LEDC_CONF: clock enable

    // Timer 0: duty resolution, divider, then latch with para_up.
    mmio::write(LEDC + 0xA0, RESOLUTION_BITS | (CLK_DIV_X256 << 5));
    mmio::set_bits(LEDC + 0xA0, 1 << 26);

    // Route channel 0 to the pad.
    let pad = soc::IO_MUX + 4 + 4 * PIN as usize;
    mmio::modify(pad, 0x7 << 12, 1 << 12);
    mmio::write(soc::GPIO + 0x558 + 4 * PIN as usize, SIG_LEDC_CH0);
    mmio::write(soc::GPIO + 0x24, 1 << PIN);
    set(1.0);
}

/// Brightness 0.0..=1.0.
pub fn set(level: f32) {
    let max = (1u32 << RESOLUTION_BITS) - 1;
    let duty = (level.clamp(0.0, 1.0) * max as f32) as u32;
    LEVEL.store(duty, Ordering::Relaxed);
    mmio::write(LEDC + 0x04, 0); // hpoint
    mmio::write(LEDC + 0x08, duty << 4); // duty has 4 fractional bits
    // Fixed duty: one step of size 0, started now.
    mmio::write(LEDC + 0x0C, (1 << 31) | (1 << 30) | (1 << 20) | (1 << 10));
    // Channel 0 on timer 0, output enabled, latch the new duty.
    mmio::write(LEDC + 0x00, (1 << 2) | (1 << 4));
}

pub fn level() -> f32 {
    LEVEL.load(Ordering::Relaxed) as f32 / ((1u32 << RESOLUTION_BITS) - 1) as f32
}
