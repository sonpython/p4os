//! ESP32-C6 Wi-Fi/BLE co-processor running ESP-Hosted (v2) slave firmware,
//! reached as an SDIO device on SDMMC slot 1 (GPIO matrix pins CLK 18,
//! CMD 19, D0-D3 14-17; reset/enable on GPIO54).
//!
//! This module covers the transport: SDIO bring-up, the slave's host
//! interface registers, and reading/writing ESP-Hosted frames.

pub use crate::sdmmc::Error;
use crate::sdmmc::{self, Resp};
use crate::{gpio, kprintln, mmio, soc, systimer};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

const PIN_CLK: u32 = 18;
const PIN_CMD: u32 = 19;
const PIN_D0: u32 = 14;
const PIN_RESET: u32 = 54;
const SLOT: u32 = 1;

/// Slave host-interface registers (function 1, address & 0x3FF).
const REG_TOKEN_RDATA: u32 = 0x044;
const REG_INT_ST: u32 = 0x058;
const REG_PACKET_LEN: u32 = 0x060;
/// Host-to-slave interrupt (scratch register 7); bit n = request n.
const REG_HOST_TO_SLAVE: u32 = 0x08C;
const OPEN_DATA_PATH: u8 = 1 << 0;
const CMD53_END_ADDR: u32 = 0x1F800;
const BLOCK_SIZE: usize = 512;
const RX_BYTE_MAX: u32 = 0x10_0000;

/// Bytes consumed from the slave's cumulative RX counter.
static RX_COUNT: AtomicU32 = AtomicU32::new(0);
/// Slave RX buffers used so far (wraps at 0x1000), for credit accounting.
static TX_BUFFERS: AtomicU32 = AtomicU32::new(0);
const TX_BUFFER_MAX: u32 = 0x1000;
const SLAVE_BUFFER_SIZE: usize = 1536;

fn pins() {
    // CLK out only; CMD and D0-D3 bidirectional with peripheral-driven OE.
    let route = |pin: u32, sig: u32, input: bool| {
        let pad = soc::IO_MUX + 4 + 4 * pin as usize;
        mmio::write(pad, (1 << 12) | (2 << 10) | (1 << 9) | (1 << 8)); // GPIO func, IE, pull-up
        mmio::write(soc::GPIO + 0x558 + 4 * pin as usize, sig);
        if input {
            mmio::write(soc::GPIO + 0x158 + 4 * sig as usize, pin | (1 << 7));
        }
    };
    route(PIN_CLK, 0, false);
    mmio::write(soc::GPIO + 0x24, 1 << PIN_CLK);
    route(PIN_CMD, 1, true);
    for i in 0..4 {
        route(PIN_D0 + i, 2 + i, true);
    }
    // Like the factory firmware, hold D3 high as a plain GPIO during card
    // identification so the slave selects SD (not SPI) mode on CMD0.
    gpio::output(PIN_D0 + 3, true);
    // Slot-1 sideband inputs: card present, not write protected, no INT.
    for (sig, val) in [(127u32, 0x3E), (131, 0x3E), (129, 0x3F)] {
        mmio::write(soc::GPIO + 0x158 + 4 * sig as usize, val | (1 << 7));
    }
}

fn reset_c6() {
    gpio::output(PIN_RESET, true);
    systimer::delay_us(10_000);
    gpio::set(PIN_RESET, false);
    systimer::delay_us(10_000);
    gpio::set(PIN_RESET, true);
}

/// CMD52: read or write one byte in function `func`'s register space.
fn cmd52(func: u32, addr: u32, write: Option<u8>) -> Result<u8, Error> {
    let arg = (write.is_some() as u32) << 31 | func << 28 | (write.is_some() as u32) << 27 | (addr & 0x1FFFF) << 9
        | write.unwrap_or(0) as u32;
    let r = sdmmc::command_on(SLOT, 52, arg, Resp::Short, 0)?;
    if r[0] & 0xCB00 != 0 {
        return Err(Error::Command(r[0]));
    }
    Ok(r[0] as u8)
}

/// CMD53 on function 1 with incrementing address. `len` <= bounce size;
/// whole 512-byte blocks use block mode, the rest byte mode.
fn cmd53(addr: u32, buf: &mut [u8], write: bool) -> Result<(), Error> {
    let mut done = 0;
    while done < buf.len() {
        let left = buf.len() - done;
        let (block_mode, count, bytes, bsize) = if left >= BLOCK_SIZE {
            let blocks = (left / BLOCK_SIZE).min(sdmmc::BOUNCE_BYTES / BLOCK_SIZE);
            (1, blocks as u32, blocks * BLOCK_SIZE, BLOCK_SIZE)
        } else {
            (0, (left as u32) & 0x1FF, left, left)
        };
        let a = addr + done as u32;
        let arg = (write as u32) << 31 | 1 << 28 | block_mode << 27 | 1 << 26 | (a & 0x1FFFF) << 9 | count;
        let flags = sdmmc::CMD_FLAG_DATA | if write { sdmmc::CMD_FLAG_WRITE } else { 0 };
        if write {
            sdmmc::bounce_write(&buf[done..done + bytes]);
        }
        sdmmc::data_command(SLOT, 53, arg, flags, bytes, bsize)?;
        if !write {
            sdmmc::bounce_read(&mut buf[done..done + bytes]);
        }
        done += bytes;
    }
    Ok(())
}

fn read_reg(reg: u32) -> Result<u32, Error> {
    let mut b = [0u8; 4];
    cmd53(reg & 0x3FF, &mut b, false)?;
    Ok(u32::from_le_bytes(b))
}


/// Bring the C6 up as an SDIO device with function 1 enabled, 4-bit bus.
pub fn init() -> Result<(), Error> {
    sdmmc::host_init()?;
    pins();
    reset_c6();
    systimer::delay_us(2_000_000); // let the C6 boot its firmware

    let _host = sdmmc::lock_host();
    sdmmc::set_bus_width_4(SLOT, false);
    sdmmc::set_slot_clock(SLOT, sdmmc::PROBE_DIV)?;
    let step = |name: &str, r: Result<[u32; 4], Error>| {
        kprintln!("c6: {} -> {:x?}", name, r.map(|v| v[0]));
        r
    };
    step("CMD0", sdmmc::command_on(SLOT, 0, 0, Resp::None, 1 << 15))?;
    let mut first = Err(Error::Timeout);
    for _ in 0..10 {
        first = sdmmc::command_on(SLOT, 5, 0, Resp::ShortNoCrc, 0);
        if first.is_ok() {
            break;
        }
        systimer::delay_us(100_000);
    }
    let mut ocr = step("CMD5", first)?[0];
    for _ in 0..100 {
        ocr = sdmmc::command_on(SLOT, 5, ocr & 0x00FF_FFFF, Resp::ShortNoCrc, 0)?[0];
        if ocr & (1 << 31) != 0 {
            break;
        }
        systimer::delay_us(10_000);
    }
    if ocr & (1 << 31) == 0 {
        return Err(Error::Timeout);
    }
    let rca = sdmmc::command_on(SLOT, 3, 0, Resp::Short, 0)?[0] >> 16;
    sdmmc::command_on(SLOT, 7, rca << 16, Resp::Short, 0)?;

    let cccr_rev = cmd52(0, 0x00, None)?;
    cmd52(0, 0x02, Some(0x02))?; // enable function 1
    let end = systimer::now_ticks() + 1_000_000 * systimer::TICKS_PER_US;
    while cmd52(0, 0x03, None)? & 0x02 == 0 {
        if systimer::now_ticks() > end {
            return Err(Error::Timeout);
        }
    }
    cmd52(0, 0x04, Some(0x03))?; // interrupts: master + function 1
    // D3 back to the SDMMC data signal for 4-bit transfers.
    mmio::write(soc::GPIO + 0x558 + 4 * (PIN_D0 + 3) as usize, 5);
    let bus = cmd52(0, 0x07, None)?;
    cmd52(0, 0x07, Some((bus & !0x03) | 0x02))?; // 4-bit
    sdmmc::set_bus_width_4(SLOT, true);
    cmd52(0, 0x110, Some((BLOCK_SIZE & 0xFF) as u8))?;
    cmd52(0, 0x111, Some((BLOCK_SIZE >> 8) as u8))?;
    sdmmc::set_slot_clock(SLOT, 1)?; // 20 MHz

    kprintln!("c6: SDIO up, ocr {:#010x} rca {:#x} cccr rev {:#04x}", ocr, rca, cccr_rev);
    let token = read_reg(REG_TOKEN_RDATA)?;
    let pkt = read_reg(REG_PACKET_LEN)?;
    let int = read_reg(REG_INT_ST)?;
    kprintln!("c6: tx credits {} rx bytes pending {} int_st {:#x}", (token >> 16) & 0xFFF, pkt & 0xF_FFFF, int);
    // Tell the slave the host is ready; it answers with an INIT event.
    cmd52(1, REG_HOST_TO_SLAVE, Some(OPEN_DATA_PATH))?;
    Ok(())
}

/// Read whatever the slave has queued (may hold several frames).
pub fn receive() -> Result<Vec<u8>, Error> {
    let _host = sdmmc::lock_host();
    // Received data is found by polling PACKET_LEN, so the slave's
    // interrupt status is not consulted or cleared here.
    let total = read_reg(REG_PACKET_LEN).inspect_err(|e| {
        kprintln!("c6: PACKET_LEN read failed {:?}", e);
        sdmmc::dump();
    })? & 0xF_FFFF;
    let consumed = RX_COUNT.load(Ordering::Relaxed);
    let len = (total + RX_BYTE_MAX - consumed) % RX_BYTE_MAX;
    if len == 0 {
        return Ok(Vec::new());
    }
    // Read exactly the pending bytes: reading past the end of the slave's
    // window desynchronises its stream and it stops answering commands.
    let mut buf = vec![0u8; len as usize];
    if let Err(e) = cmd53(CMD53_END_ADDR - len, &mut buf, false) {
        kprintln!("c6: rx of {} bytes (total {:#x}, consumed {:#x}) failed: {:?}", len, total, consumed, e);
        sdmmc::dump();
        return Err(e);
    }
    buf.truncate(len as usize);
    RX_COUNT.store((consumed + len) % RX_BYTE_MAX, Ordering::Relaxed);
    Ok(buf)
}

/// Send one ESP-Hosted frame (header included), waiting for a free slave
/// buffer if needed.
pub fn send(frame: &[u8]) -> Result<(), Error> {
    assert!(frame.len() <= SLAVE_BUFFER_SIZE);
    let _host = sdmmc::lock_host();
    let end = systimer::now_ticks() + 1_000_000 * systimer::TICKS_PER_US;
    loop {
        let token = (read_reg(REG_TOKEN_RDATA)? >> 16) & 0xFFF;
        let used = TX_BUFFERS.load(Ordering::Relaxed);
        if (token + TX_BUFFER_MAX - used) % TX_BUFFER_MAX > 0 {
            break;
        }
        if systimer::now_ticks() > end {
            return Err(Error::Timeout);
        }
    }
    let mut buf = vec![0u8; (frame.len() + 3) & !3];
    buf[..frame.len()].copy_from_slice(frame);
    let len = buf.len() as u32;
    cmd53(CMD53_END_ADDR - len, &mut buf, true)?;
    TX_BUFFERS.store((TX_BUFFERS.load(Ordering::Relaxed) + 1) % TX_BUFFER_MAX, Ordering::Relaxed);
    Ok(())
}

/// Shell probe: init and dump the first frames the slave sends.
pub fn probe() {
    if let Err(e) = init() {
        kprintln!("c6: init failed: {:?}", e);
        sdmmc::dump();
        return;
    }
    let t0 = systimer::now_ticks();
    for _ in 0..20 {
        let ms = (systimer::now_ticks() - t0) / (systimer::TICKS_PER_US * 1000);
        match receive() {
            Ok(data) if !data.is_empty() => {
                kprintln!("c6: +{} ms rx {} bytes: {:02x?}", ms, data.len(), &data[..data.len().min(32)]);
            }
            Ok(_) => kprintln!("c6: +{} ms idle", ms),
            Err(e) => kprintln!("c6: +{} ms rx error {:?}", ms, e),
        }
        systimer::delay_us(20_000);
    }
}
