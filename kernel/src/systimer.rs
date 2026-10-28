//! 64-bit system timer, unit 0. Ticks at 16 MHz (40 MHz XTAL / 2.5).

use crate::{mmio, soc};

const UNIT0_OP: usize = soc::SYSTIMER + 0x04;
const UNIT0_VALUE_HI: usize = soc::SYSTIMER + 0x40;
const UNIT0_VALUE_LO: usize = soc::SYSTIMER + 0x44;
const UPDATE: u32 = 1 << 30;
const VALUE_VALID: u32 = 1 << 29;

pub const TICKS_PER_US: u64 = 16;

pub fn now_ticks() -> u64 {
    mmio::write(UNIT0_OP, UPDATE);
    while mmio::read(UNIT0_OP) & VALUE_VALID == 0 {}
    let hi = mmio::read(UNIT0_VALUE_HI) as u64;
    let lo = mmio::read(UNIT0_VALUE_LO) as u64;
    (hi << 32) | lo
}


pub fn delay_us(us: u64) {
    let end = now_ticks() + us * TICKS_PER_US;
    while now_ticks() < end {}
}

const CONF: usize = soc::SYSTIMER;
const TARGET0_HI: usize = soc::SYSTIMER + 0x1C;
const TARGET0_LO: usize = soc::SYSTIMER + 0x20;
const TARGET0_CONF: usize = soc::SYSTIMER + 0x34;
const COMP0_LOAD: usize = soc::SYSTIMER + 0x50;
const INT_ENA: usize = soc::SYSTIMER + 0x64;
const INT_CLR: usize = soc::SYSTIMER + 0x6C;

const CONF_TARGET0_WORK_EN: u32 = 1 << 24;
const CONF_UNIT0_WORK_EN: u32 = 1 << 30;
const CONF_CLK_EN: u32 = 1 << 31;
const TARGET0_INT: u32 = 1 << 0;

/// Prepare comparator 0 as a one-shot alarm against unit 0.
pub fn alarm_init() {
    mmio::write(TARGET0_CONF, 0);
    mmio::set_bits(CONF, CONF_CLK_EN | CONF_UNIT0_WORK_EN | CONF_TARGET0_WORK_EN);
    mmio::write(INT_CLR, TARGET0_INT);
    mmio::set_bits(INT_ENA, TARGET0_INT);
}

/// Fire the alarm interrupt once the counter reaches `deadline` (in ticks).
pub fn alarm_set(deadline: u64) {
    mmio::write(TARGET0_HI, (deadline >> 32) as u32 & 0xF_FFFF);
    mmio::write(TARGET0_LO, deadline as u32);
    mmio::write(COMP0_LOAD, 1);
}

pub fn alarm_ack() {
    mmio::write(INT_CLR, TARGET0_INT);
}
