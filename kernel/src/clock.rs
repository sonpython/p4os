//! HP clock tree: CPU, MEM, SYS and APB clocks.
//!
//! The bootloader leaves CPU_CLK at CPLL/4 (90 MHz). Running at full speed
//! requires CPU = CPLL/1 = 360 MHz while keeping MEM_CLK <= 200 MHz and
//! APB_CLK <= 100 MHz, so the chain becomes 360 -> /2 -> 180 -> /1 -> 180 -> /2 -> 90.

use crate::{mmio, soc};

const ROOT_CLK_CTRL0: usize = soc::HP_SYS_CLKRST + 0x4;
const ROOT_CLK_CTRL1: usize = soc::HP_SYS_CLKRST + 0x8;
const ROOT_CLK_CTRL2: usize = soc::HP_SYS_CLKRST + 0xC;
const LP_HP_CLK_CTRL: usize = soc::LP_CLKRST + 0x40;

const SOC_CLK_DIV_UPDATE: u32 = 1 << 4;
const CPU_DIV_NUM_SHIFT: u32 = 5;
const CPU_DIV_NUMERATOR_SHIFT: u32 = 13;
const CPU_DIV_DENOMINATOR_SHIFT: u32 = 21;
const MEM_DIV_NUM_SHIFT: u32 = 0;
const SYS_DIV_NUM_SHIFT: u32 = 24;
const APB_DIV_NUM_SHIFT: u32 = 16;

const ROOT_SRC_CPLL: u32 = 1;

#[derive(Clone, Copy)]
pub struct Dividers {
    pub root_src: u32,
    pub cpu: u32,
    pub mem: u32,
    pub sys: u32,
    pub apb: u32,
}

fn field(addr: usize, shift: u32) -> u32 {
    (mmio::read(addr) >> shift) & 0xFF
}

fn set_field(addr: usize, shift: u32, value: u32) {
    let v = mmio::read(addr) & !(0xFF << shift);
    mmio::write(addr, v | (value << shift));
}

/// Latch new divider values into the clock tree and wait for completion.
fn bus_update() {
    mmio::set_bits(ROOT_CLK_CTRL0, SOC_CLK_DIV_UPDATE);
    while mmio::read(ROOT_CLK_CTRL0) & SOC_CLK_DIV_UPDATE != 0 {}
}

pub fn dividers() -> Dividers {
    Dividers {
        root_src: mmio::read(LP_HP_CLK_CTRL) & 0x3,
        cpu: field(ROOT_CLK_CTRL0, CPU_DIV_NUM_SHIFT) + 1,
        mem: field(ROOT_CLK_CTRL1, MEM_DIV_NUM_SHIFT) + 1,
        sys: field(ROOT_CLK_CTRL1, SYS_DIV_NUM_SHIFT) + 1,
        apb: field(ROOT_CLK_CTRL2, APB_DIV_NUM_SHIFT) + 1,
    }
}

/// Raise CPU_CLK to 360 MHz from the 360 MHz CPLL.
///
/// Dividers are raised from APB up to CPU so that no intermediate state
/// over-clocks the memory or APB buses.
pub fn set_cpu_360mhz() {
    set_field(ROOT_CLK_CTRL2, APB_DIV_NUM_SHIFT, 2 - 1);
    bus_update();
    set_field(ROOT_CLK_CTRL1, SYS_DIV_NUM_SHIFT, 1 - 1);
    bus_update();
    set_field(ROOT_CLK_CTRL1, MEM_DIV_NUM_SHIFT, 2 - 1);
    bus_update();
    set_field(ROOT_CLK_CTRL0, CPU_DIV_NUM_SHIFT, 1 - 1);
    set_field(ROOT_CLK_CTRL0, CPU_DIV_NUMERATOR_SHIFT, 0);
    set_field(ROOT_CLK_CTRL0, CPU_DIV_DENOMINATOR_SHIFT, 0);
    bus_update();
    let v = mmio::read(LP_HP_CLK_CTRL) & !0x3;
    mmio::write(LP_HP_CLK_CTRL, v | ROOT_SRC_CPLL);
}

impl core::fmt::Display for Dividers {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let src = match self.root_src {
            0 => "XTAL",
            1 => "CPLL",
            2 => "RC_FAST",
            _ => "?",
        };
        write!(f, "root={} cpu/{} mem/{} sys/{} apb/{}", src, self.cpu, self.mem, self.sys, self.apb)
    }
}
