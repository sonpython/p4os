//! Watchdogs left running by the ROM and the 2nd-stage bootloader.

use crate::{mmio, soc};

const WKEY: u32 = 0x50D8_3AA1;

const LP_WDT_CONFIG0: usize = soc::LP_WDT;
const LP_WDT_WPROTECT: usize = soc::LP_WDT + 0x18;
const LP_WDT_SWD_CONFIG: usize = soc::LP_WDT + 0x1C;
const LP_WDT_SWD_WPROTECT: usize = soc::LP_WDT + 0x20;
const LP_WDT_EN: u32 = 1 << 31;
const LP_WDT_FLASHBOOT_MOD_EN: u32 = 1 << 12;
const SWD_AUTO_FEED_EN: u32 = 1 << 18;

const TIMG_WDTCONFIG0: usize = 0x48;
const TIMG_WDTWPROTECT: usize = 0x64;
const TIMG_WDT_EN: u32 = 1 << 31;
const TIMG_WDT_FLASHBOOT_MOD_EN: u32 = 1 << 14;

/// Disable the RTC watchdog and both timer-group watchdogs, and let the
/// super watchdog feed itself. Without this the chip resets within seconds.
pub fn disable_all() {
    mmio::write(LP_WDT_WPROTECT, WKEY);
    mmio::clear_bits(LP_WDT_CONFIG0, LP_WDT_EN | LP_WDT_FLASHBOOT_MOD_EN);
    mmio::write(LP_WDT_WPROTECT, 0);

    mmio::write(LP_WDT_SWD_WPROTECT, WKEY);
    mmio::set_bits(LP_WDT_SWD_CONFIG, SWD_AUTO_FEED_EN);
    mmio::write(LP_WDT_SWD_WPROTECT, 0);

    for base in [soc::TIMG0, soc::TIMG1] {
        mmio::write(base + TIMG_WDTWPROTECT, WKEY);
        mmio::clear_bits(base + TIMG_WDTCONFIG0, TIMG_WDT_EN | TIMG_WDT_FLASHBOOT_MOD_EN);
        mmio::write(base + TIMG_WDTWPROTECT, 0);
    }
}
