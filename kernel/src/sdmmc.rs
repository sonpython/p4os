//! SD card on SDMMC slot 0 (Synopsys DWC mobile storage host), 4-bit bus
//! on the dedicated IO MUX pins CLK 43, CMD 44, D0-D3 39-42.
//!
//! Data moves with the controller's internal DMA (IDMAC) through a bounce
//! buffer in internal SRAM that the CPU accesses via the non-cacheable
//! alias, so no cache maintenance is needed. (CPU reads of the FIFO window
//! do not pop data on this chip, which is why ESP-IDF always uses IDMAC.)
//! The card supply (TF_VCC) and the GPIO39-48 IO bank come from on-chip
//! LDO channel 4 at 3.3 V.

use crate::sync::Mutex;
use crate::{kprintln, mmio, soc, systimer};

const BASE: usize = 0x5008_3000;
const CTRL: usize = BASE + 0x00;
const CLKDIV: usize = BASE + 0x08;
const CLKSRC: usize = BASE + 0x0C;
const CLKENA: usize = BASE + 0x10;
const TMOUT: usize = BASE + 0x14;
const CTYPE: usize = BASE + 0x18;
const BLKSIZ: usize = BASE + 0x1C;
const BYTCNT: usize = BASE + 0x20;
const INTMASK: usize = BASE + 0x24;
const CMDARG: usize = BASE + 0x28;
const CMD: usize = BASE + 0x2C;
const RESP0: usize = BASE + 0x30;
const RINTSTS: usize = BASE + 0x44;
const STATUS: usize = BASE + 0x48;
const BMOD: usize = BASE + 0x80;
const PLDMND: usize = BASE + 0x84;
const DBADDR: usize = BASE + 0x88;
const IDSTS: usize = BASE + 0x8C;

const CTRL_DMA_ENABLE: u32 = 1 << 5;
const CTRL_USE_IDMAC: u32 = 1 << 25;
const BMOD_SWR: u32 = 1 << 0;
const BMOD_FB: u32 = 1 << 1;
const BMOD_DE: u32 = 1 << 7;
const DES0_LD: u32 = 1 << 2;
const DES0_FS: u32 = 1 << 3;
const DES0_CH: u32 = 1 << 4;
const DES0_OWN: u32 = 1 << 31;
const IDSTS_ERRORS: u32 = (1 << 2) | (1 << 5) | (1 << 8) | (1 << 9);

/// Non-cacheable alias offset for internal SRAM.
const UNCACHED: usize = 0x4000_0000;
/// Blocks moved per DMA transfer (bounce buffer size / 512).
const CHUNK_BLOCKS: usize = 8;

#[repr(C, align(64))]
struct Dma {
    desc: [u32; 16],
    buf: [u8; CHUNK_BLOCKS * 512],
}

static mut DMA: Dma = Dma { desc: [0; 16], buf: [0; CHUNK_BLOCKS * 512] };

fn dma_desc() -> usize {
    &raw mut DMA as usize
}

fn dma_buf() -> usize {
    dma_desc() + 64
}

const CMD_START: u32 = 1 << 31;
const CMD_USE_HOLD: u32 = 1 << 29;
const CMD_UPDATE_CLK: u32 = 1 << 21;
const CMD_SEND_INIT: u32 = 1 << 15;
const CMD_WAIT_PRVDATA: u32 = 1 << 13;
const CMD_AUTO_STOP: u32 = 1 << 12;
const CMD_WRITE: u32 = 1 << 10;
const CMD_DATA: u32 = 1 << 9;
const CMD_CHECK_CRC: u32 = 1 << 8;
const CMD_LONG_RESP: u32 = 1 << 7;
const CMD_RESP: u32 = 1 << 6;

const INT_RE: u32 = 1 << 1;
const INT_CMD_DONE: u32 = 1 << 2;
const INT_DATA_OVER: u32 = 1 << 3;
const INT_RCRC: u32 = 1 << 6;
const INT_DCRC: u32 = 1 << 7;
const INT_RTO: u32 = 1 << 8;
const INT_DRTO: u32 = 1 << 9;
const INT_HTO: u32 = 1 << 10;
const INT_FRUN: u32 = 1 << 11;
const INT_SBE: u32 = 1 << 13;
const INT_EBE: u32 = 1 << 15;
const CMD_ERRORS: u32 = INT_RE | INT_RCRC | INT_RTO | INT_HTO;
const DATA_ERRORS: u32 = INT_DCRC | INT_DRTO | INT_FRUN | INT_SBE | INT_EBE;

const STATUS_DATA_BUSY: u32 = 1 << 9;

pub const BLOCK: usize = 512;



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NoCard,
    Timeout,
    Crc,
    Command(u32),
    Data(u32),
    Unsupported,
}

#[derive(Clone, Copy)]
struct Card {
    /// SDHC/SDXC address blocks; SDSC addresses bytes.
    block_addressed: bool,
}

static CARD: Mutex<Option<Card>> = Mutex::new(None);
/// Serialises use of the controller (and its DMA bounce buffer) between the
/// SD card on slot 0 and the Wi-Fi co-processor on slot 1.
static HOST: Mutex<bool> = Mutex::new(false);

/// Shared host clock: PLL_F160M / HOST_DIV = 40 MHz. Each slot divides it
/// further by 2*m (m = 0: no division).
const HOST_DIV: u32 = 4;
pub const PROBE_DIV: u32 = 50; // 400 kHz

fn ldo4_on() {
    // LDO channel 4 in bypass ("tieh") mode gives the full 3.3 V rail.
    let ana = soc::PMU + 0x1DC;
    let ctrl = soc::PMU + 0x1D8;
    mmio::set_bits(ana, 1 << 26);
    let v = mmio::read(ctrl) & !(0b111 << 9);
    mmio::write(ctrl, v | (1 << 14) | (1 << 7));
    mmio::set_bits(ctrl, 1 << 8);
    systimer::delay_us(1000);
}

fn pads() {
    for pin in 39..=44u32 {
        let pad = soc::IO_MUX + 4 + 4 * pin as usize;
        // Function 0 (SDMMC), input enabled, pull-up, strong drive.
        mmio::write(pad, (3 << 10) | (1 << 9) | (1 << 8));
    }
    // Slot 0 sideband inputs from GPIO matrix constants: card present,
    // not write protected, no card interrupt.
    const CONST_LOW: u32 = 0x3E;
    const CONST_HIGH: u32 = 0x3F;
    for (sig, val) in [(126u32, CONST_LOW), (130, CONST_LOW), (128, CONST_HIGH)] {
        mmio::write(soc::GPIO + 0x158 + 4 * sig as usize, val | (1 << 7));
    }
}

fn host_clock() {
    let clk = soc::HP_SYS_CLKRST;
    let n = HOST_DIV;
    let p1 = mmio::read(clk + 0x34) & !((1 << 22) | (1 << 23));
    mmio::write(clk + 0x34, p1 | (1 << 24)); // PLL_F160M, enabled
    let edges = ((n / 2 - 1) << 13) | ((n - 1) << 17) | ((n - 1) << 9);
    let phases = (1 << 27) | (1 << 28) | (1 << 29) | (1 << 23); // drv edge 1
    mmio::write(clk + 0x38, edges | phases);
    mmio::write(clk + 0x38, edges | phases | (1 << 8));
}

/// Card clock for `slot` = 40 MHz / (2 * m), or 40 MHz for m = 0.
pub fn set_slot_clock(slot: u32, m: u32) -> Result<(), Error> {
    mmio::clear_bits(CLKENA, (1 << slot) | (1 << (16 + slot)));
    update_clock(slot)?;
    // Slot n uses divider n.
    let src = mmio::read(CLKSRC) & !(0b11 << (2 * slot));
    mmio::write(CLKSRC, src | (slot << (2 * slot)));
    let div = mmio::read(CLKDIV) & !(0xFF << (8 * slot));
    mmio::write(CLKDIV, div | (m << (8 * slot)));
    update_clock(slot)?;
    mmio::set_bits(CLKENA, (1 << slot) | (1 << (16 + slot)));
    update_clock(slot)
}

pub fn set_bus_width_4(slot: u32, four: bool) {
    if four { mmio::set_bits(CTYPE, 1 << slot) } else { mmio::clear_bits(CTYPE, 1 << slot) }
}

fn update_clock(slot: u32) -> Result<(), Error> {
    mmio::write(CMD, CMD_START | CMD_UPDATE_CLK | CMD_WAIT_PRVDATA | CMD_USE_HOLD | (slot << 16));
    wait(|| mmio::read(CMD) & CMD_START == 0)
}

fn wait(mut done: impl FnMut() -> bool) -> Result<(), Error> {
    let end = systimer::now_ticks() + 500_000 * systimer::TICKS_PER_US;
    while !done() {
        if systimer::now_ticks() > end {
            return Err(Error::Timeout);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
pub enum Resp {
    None,
    Short,
    ShortNoCrc,
    Long,
}

fn command(index: u32, arg: u32, resp: Resp, flags: u32) -> Result<[u32; 4], Error> {
    command_on(0, index, arg, resp, flags)
}

/// Issue a command on `slot` (caller holds the host lock).
pub fn command_on(slot: u32, index: u32, arg: u32, resp: Resp, flags: u32) -> Result<[u32; 4], Error> {
    wait(|| mmio::read(CMD) & CMD_START == 0)?;
    if flags & CMD_DATA == 0 {
        wait(|| mmio::read(STATUS) & STATUS_DATA_BUSY == 0)?;
    }
    mmio::write(RINTSTS, 0xFFFF_FFFF);
    mmio::write(CMDARG, arg);
    let rflags = match resp {
        Resp::None => 0,
        Resp::Short => CMD_RESP | CMD_CHECK_CRC,
        Resp::ShortNoCrc => CMD_RESP,
        Resp::Long => CMD_RESP | CMD_LONG_RESP | CMD_CHECK_CRC,
    };
    mmio::write(CMD, CMD_START | CMD_USE_HOLD | CMD_WAIT_PRVDATA | rflags | flags | (slot << 16) | index);
    wait(|| mmio::read(RINTSTS) & (INT_CMD_DONE | CMD_ERRORS) != 0)?;
    let st = mmio::read(RINTSTS);
    if st & INT_RTO != 0 {
        return Err(Error::Timeout);
    }
    if st & INT_RCRC != 0 && resp != Resp::ShortNoCrc {
        return Err(Error::Crc);
    }
    if st & (INT_RE | INT_HTO) != 0 {
        return Err(Error::Command(st));
    }
    Ok([mmio::read(RESP0), mmio::read(RESP0 + 4), mmio::read(RESP0 + 8), mmio::read(RESP0 + 12)])
}

fn app_command(rca: u32, index: u32, arg: u32, resp: Resp) -> Result<[u32; 4], Error> {
    command(55, rca << 16, Resp::Short, 0)?;
    command(index, arg, resp, 0)
}

/// Card capacity in 512-byte blocks from the CSD register.
fn csd_blocks(csd: [u32; 4]) -> u64 {
    // RESP0..3 hold CSD bits [31:0]..[127:96] (CRC byte stripped).
    let bits = |hi: u32, lo: u32| -> u64 {
        let mut v = 0u64;
        for b in (lo..=hi).rev() {
            v = (v << 1) | ((csd[(b / 32) as usize] >> (b % 32)) & 1) as u64;
        }
        v
    };
    match bits(127, 126) {
        1 => (bits(69, 48) + 1) * 1024,
        _ => {
            let c_size = bits(73, 62);
            let mult = bits(49, 47);
            let read_bl_len = bits(83, 80);
            ((c_size + 1) << (mult + 2)) << read_bl_len >> 9
        }
    }
}

/// Bus configuration used after identification (for bring-up experiments).
#[derive(Clone, Copy)]
pub struct BusConfig {
    pub four_bit: bool,
    /// (unused, card divider m): card clock = 40 MHz / (2 * m).
    pub clock: (u32, u32),
}

pub const DEFAULT_BUS: BusConfig = BusConfig { four_bit: true, clock: (0, 1) };

pub fn init() -> Result<u64, Error> {
    init_with(DEFAULT_BUS)
}

/// Power, clock and reset the controller once for both slots.
pub fn host_init() -> Result<(), Error> {
    let mut ready = HOST.lock();
    if *ready {
        return Ok(());
    }
    ldo4_on();
    let clk = soc::HP_SYS_CLKRST;
    mmio::set_bits(clk + 0x18, 1 << 14);
    let rst = soc::LP_CLKRST + 0x4C;
    mmio::set_bits(rst, 1 << 28);
    mmio::clear_bits(rst, 1 << 28);
    host_clock();
    mmio::write(CTRL, 0b111);
    wait(|| mmio::read(CTRL) & 0b111 == 0)?;
    mmio::write(RINTSTS, 0xFFFF_FFFF);
    mmio::write(INTMASK, 0);
    mmio::write(TMOUT, 0xFFFF_FF40);
    mmio::write(CTYPE, 0);
    *ready = true;
    Ok(())
}

/// Hold the controller for a sequence of commands.
pub fn lock_host() -> crate::sync::MutexGuard<'static, bool> {
    HOST.lock()
}

pub fn init_with(bus: BusConfig) -> Result<u64, Error> {
    host_init()?;
    let _host = HOST.lock();
    pads();
    set_slot_clock(0, PROBE_DIV)?;

    command(0, 0, Resp::None, CMD_SEND_INIT)?;
    let v2 = match command(8, 0x1AA, Resp::Short, 0) {
        Ok(r) if r[0] & 0xFFF == 0x1AA => true,
        Ok(_) => return Err(Error::Unsupported),
        Err(Error::Timeout) => false,
        Err(e) => return Err(e),
    };
    let hcs = if v2 { 1 << 30 } else { 0 };
    let mut ocr = 0;
    for _ in 0..200 {
        ocr = app_command(0, 41, 0x00FF_8000 | hcs, Resp::ShortNoCrc).map_err(|e| match e {
            Error::Timeout => Error::NoCard,
            e => e,
        })?[0];
        if ocr & (1 << 31) != 0 {
            break;
        }
        systimer::delay_us(10_000);
    }
    if ocr & (1 << 31) == 0 {
        return Err(Error::Timeout);
    }
    let block_addressed = ocr & (1 << 30) != 0;
    command(2, 0, Resp::Long, 0)?;
    let rca = command(3, 0, Resp::Short, 0)?[0] >> 16;
    let csd = command(9, rca << 16, Resp::Long, 0)?;
    let blocks = csd_blocks(csd);
    command(7, rca << 16, Resp::Short, 0)?;
    if bus.four_bit {
        app_command(rca, 6, 2, Resp::Short)?;
        mmio::write(CTYPE, 1);
    }
    command(16, BLOCK as u32, Resp::Short, 0)?;
    set_slot_clock(0, bus.clock.1)?;

    *CARD.lock() = Some(Card { block_addressed });
    kprintln!(
        "sd: {} card, {} MiB, rca {:#x}",
        if block_addressed { "SDHC/SDXC" } else { "SDSC" },
        blocks / 2048,
        rca
    );
    Ok(blocks)
}

/// One DMA transfer of up to CHUNK_BLOCKS blocks through the bounce buffer.
fn transfer_chunk(start: u64, count: usize, write: bool) -> Result<(), Error> {
    let card = CARD.lock().ok_or(Error::NoCard)?;
    let addr = if card.block_addressed { start as u32 } else { (start * BLOCK as u64) as u32 };
    let (index, flags) = match (write, count > 1) {
        (false, false) => (17, CMD_DATA),
        (false, true) => (18, CMD_DATA | CMD_AUTO_STOP),
        (true, false) => (24, CMD_DATA | CMD_WRITE),
        (true, true) => (25, CMD_DATA | CMD_WRITE | CMD_AUTO_STOP),
    };
    data_command(0, index, addr, flags, count * BLOCK, BLOCK)
}

pub const CMD_FLAG_DATA: u32 = CMD_DATA;
pub const CMD_FLAG_WRITE: u32 = CMD_WRITE;
/// Largest single data transfer through the bounce buffer.
pub const BOUNCE_BYTES: usize = CHUNK_BLOCKS * 512;

/// Run a data command moving `bytes` through the bounce buffer in blocks of
/// `block_size` (caller holds the host lock; fill/read the buffer with
/// `bounce_write`/`bounce_read`).
pub fn data_command(slot: u32, index: u32, arg: u32, flags: u32, bytes: usize, block_size: usize) -> Result<(), Error> {
    let desc = dma_desc() + UNCACHED;
    mmio::write(desc, DES0_OWN | DES0_FS | DES0_LD | DES0_CH);
    mmio::write(desc + 4, bytes as u32);
    mmio::write(desc + 8, dma_buf() as u32);
    mmio::write(desc + 12, 0);

    mmio::set_bits(CTRL, (1 << 1) | (1 << 2)); // FIFO + DMA interface reset
    wait(|| mmio::read(CTRL) & 0b110 == 0)?;
    mmio::write(IDSTS, 0xFFFF_FFFF);
    mmio::write(BMOD, BMOD_SWR);
    mmio::write(BMOD, BMOD_DE | BMOD_FB);
    mmio::set_bits(CTRL, CTRL_DMA_ENABLE | CTRL_USE_IDMAC);
    mmio::write(DBADDR, dma_desc() as u32);
    mmio::write(BLKSIZ, block_size as u32);
    mmio::write(BYTCNT, bytes as u32);
    mmio::write(PLDMND, 1);

    let result = command_on(slot, index, arg, Resp::Short, flags).and_then(|_| {
        wait(|| mmio::read(RINTSTS) & (INT_DATA_OVER | DATA_ERRORS) != 0)?;
        let st = mmio::read(RINTSTS);
        if st & DATA_ERRORS != 0 {
            return Err(Error::Data(st));
        }
        if mmio::read(IDSTS) & IDSTS_ERRORS != 0 {
            return Err(Error::Data(mmio::read(IDSTS) | 0x8000_0000));
        }
        wait(|| mmio::read(STATUS) & STATUS_DATA_BUSY == 0)
    });
    mmio::clear_bits(CTRL, CTRL_DMA_ENABLE | CTRL_USE_IDMAC);
    mmio::write(BMOD, 0);
    result
}

fn bounce(i: usize) -> usize {
    dma_buf() + UNCACHED + i
}

pub fn bounce_write(data: &[u8]) {
    for (i, chunk) in data.chunks(4).enumerate() {
        let mut w = [0u8; 4];
        w[..chunk.len()].copy_from_slice(chunk);
        mmio::write(bounce(i * 4), u32::from_le_bytes(w));
    }
}

pub fn bounce_read(out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(4).enumerate() {
        let w = mmio::read(bounce(i * 4)).to_le_bytes();
        chunk.copy_from_slice(&w[..chunk.len()]);
    }
}

pub fn read_blocks(start: u64, buf: &mut [u8]) -> Result<(), Error> {
    assert!(buf.len() % BLOCK == 0);
    let _host = HOST.lock();
    for (n, chunk) in buf.chunks_mut(CHUNK_BLOCKS * BLOCK).enumerate() {
        transfer_chunk(start + (n * CHUNK_BLOCKS) as u64, chunk.len() / BLOCK, false)?;
        for (i, word) in chunk.chunks_mut(4).enumerate() {
            word.copy_from_slice(&mmio::read(bounce(i * 4)).to_le_bytes());
        }
    }
    Ok(())
}

pub fn write_blocks(start: u64, buf: &[u8]) -> Result<(), Error> {
    assert!(buf.len() % BLOCK == 0);
    let _host = HOST.lock();
    for (n, chunk) in buf.chunks(CHUNK_BLOCKS * BLOCK).enumerate() {
        for (i, word) in chunk.chunks(4).enumerate() {
            mmio::write(bounce(i * 4), u32::from_le_bytes(word.try_into().unwrap()));
        }
        transfer_chunk(start + (n * CHUNK_BLOCKS) as u64, chunk.len() / BLOCK, true)?;
    }
    Ok(())
}



/// Raw controller state for bring-up debugging.
pub fn dump() {
    kprintln!(
        "sdmmc: ctrl {:#x} clkdiv {:#x} clksrc {:#x} clkena {:#x} ctype {:#x} status {:#x} rint {:#x} cmd {:#x}",
        mmio::read(CTRL), mmio::read(CLKDIV), mmio::read(CLKSRC), mmio::read(CLKENA), mmio::read(CTYPE),
        mmio::read(STATUS), mmio::read(RINTSTS), mmio::read(CMD)
    );
    kprintln!(
        "sdmmc: bytcnt {:#x} blksiz {:#x} tcbcnt {:#x} tbbcnt {:#x} idsts {:#x} bmod {:#x} resp0 {:#x}",
        mmio::read(BYTCNT), mmio::read(BLKSIZ), mmio::read(BASE + 0x5C), mmio::read(BASE + 0x60),
        mmio::read(IDSTS), mmio::read(BMOD), mmio::read(RESP0)
    );
}
