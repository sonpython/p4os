//! Internal analog configuration bus ("regi2c") used to program PLLs.
//!
//! The P4 ROM has no regi2c helpers, but the protocol is plain register
//! access through the LP analog I2C master.

use crate::mmio;

const LP_PERI_CLK_EN: usize = 0x5012_0000;
const LP_I2C_ANA_MST_CLK_EN: u32 = 1 << 27;
const ANA_I2C0_CTRL: usize = 0x5012_4000;
const ANA_CONF1: usize = 0x5012_401C;
const ANA_CONF2: usize = 0x5012_4020;
const BUSY: u32 = 1 << 25;
const WRITE: u32 = 1 << 24;

/// Analog block id of the MSPI PLL (MPLL).
pub const MPLL: u32 = 0x63;
/// ANA_CONF2 bit selecting the MPLL block on the master.
const MPLL_SEL: u32 = 1 << 9;

fn wait_idle() {
    while mmio::read(ANA_I2C0_CTRL) & BUSY != 0 {}
}

fn select(block: u32) {
    mmio::set_bits(LP_PERI_CLK_EN, LP_I2C_ANA_MST_CLK_EN);
    mmio::clear_bits(ANA_CONF2, 0x00FF_FFFF);
    mmio::clear_bits(ANA_CONF1, 0x00FF_FFFF);
    match block {
        MPLL => mmio::set_bits(ANA_CONF2, MPLL_SEL),
        _ => panic!("regi2c block {block:#x} not wired up"),
    }
}

pub fn read(block: u32, reg: u32) -> u8 {
    select(block);
    wait_idle();
    mmio::write(ANA_I2C0_CTRL, block | (reg << 8));
    wait_idle();
    ((mmio::read(ANA_I2C0_CTRL) >> 16) & 0xFF) as u8
}

pub fn write(block: u32, reg: u32, data: u8) {
    select(block);
    wait_idle();
    mmio::write(ANA_I2C0_CTRL, block | (reg << 8) | WRITE | ((data as u32) << 16));
    wait_idle();
}
