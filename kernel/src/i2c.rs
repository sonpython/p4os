//! I2C master (controller I2C1) on the board's shared bus: GPIO7 = SDA,
//! GPIO8 = SCL, 100 kHz, 2.2 kOhm external pull-ups. Carries the GT911
//! touch controller, the ES8311 codec and the camera connector.
//!
//! Transfers use the controller's command list (up to 8 commands) and its
//! 32-byte FIFO, so a single transaction moves at most 31 data bytes.

use crate::sync::Mutex;
use crate::{mmio, soc, systimer};

const BASE: usize = soc::HPPERIPH1 + 0x5000;
const SDA_PIN: u32 = 7;
const SCL_PIN: u32 = 8;
const SIG_SCL: u32 = 70;
const SIG_SDA: u32 = 71;

const CTR: usize = BASE + 0x04;
const TO: usize = BASE + 0x0C;
const FIFO_CONF: usize = BASE + 0x18;
const DATA: usize = BASE + 0x1C;
const INT_RAW: usize = BASE + 0x20;
const INT_CLR: usize = BASE + 0x24;
const COMD0: usize = BASE + 0x58;

const CTR_MS_MODE: u32 = 1 << 4;
const CTR_TRANS_START: u32 = 1 << 5;
const CTR_CLK_EN: u32 = 1 << 8;
const CTR_FSM_RST: u32 = 1 << 10;
const CTR_CONF_UPGATE: u32 = 1 << 11;

const INT_ARB_LOST: u32 = 1 << 5;
const INT_TRANS_COMPLETE: u32 = 1 << 7;
const INT_TIMEOUT: u32 = 1 << 8;
const INT_NACK: u32 = 1 << 10;

const OP_WRITE: u32 = 1;
const OP_STOP: u32 = 2;
const OP_READ: u32 = 3;
const OP_RSTART: u32 = 6;
const ACK_CHECK: u32 = 1 << 8;
const ACK_VALUE_NACK: u32 = 1 << 10;

pub const MAX_CHUNK: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Nack,
    Timeout,
    ArbitrationLost,
}

static BUS: Mutex<bool> = Mutex::new(false);

fn route_pin(pin: u32, signal: u32) {
    let pad = soc::IO_MUX + 4 + 4 * pin as usize;
    // GPIO function, input enabled, internal pull-up as a backup.
    let v = mmio::read(pad) & !(0x7 << 12);
    mmio::write(pad, v | (1 << 12) | (1 << 9) | (1 << 8));
    mmio::set_bits(soc::GPIO + 0x74 + 4 * pin as usize, 1 << 2); // open drain
    mmio::write(soc::GPIO + 0x558 + 4 * pin as usize, signal);
    mmio::write(soc::GPIO + 0x24, 1 << pin); // output enable (pins < 32)
    mmio::write(soc::GPIO + 0x158 + 4 * signal as usize, pin | (1 << 7));
}

pub fn init() {
    let clk = soc::HP_SYS_CLKRST;
    mmio::set_bits(clk + 0x1C, 1 << 13);
    mmio::clear_bits(clk + 0x40, 1 << 26); // source: XTAL 40 MHz
    mmio::set_bits(clk + 0x40, 1 << 27);
    mmio::clear_bits(clk + 0x44, 0xFF); // divider 1
    mmio::set_bits(clk + 0xC4, 1 << 21);
    mmio::clear_bits(clk + 0xC4, 1 << 21);

    route_pin(SDA_PIN, SIG_SDA);
    route_pin(SCL_PIN, SIG_SCL);

    mmio::write(CTR, CTR_MS_MODE | CTR_CLK_EN);
    // FIFO mode with pointer protection; pulse both FIFO resets.
    mmio::write(FIFO_CONF, (1 << 14) | (1 << 12) | (1 << 13));
    mmio::write(FIFO_CONF, 1 << 14);

    // 100 kHz from 40 MHz: half period 200 clocks (ESP-IDF formula).
    let half = 200;
    let wait_high = half / 2 - 2;
    mmio::write(BASE + 0x00, half - 1);
    mmio::write(BASE + 0x38, (half - wait_high) | (wait_high << 9));
    mmio::write(BASE + 0x30, half / 4 - 1);
    mmio::write(BASE + 0x34, half / 2 - 1);
    for off in [0x40, 0x44, 0x48, 0x4C] {
        mmio::write(BASE + off, half - 1);
    }
    mmio::write(TO, 12 | (1 << 5));
    mmio::set_bits(CTR, CTR_CONF_UPGATE);
    *BUS.lock() = true;
}

fn cmd(i: usize, op: u32, bytes: u32, extra: u32) {
    mmio::write(COMD0 + i * 4, (op << 11) | extra | bytes);
}

fn reset_after_error() {
    mmio::set_bits(CTR, CTR_FSM_RST);
    mmio::write(FIFO_CONF, (1 << 14) | (1 << 12) | (1 << 13));
    mmio::write(FIFO_CONF, 1 << 14);
    mmio::clear_bits(CTR, CTR_FSM_RST);
}

fn run() -> Result<(), Error> {
    mmio::write(INT_CLR, 0xFFFF_FFFF);
    mmio::set_bits(CTR, CTR_CONF_UPGATE);
    mmio::set_bits(CTR, CTR_TRANS_START);
    let deadline = systimer::now_ticks() + 20_000 * systimer::TICKS_PER_US;
    loop {
        let raw = mmio::read(INT_RAW);
        let err = if raw & INT_NACK != 0 {
            Some(Error::Nack)
        } else if raw & INT_ARB_LOST != 0 {
            Some(Error::ArbitrationLost)
        } else if raw & INT_TIMEOUT != 0 || systimer::now_ticks() > deadline {
            Some(Error::Timeout)
        } else {
            None
        };
        if let Some(e) = err {
            reset_after_error();
            mmio::write(INT_CLR, 0xFFFF_FFFF);
            return Err(e);
        }
        if raw & INT_TRANS_COMPLETE != 0 {
            mmio::write(INT_CLR, 0xFFFF_FFFF);
            return Ok(());
        }
    }
}

/// Write `prefix` then `data` to device `addr` (7-bit).
pub fn write(addr: u8, prefix: &[u8], data: &[u8]) -> Result<(), Error> {
    let _bus = BUS.lock();
    let n = 1 + prefix.len() + data.len();
    assert!(n <= 32);
    mmio::write(DATA, (addr as u32) << 1);
    for &b in prefix.iter().chain(data) {
        mmio::write(DATA, b as u32);
    }
    cmd(0, OP_RSTART, 0, 0);
    cmd(1, OP_WRITE, n as u32, ACK_CHECK);
    cmd(2, OP_STOP, 0, 0);
    run()
}

/// Write `reg` (register address bytes) then read `buf.len()` bytes.
pub fn write_read(addr: u8, reg: &[u8], buf: &mut [u8]) -> Result<(), Error> {
    assert!(!buf.is_empty() && buf.len() <= MAX_CHUNK);
    let _bus = BUS.lock();
    mmio::write(DATA, (addr as u32) << 1);
    for &b in reg {
        mmio::write(DATA, b as u32);
    }
    mmio::write(DATA, ((addr as u32) << 1) | 1);
    let mut i = 0;
    cmd(i, OP_RSTART, 0, 0);
    i += 1;
    if !reg.is_empty() {
        cmd(i, OP_WRITE, 1 + reg.len() as u32, ACK_CHECK);
        i += 1;
        cmd(i, OP_RSTART, 0, 0);
        i += 1;
    } else {
        // No register phase: skip the first address byte in the FIFO.
        unreachable!("register-less reads are not used");
    }
    cmd(i, OP_WRITE, 1, ACK_CHECK);
    i += 1;
    if buf.len() > 1 {
        cmd(i, OP_READ, buf.len() as u32 - 1, 0);
        i += 1;
    }
    cmd(i, OP_READ, 1, ACK_VALUE_NACK);
    i += 1;
    cmd(i, OP_STOP, 0, 0);
    run()?;
    for b in buf.iter_mut() {
        *b = mmio::read(DATA) as u8;
    }
    Ok(())
}

/// True if a device acknowledges its address.
pub fn probe(addr: u8) -> bool {
    write(addr, &[], &[]).is_ok()
}
