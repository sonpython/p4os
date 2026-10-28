//! Read-only access to factory eFuse fields.

use crate::{mmio, soc};

const RD_MAC_SYS_0: usize = soc::EFUSE + 0x44;
const RD_MAC_SYS_1: usize = soc::EFUSE + 0x48;
const RD_MAC_SYS_2: usize = soc::EFUSE + 0x4C;

/// Chip revision as (major, minor), e.g. (1, 0) for v1.0.
pub fn chip_revision() -> (u32, u32) {
    let w = mmio::read(RD_MAC_SYS_2);
    ((w >> 4) & 0x3, w & 0xF)
}

pub fn mac() -> [u8; 6] {
    let lo = mmio::read(RD_MAC_SYS_0).to_le_bytes();
    let hi = mmio::read(RD_MAC_SYS_1).to_le_bytes();
    [hi[1], hi[0], lo[3], lo[2], lo[1], lo[0]]
}
