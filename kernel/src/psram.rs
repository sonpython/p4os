//! In-package 32 MiB AP Memory HEX (x16 DDR) PSRAM, mapped at 0x4800_0000.
//!
//! Bring-up runs at MPLL 400 MHz / 20 = 20 MHz, where no timing tuning is
//! needed, to program the die's latency registers. The bus is then raised
//! to 200 MHz (or 100 MHz as a fallback) and the DQS phase and delay lines
//! are swept with real loads/stores through the cache, keeping the centre of
//! the widest passing window. The mode-register transactions use the
//! mask-ROM SPI helpers (part of the silicon, not ESP-IDF); everything else
//! is direct register programming following ESP-IDF v5.5 for silicon v1.x.

use crate::{kprintln, mmio, regi2c, soc, systimer};

pub const BASE: usize = 0x4800_0000;

const SPIMEM2: usize = 0x5008_E000; // PSRAM MSPI0: cache/AXI side
const SPIMEM3: usize = 0x5008_F000; // PSRAM MSPI1: register commands
const IOMUX_MSPI: usize = soc::HPPERIPH1 + 0x21200;
const CLKRST: usize = soc::HP_SYS_CLKRST;

const PMU_LDO_P1_0P1A: usize = 0x5011_51D0;
const PMU_LDO_P1_0P1A_ANA: usize = 0x5011_51D4;
const PMU_RF_PWC: usize = 0x5011_515C;
const LP_HP_CLK_CTRL: usize = soc::LP_CLKRST + 0x40;
const ANA_PLL_CTRL0: usize = CLKRST + 0xBC;

const ROM_SPI_CMD_CONFIG: usize = 0x4FC0_0108;
const ROM_SPI_CMD_START: usize = 0x4FC0_010C;
const ROM_SPI_SET_OP_MODE: usize = 0x4FC0_0110;
const ROM_CACHE_INVALIDATE_ADDR: usize = 0x4FC0_03E4;
const ROM_CACHE_WRITEBACK_ADDR: usize = 0x4FC0_03F4;
const CACHE_MAP_L1D_L2: u32 = (1 << 4) | (1 << 5);
const ROM_OPI_DTR_MODE: u32 = 7;
const PSRAM_SPI_NUM: i32 = 3;
const PSRAM_CS_MASK: u8 = 1 << 1;

const MMU_ITEM_INDEX: usize = SPIMEM2 + 0x380;
const MMU_ITEM_CONTENT: usize = SPIMEM2 + 0x37C;
const MMU_VALID: u32 = 1 << 11;
const MMU_ACCESS_PSRAM: u32 = 1 << 10;
const PAGE: usize = 64 * 1024;

/// Latency settings per bus speed (ESP-IDF AP HEX values).
#[derive(Clone, Copy)]
struct Speed {
    mhz: u32,
    div: u32,
    read_latency: u32,
    write_latency: u32,
    /// AXI-side dummy cycles (bit length - 1 in DDR).
    rd_dummy: u32,
    wr_dummy: u32,
}

const SPEED_200: Speed = Speed { mhz: 200, div: 2, read_latency: 4, write_latency: 1, rd_dummy: 25, wr_dummy: 11 };
const SPEED_100: Speed = Speed { mhz: 100, div: 4, read_latency: 2, write_latency: 2, rd_dummy: 17, wr_dummy: 7 };
const SPEED_20: Speed = Speed { mhz: 20, div: 20, read_latency: 2, write_latency: 2, rd_dummy: 17, wr_dummy: 7 };

/// Fields are only read through `Debug` when the failure is logged.
#[derive(Debug)]
#[allow(dead_code)]
pub enum Error {
    WrongVendor(u32),
    UnknownDensity(u32),
    MemTest { addr: usize, wrote: u32, read: u32 },
}

fn field(addr: usize, hi: u32, lo: u32, value: u32) {
    let mask = (((1u64 << (hi - lo + 1)) - 1) as u32) << lo;
    mmio::modify(addr, mask, value << lo);
}

#[repr(C)]
struct RomSpiCmd {
    cmd: u16,
    cmd_bitlen: u16,
    addr: *const u32,
    addr_bitlen: u32,
    tx: *const u32,
    tx_bitlen: u32,
    rx: *mut u32,
    rx_bitlen: u32,
    dummy_bitlen: u32,
}

/// One OPI-DTR register transaction to the PSRAM die.
fn xfer(cmd: u16, addr: u32, dummy_bits: u32, tx: Option<u16>, rx_bits: u32) -> u32 {
    let a = addr;
    let t = [tx.unwrap_or(0) as u32];
    let mut r = [0u32];
    let c = RomSpiCmd {
        cmd,
        cmd_bitlen: 16,
        addr: &a,
        addr_bitlen: 32,
        tx: if tx.is_some() { t.as_ptr() } else { core::ptr::null() },
        tx_bitlen: if tx.is_some() { 16 } else { 0 },
        rx: if rx_bits > 0 { r.as_mut_ptr() } else { core::ptr::null_mut() },
        rx_bitlen: rx_bits,
        dummy_bitlen: dummy_bits,
    };
    unsafe {
        let set_mode: unsafe extern "C" fn(i32, u32) = core::mem::transmute(ROM_SPI_SET_OP_MODE);
        let config: unsafe extern "C" fn(i32, *const RomSpiCmd) = core::mem::transmute(ROM_SPI_CMD_CONFIG);
        let start: unsafe extern "C" fn(i32, *mut u8, u16, u8, bool) = core::mem::transmute(ROM_SPI_CMD_START);
        set_mode(PSRAM_SPI_NUM, ROM_OPI_DTR_MODE);
        config(PSRAM_SPI_NUM, &c);
        start(PSRAM_SPI_NUM, r.as_mut_ptr() as *mut u8, (rx_bits / 8) as u16, PSRAM_CS_MASK, false);
    }
    r[0]
}

/// LDO channel 2 (VO2) powers both the PSRAM die and the MPLL.
fn power_on() {
    let ctrl = mmio::read(PMU_LDO_P1_0P1A);
    kprintln!("psram: LDO VO2 before init ctrl={:#010x} ana={:#010x}", ctrl, mmio::read(PMU_LDO_P1_0P1A_ANA));
    // dref=7, mul=5 gives 0.85 V x 2.25 ~= 1.91 V (untrimmed).
    field(PMU_LDO_P1_0P1A_ANA, 31, 28, 7);
    field(PMU_LDO_P1_0P1A_ANA, 25, 23, 5);
    field(PMU_LDO_P1_0P1A_ANA, 26, 26, 1);
    field(PMU_LDO_P1_0P1A, 14, 14, 0);
    field(PMU_LDO_P1_0P1A, 7, 7, 1);
    field(PMU_LDO_P1_0P1A, 11, 9, 0);
    field(PMU_LDO_P1_0P1A, 8, 8, 1);
    systimer::delay_us(100);
}

/// MPLL = 40 MHz x (div + 1) / (ref_div + 1).
fn mpll_set(mhz: u32) {
    mmio::set_bits(PMU_RF_PWC, 1 << 24);
    mmio::set_bits(LP_HP_CLK_CTRL, 1 << 28);
    mmio::clear_bits(ANA_PLL_CTRL0, 1 << 9);
    let r3 = regi2c::read(regi2c::MPLL, 3);
    regi2c::write(regi2c::MPLL, 3, r3 | (3 << 4));
    let r1 = regi2c::read(regi2c::MPLL, 1);
    regi2c::write(regi2c::MPLL, 1, r1 & 0xDF);
    regi2c::write(regi2c::MPLL, 1, r1 | 0x20);
    let div = (mhz / 20 - 1) as u8;
    regi2c::write(regi2c::MPLL, 2, (div << 3) | 1);
    while mmio::read(ANA_PLL_CTRL0) & (1 << 8) == 0 {}
    mmio::set_bits(ANA_PLL_CTRL0, 1 << 9);
}

fn set_bus_div(div: u32) {
    let clk = ((div - 1) << 16) | ((div / 2 - 1) << 8) | (div - 1);
    mmio::write(SPIMEM2 + 0x50, clk);
    mmio::write(SPIMEM3 + 0x14, clk);
}

fn clocks_and_reset(bus_div: u32) {
    mmio::set_bits(CLKRST + 0x14, 1 << 31);
    mmio::set_bits(CLKRST + 0x30, 1 << 14);
    for bit in [23, 25] {
        mmio::set_bits(CLKRST + 0xC0, 1 << bit);
        mmio::clear_bits(CLKRST + 0xC0, 1 << bit);
    }
    field(CLKRST + 0x30, 13, 12, 1); // source: MPLL
    field(CLKRST + 0x30, 15, 15, 1);
    field(CLKRST + 0x30, 23, 16, 0);

    // Pads: drive strength 2, DLC delays cleared, DQS receivers on with
    // phase and delay lines reset (an earlier 200 MHz app may have tuned them).
    let data_pads = [0x1C, 0x20, 0x24, 0x28, 0x2C, 0x30, 0x34, 0x38, 0x40, 0x44]
        .into_iter()
        .chain((0x48..=0x64).step_by(4));
    for off in data_pads {
        field(IOMUX_MSPI + off, 13, 12, 2);
        field(IOMUX_MSPI + off, 7, 4, 0);
    }
    for off in [0x3C, 0x68] {
        let a = IOMUX_MSPI + off;
        field(a, 16, 15, 2);
        field(a, 2, 1, 0);
        field(a, 10, 7, 0);
        field(a, 20, 17, 0);
        field(a, 0, 0, 1);
    }

    // Chip-select timing.
    let ac = SPIMEM2 + 0x1A0;
    field(ac, 0, 0, 1);
    field(ac, 6, 2, 3);
    field(ac, 1, 1, 1);
    field(ac, 11, 7, 3);
    field(ac, 30, 25, 2);

    set_bus_div(bus_div);
    mmio::set_bits(SPIMEM2 + 0x190, 1 << 5);
    mmio::set_bits(SPIMEM2 + 0x180, 1 << 5);
}

/// Program latency/burst mode registers for `speed` (while the bus still
/// runs at 20 MHz) and return the size in bytes.
fn configure_die(speed: Speed) -> Result<usize, Error> {
    let m01 = xfer(0x4040, 0, 8, None, 16);
    let m4 = xfer(0x4040, 4, 8, None, 16);
    // MR0: fixed latency (bit 5), read latency [4:2]; MR4: write latency [7:5].
    let mr0 = (m01 & 0xC0) | 0x20 | (speed.read_latency << 2);
    let mr4 = (m4 & 0x1F) | (speed.write_latency << 5);
    xfer(0xC0C0, 0, 0, Some(((m01 & 0xFF00) | mr0) as u16), 0);
    xfer(0xC0C0, 4, 0, Some(((m4 & 0xFF00) | mr4) as u16), 0);
    let m8 = xfer(0x4040, 8, 8, None, 8);
    xfer(0xC0C0, 8, 0, Some(((m8 & 0xB0) | 0x4B) as u16), 0);

    let vendor = (xfer(0x4040, 0, 8, None, 16) >> 8) & 0x1F;
    let density = xfer(0x4040, 2, 8, None, 16) & 0x7;
    kprintln!("psram: MR0/1={:#06x} MR4={:#06x} MR8={:#04x} vendor={:#x} density={}", m01, m4, m8, vendor, density);
    if vendor != 0x0D {
        return Err(Error::WrongVendor(vendor));
    }
    match density {
        1 => Ok(4 << 20),
        3 => Ok(8 << 20),
        5 => Ok(16 << 20),
        7 => Ok(32 << 20),
        6 => Ok(64 << 20),
        d => Err(Error::UnknownDensity(d)),
    }
}

/// Memory-mapped access path: commands, dummy cycles and DDR for the cache.
fn configure_axi(speed: Speed) {
    mmio::write(SPIMEM2 + 0x4C, 0xF000_8080);
    mmio::write(SPIMEM2 + 0x48, 0xF000_0000);
    let sc = SPIMEM2 + 0x40;
    field(sc, 0, 0, 1);
    field(sc, 19, 14, 31);
    field(sc, 3, 3, 1);
    field(sc, 27, 22, speed.wr_dummy);
    field(sc, 4, 4, 1);
    field(sc, 11, 6, speed.rd_dummy);
    field(sc, 5, 5, 1);
    field(sc, 20, 20, 1);
    field(sc, 21, 21, 1);
    mmio::set_bits(SPIMEM2 + 0x44, (1 << 23) | (0xF << 18) | (3 << 26));
    let ddr = mmio::read(SPIMEM2 + 0xD8);
    mmio::write(SPIMEM2 + 0xD8, (ddr & !0xF) | 0b0011);
    let fctrl = mmio::read(SPIMEM2 + 0x3C);
    mmio::write(SPIMEM2 + 0x3C, (fctrl & !((1 << 31) | 1)) | 1);
    mmio::set_bits(SPIMEM2 + 0x0C, (1 << 25) | (1 << 26));
    mmio::set_bits(SPIMEM3 + 0xD4, 1 << 1);
}

fn map(size: usize) {
    for page in 0..(size / PAGE) as u32 {
        mmio::write(MMU_ITEM_INDEX, page);
        mmio::write(MMU_ITEM_CONTENT, MMU_VALID | MMU_ACCESS_PSRAM | page);
    }
    cache_invalidate(BASE, size);
}

/// Serialises the cache sync engine, which both cores share: the ROM
/// helpers program its address/size/control registers in several steps.
pub static CACHE_OP: crate::spin::SpinLock<()> = crate::spin::SpinLock::new(());
/// Work done per lock hold, so the other core and interrupts never wait
/// long behind a framebuffer-sized operation.
const CACHE_OP_CHUNK: usize = 8 * 1024;

fn rom_cache_op(rom_fn: usize, addr: usize, size: usize) {
    let f: unsafe extern "C" fn(u32, u32, u32) -> i32 = unsafe { core::mem::transmute(rom_fn) };
    let end = addr + size;
    let mut at = addr;
    while at < end {
        let len = (end - at).min(CACHE_OP_CHUNK);
        let _guard = CACHE_OP.lock();
        unsafe { f(CACHE_MAP_L1D_L2, at as u32, len as u32) };
        at += len;
    }
}

pub fn cache_invalidate(addr: usize, size: usize) {
    rom_cache_op(ROM_CACHE_INVALIDATE_ADDR, addr, size);
}

/// Push dirty cache lines to PSRAM, e.g. before a DMA engine reads them.
pub fn cache_writeback(addr: usize, size: usize) {
    rom_cache_op(ROM_CACHE_WRITEBACK_ADDR, addr, size);
}

/// Delay-line sweep order used by ESP-IDF: data lines delayed 15..1, then
/// nothing, then DQS delayed 1..15. Returns (dqs_delay, data_delay).
fn delayline(i: u32) -> (u32, u32) {
    if i < 15 { (0, 15 - i) } else if i == 15 { (0, 0) } else { (i - 15, 0) }
}
const DELAYLINES: u32 = 31;

fn set_timing(phase: u32, line: u32) {
    let (dqs, data) = delayline(line);
    for off in [0x3C, 0x68] {
        let a = IOMUX_MSPI + off;
        field(a, 2, 1, phase);
        field(a, 10, 7, dqs);
        field(a, 20, 17, dqs);
    }
    for off in [0x1C, 0x20, 0x24, 0x28, 0x2C, 0x30, 0x34, 0x38].into_iter().chain((0x48..=0x64).step_by(4)) {
        field(IOMUX_MSPI + off, 7, 4, data);
    }
}

/// Quick 4 KiB pattern check through the cache at the start of PSRAM.
fn timing_ok(seed: u32) -> bool {
    const LEN: usize = 4096;
    let pattern = |i: usize| (i as u32 ^ seed).wrapping_mul(0x9E37_79B9) ^ 0xA55A_5AA5;
    for i in (0..LEN).step_by(4) {
        mmio::write(BASE + i, pattern(i));
    }
    cache_writeback(BASE, LEN);
    cache_invalidate(BASE, LEN);
    (0..LEN).step_by(4).all(|i| mmio::read(BASE + i) == pattern(i))
}

/// Sweep phase x delay line at the current bus speed and apply the centre
/// of the longest passing run. Returns false if nothing passed.
fn tune() -> bool {
    let mut best: Option<(u32, u32, u32)> = None; // (phase, start, len)
    for phase in 0..4 {
        let mut run_start = 0;
        let mut run_len = 0;
        for line in 0..DELAYLINES {
            set_timing(phase, line);
            if timing_ok(phase * 64 + line) && timing_ok(!(phase * 64 + line)) {
                if run_len == 0 {
                    run_start = line;
                }
                run_len += 1;
                if best.is_none_or(|(_, _, l)| run_len > l) {
                    best = Some((phase, run_start, run_len));
                }
            } else {
                run_len = 0;
            }
        }
    }
    match best {
        Some((phase, start, len)) => {
            let line = start + len / 2;
            set_timing(phase, line);
            kprintln!("psram: tuned phase {} delay-line {} (window {} wide)", phase, line, len);
            true
        }
        None => false,
    }
}

/// Write an address-dependent pattern across `size` bytes (sampling every
/// 4 KiB plus the first and last 64 KiB), flush, invalidate and verify.
fn memtest(size: usize) -> Result<(), Error> {
    let pattern = |a: usize| (a as u32).wrapping_mul(0x9E37_79B9) ^ 0x5A5A_A5A5;
    let addrs = (0..PAGE).step_by(4)
        .chain((PAGE..size - PAGE).step_by(4096))
        .chain((size - PAGE..size).step_by(4))
        .map(|o| BASE + o);
    for a in addrs.clone() {
        mmio::write(a, pattern(a));
    }
    cache_writeback(BASE, size);
    cache_invalidate(BASE, size);
    for a in addrs {
        let v = mmio::read(a);
        if v != pattern(a) {
            return Err(Error::MemTest { addr: a, wrote: pattern(a), read: v });
        }
    }
    Ok(())
}

/// Bring PSRAM up and return its mapped size in bytes.
pub fn init() -> Result<usize, Error> {
    power_on();
    mpll_set(400);
    let mut size = 0;
    for speed in [SPEED_200, SPEED_100, SPEED_20] {
        clocks_and_reset(SPEED_20.div);
        set_timing(0, 15);
        size = configure_die(speed)?;
        configure_axi(speed);
        map(size);
        set_bus_div(speed.div);
        if speed.mhz == SPEED_20.mhz || tune() {
            kprintln!("psram: bus at {} MHz", speed.mhz);
            break;
        }
        kprintln!("psram: no stable timing at {} MHz, falling back", speed.mhz);
    }
    let t0 = systimer::now_ticks();
    memtest(size)?;
    let ms = (systimer::now_ticks() - t0) / (systimer::TICKS_PER_US * 1000);
    kprintln!("psram: {} MiB at {:#x}, memtest ok in {} ms", size >> 20, BASE, ms);
    Ok(size)
}
