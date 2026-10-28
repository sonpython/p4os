//! HP clock tree: CPU, MEM, SYS and APB clocks.
//!
//! Each clock divides the one before it: ROOT -> CPU -> MEM -> SYS -> APB.
//! MEM_CLK must stay <= 200 MHz and APB_CLK <= 100 MHz. The supported
//! operating points follow ESP-IDF v5.5 `rtc_clk.c` for the P4:
//!
//! | CPU     | root  | cpu | mem | sys | apb | MEM / SYS / APB    |
//! |---------|-------|-----|-----|-----|-----|--------------------|
//! | 360 MHz | CPLL  | /1  | /2  | /1  | /2  | 180 / 180 / 90 MHz |
//! | 180 MHz | CPLL  | /2  | /1  | /1  | /2  | 180 / 180 / 90 MHz |
//! |  90 MHz | CPLL  | /4  | /1  | /1  | /1  |  90 /  90 / 90 MHz |
//! |  40 MHz | XTAL  | /1  | /1  | /1  | /1  |  40 /  40 / 40 MHz |
//!
//! At 180 MHz only the core slows down: memory and bus bandwidth (PSRAM,
//! DMA) are the same as at 360 MHz. The bootloader leaves CPU_CLK at 90 MHz.
//! The systimer, I2C, I2S, LEDC (XTAL), SDMMC (PLL_F160M), the DSI pixel
//! clock (PLL_F240M) and PSRAM (MPLL) have their own sources and keep
//! their rates across these changes.

use crate::spin::SpinLock;
use crate::{mmio, soc};
use core::sync::atomic::{AtomicU32, Ordering};

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

const ROOT_SRC_XTAL: u32 = 0;
const ROOT_SRC_CPLL: u32 = 1;

/// `ets_update_cpu_frequency` (esp_rom/esp32p4/ld/esp32p4.rom.ld): the ROM's
/// cycles-per-microsecond for its busy-wait delays (flash routines).
const ROM_UPDATE_CPU_FREQUENCY: usize = 0x4FC0_0044;

/// CPU frequencies the clock tree supports, fastest first.
pub const LEVELS_MHZ: [u32; 4] = [360, 180, 90, 40];

#[derive(Clone, Copy)]
pub struct Dividers {
    pub root_src: u32,
    pub cpu: u32,
    pub mem: u32,
    pub sys: u32,
    pub apb: u32,
}

/// Divider set for one of `LEVELS_MHZ`.
fn operating_point(mhz: u32) -> Option<Dividers> {
    let (root_src, cpu, mem, apb) = match mhz {
        360 => (ROOT_SRC_CPLL, 1, 2, 2),
        180 => (ROOT_SRC_CPLL, 2, 1, 2),
        90 => (ROOT_SRC_CPLL, 4, 1, 1),
        40 => (ROOT_SRC_XTAL, 1, 1, 1),
        _ => return None,
    };
    Some(Dividers { root_src, cpu, mem, sys: 1, apb })
}

/// Current CPU frequency in MHz; 0 until `set_cpu_mhz` first runs.
static CPU_MHZ: AtomicU32 = AtomicU32::new(0);
/// Serialises frequency changes between cores and threads.
static SWITCH: SpinLock<()> = SpinLock::new(());

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

fn set_root(src: u32) {
    let v = mmio::read(LP_HP_CLK_CTRL) & !0x3;
    mmio::write(LP_HP_CLK_CTRL, v | src);
}

fn set_cpu_div(div: u32) {
    set_field(ROOT_CLK_CTRL0, CPU_DIV_NUM_SHIFT, div - 1);
    set_field(ROOT_CLK_CTRL0, CPU_DIV_NUMERATOR_SHIFT, 0);
    set_field(ROOT_CLK_CTRL0, CPU_DIV_DENOMINATOR_SHIFT, 0);
}

/// Raise dividers from APB up to CPU, so no intermediate state
/// over-clocks the memory or APB buses.
fn apply_upwards(d: &Dividers) {
    set_field(ROOT_CLK_CTRL2, APB_DIV_NUM_SHIFT, d.apb - 1);
    bus_update();
    set_field(ROOT_CLK_CTRL1, SYS_DIV_NUM_SHIFT, d.sys - 1);
    bus_update();
    set_field(ROOT_CLK_CTRL1, MEM_DIV_NUM_SHIFT, d.mem - 1);
    bus_update();
    set_cpu_div(d.cpu);
    bus_update();
}

/// Lower dividers from CPU down to APB (the reverse order of `apply_upwards`).
fn apply_downwards(d: &Dividers) {
    set_cpu_div(d.cpu);
    bus_update();
    set_field(ROOT_CLK_CTRL1, MEM_DIV_NUM_SHIFT, d.mem - 1);
    bus_update();
    set_field(ROOT_CLK_CTRL1, SYS_DIV_NUM_SHIFT, d.sys - 1);
    bus_update();
    set_field(ROOT_CLK_CTRL2, APB_DIV_NUM_SHIFT, d.apb - 1);
    bus_update();
}

fn rom_set_ticks_per_us(mhz: u32) {
    let f: extern "C" fn(u32) = unsafe { core::mem::transmute(ROM_UPDATE_CPU_FREQUENCY) };
    f(mhz);
}

/// Switch CPU_CLK to `mhz`, one of `LEVELS_MHZ` (others are ignored and
/// return false). Safe from any thread on either core while the other core
/// runs; not from interrupt handlers. Sequences follow ESP-IDF: the CPU
/// mux is not covered by the divider update bit, so the root source is
/// switched last when moving onto the CPLL and first when leaving it.
pub fn set_cpu_mhz(mhz: u32) -> bool {
    let Some(target) = operating_point(mhz) else { return false };
    let _guard = SWITCH.lock();
    let current = CPU_MHZ.load(Ordering::Relaxed);
    if current == mhz {
        return true;
    }
    let on_xtal = mmio::read(LP_HP_CLK_CTRL) & 0x3 == ROOT_SRC_XTAL;
    if target.root_src == ROOT_SRC_XTAL {
        // CPLL -> XTAL: every intermediate state runs slower than both ends.
        set_root(ROOT_SRC_XTAL);
        set_cpu_div(target.cpu);
        set_field(ROOT_CLK_CTRL1, MEM_DIV_NUM_SHIFT, target.mem - 1);
        set_field(ROOT_CLK_CTRL1, SYS_DIV_NUM_SHIFT, target.sys - 1);
        set_field(ROOT_CLK_CTRL2, APB_DIV_NUM_SHIFT, target.apb - 1);
        bus_update();
    } else if on_xtal || mhz > current {
        apply_upwards(&target);
        set_root(ROOT_SRC_CPLL);
    } else {
        apply_downwards(&target);
        set_root(ROOT_SRC_CPLL);
    }
    rom_set_ticks_per_us(mhz);
    CPU_MHZ.store(mhz, Ordering::Relaxed);
    true
}

/// Raise CPU_CLK to 360 MHz from the 360 MHz CPLL (boot).
pub fn set_cpu_360mhz() {
    set_cpu_mhz(360);
}

/// CPU frequency last programmed, in MHz.
pub fn cpu_mhz() -> u32 {
    CPU_MHZ.load(Ordering::Relaxed)
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
