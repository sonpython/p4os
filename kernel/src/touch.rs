//! GT911 capacitive touch controller at I2C address 0x5D. Its interrupt
//! and reset lines are not wired to the P4, so it is polled.

use crate::i2c;

pub const ADDR: u8 = 0x5D;
const REG_STATUS: u16 = 0x814E;
const REG_POINT0: u16 = 0x814F;
const REG_PRODUCT_ID: u16 = 0x8140;
const STATUS_READY: u8 = 1 << 7;

#[derive(Clone, Copy, Debug)]
pub struct Point {
    /// Panel coordinates in its native portrait space (0..480, 0..800).
    pub x: i32,
    pub y: i32,
}

fn reg(r: u16) -> [u8; 2] {
    r.to_be_bytes()
}

pub fn product_id() -> Option<[u8; 4]> {
    let mut id = [0u8; 4];
    i2c::write_read(ADDR, &reg(REG_PRODUCT_ID), &mut id).ok()?;
    Some(id)
}

/// Latest touch state: `Some(point)` while a finger is down, `None` when
/// released. Returns `Err(())` when the controller has no new sample.
pub fn poll() -> Result<Option<Point>, ()> {
    let mut status = [0u8; 1];
    i2c::write_read(ADDR, &reg(REG_STATUS), &mut status).map_err(|_| ())?;
    if status[0] & STATUS_READY == 0 {
        return Err(());
    }
    let count = status[0] & 0x0F;
    let mut result = None;
    if (1..=5).contains(&count) {
        let mut p = [0u8; 8];
        if i2c::write_read(ADDR, &reg(REG_POINT0), &mut p).is_ok() {
            let x = u16::from_le_bytes([p[1], p[2]]) as i32;
            let y = u16::from_le_bytes([p[3], p[4]]) as i32;
            result = Some(Point { x, y });
        }
    }
    // The GT911 stops updating until the status register is cleared.
    let _ = i2c::write(ADDR, &reg(REG_STATUS), &[0]);
    Ok(result)
}
