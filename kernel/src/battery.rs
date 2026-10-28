//! Battery voltage on the Guition JC4880P443C.
//!
//! The schematic (JC4880P443_V1.0) divides BAT+ by R52 68k / R57 100k into
//! GPIO53 (through R55, 0 Ω), which is ADC2 channel 4. The vendor example
//! reads it with 12 dB attenuation (4.2 V -> ~2.5 V at the pin).
//!
//! One-shot conversions use the LP ADC's RTC controller as ESP-IDF v5.5
//! `adc_oneshot` does on the P4 (`hal/esp32p4/include/hal/adc_ll.h`). Raw
//! codes are converted with the factory eFuse calibration, following
//! `esp_efuse_rtc_calib.c` and `adc_cali_curve_fitting.c` (calibration
//! version 1): a hardware init code, a per-channel code offset, one
//! reference point and a linear error term.
//!
//! The IP5306 charger sits between battery and board, so on USB power the
//! reading is the charging voltage, not the resting cell voltage.

use crate::sync::Mutex;
use crate::{mmio, regi2c, soc, systimer};
use core::sync::atomic::{AtomicU32, Ordering};

const PIN: u32 = 53;
const CHANNEL: u32 = 4;
/// 12 dB.
const ATTEN: u32 = 3;
/// R52 + R57 over R57.
const DIVIDER_NUM: u32 = 168;
const DIVIDER_DEN: u32 = 100;
const SAMPLES: u32 = 16;
/// A reading is reused for this long.
const CACHE_MS: u32 = 5000;

const LP_PERI_CLK_EN: usize = soc::LPPERIPH;
const CK_EN_LP_ADC: u32 = 1 << 23;
const LP_ADC: usize = soc::LPPERIPH + 0x7000;
const READER2_CTRL: usize = LP_ADC + 0x24;
const MEAS2_CTRL2: usize = LP_ADC + 0x30;
const MEAS2_MUX: usize = LP_ADC + 0x34;
const ATTEN2: usize = LP_ADC + 0x38;
const FORCE_WPD_SAR: usize = LP_ADC + 0x3C;
const HP_ADC_CTRL: usize = soc::HPPERIPH1 + 0x1E000;
const CLKRST: usize = soc::HP_SYS_CLKRST;

const MEAS2_DONE: u32 = 1 << 16;
const MEAS2_START: u32 = 1 << 17;
const MEAS2_START_FORCE: u32 = 1 << 18;
const SAR2_EN_PAD_SHIFT: u32 = 19;
const SAR2_EN_PAD_FORCE: u32 = 1 << 31;
const SAR2_RTC_FORCE: u32 = 1 << 31;
const XPD_FORCE_PU: u32 = 3;

/// SAR ADC analog block on the regi2c bus and its SAR2 registers.
const SAR2_INIT_CODE_LOW: u32 = 0x3;
const SAR2_INIT_CODE_HIGH: u32 = 0x4;
const SAR2_DREF: u32 = 0x5;
const SAR2_ENCAL_GND: u32 = 0x7;

/// eFuse words holding the ADC2 calibration (`efuse_reg.h`).
const EFUSE_RD_MAC_SYS_2: usize = soc::EFUSE + 0x4C;
const EFUSE_SYS_PART1_DATA6: usize = soc::EFUSE + 0x74;
const EFUSE_SYS_PART2_DATA0: usize = soc::EFUSE + 0x15C;
const EFUSE_SYS_PART2_DATA1: usize = soc::EFUSE + 0x160;
const EFUSE_SYS_PART2_DATA2: usize = soc::EFUSE + 0x164;
/// Calibration reference for 12 dB: the code `digi` reads `CAL_MV`.
const CAL_MV: u32 = 2300;
const CAL_DIGI_BASE: i32 = 2350;
const INIT_CODE_BASE: u32 = 1400;
/// Curve-fitting error term for ADC2 at 12 dB: err = -K0 + K1 * v.
const ERR_K0: f32 = 2.442_140_1;
const ERR_K1: f32 = 0.000_945_850_1;

#[derive(Clone, Copy, Debug)]
pub struct Calibration {
    pub init_code: u32,
    /// Raw code offset of channel 4 at 12 dB.
    pub chan_offset: i32,
    /// Raw code that reads `CAL_MV`.
    pub digi: u32,
}

struct State {
    cal: Option<Calibration>,
    ready: bool,
    at: u32,
    mv: Option<u32>,
}

static STATE: Mutex<State> = Mutex::new(State { cal: None, ready: false, at: 0, mv: None });
static LAST_RAW: AtomicU32 = AtomicU32::new(0);

fn signed(v: u32, sign_bit: u32) -> i32 {
    let mag = (v & !(1 << sign_bit)) as i32;
    if v & (1 << sign_bit) != 0 { -mag } else { mag }
}

/// Factory calibration, if this chip's eFuse block carries version 1 data.
fn calibration() -> Option<Calibration> {
    let blk_version = ((mmio::read(EFUSE_RD_MAC_SYS_2) >> 11) & 0x3) * 100 + ((mmio::read(EFUSE_RD_MAC_SYS_2) >> 8) & 0x7);
    if !(1..100).contains(&blk_version) {
        return None;
    }
    let init_code = ((mmio::read(EFUSE_SYS_PART1_DATA6) >> 6) & 0x3FF) + INIT_CODE_BASE;
    let hi_dout = (mmio::read(EFUSE_SYS_PART2_DATA0) >> 30) | ((mmio::read(EFUSE_SYS_PART2_DATA1) & 0xFF) << 2);
    let digi = (CAL_DIGI_BASE + signed(hi_dout, 9)).max(1) as u32;
    let diff = (mmio::read(EFUSE_SYS_PART2_DATA2) >> 24) & 0xF;
    Some(Calibration { init_code, chan_offset: signed(diff, 3) * (4 - ATTEN as i32), digi })
}

/// Raw 12-bit code to millivolts at the pin.
pub fn raw_to_mv(raw: u32, cal: &Calibration) -> u32 {
    let raw = (raw as i32 - cal.chan_offset).clamp(0, 4095) as f32;
    let v = raw * CAL_MV as f32 / cal.digi as f32;
    let err = -ERR_K0 + ERR_K1 * v;
    (v - err).max(0.0) as u32
}

fn setup(cal: Option<&Calibration>) {
    // GPIO53 as an analog input: GPIO function, no input buffer or pulls.
    mmio::write(soc::IO_MUX + 4 + 4 * PIN as usize, 1 << 12);
    mmio::set_bits(CLKRST + 0x1C, 1 << 5); // ADC APB clock
    mmio::set_bits(CLKRST + 0xA0, 1 << 0); // ADC function clock
    mmio::set_bits(LP_PERI_CLK_EN, CK_EN_LP_ADC);
    mmio::modify(HP_ADC_CTRL, 0x3 << 28, XPD_FORCE_PU << 28);
    mmio::modify(FORCE_WPD_SAR, 0x3 << 2, XPD_FORCE_PU << 2);

    regi2c::write_mask(regi2c::SAR_ADC, SAR2_DREF, 6, 4, 4);
    if let Some(c) = cal {
        regi2c::write_mask(regi2c::SAR_ADC, SAR2_INIT_CODE_HIGH, 3, 0, (c.init_code >> 8) as u8);
        regi2c::write_mask(regi2c::SAR_ADC, SAR2_INIT_CODE_LOW, 7, 0, c.init_code as u8);
    }
    regi2c::write_mask(regi2c::SAR_ADC, SAR2_ENCAL_GND, 7, 7, 0);

    mmio::modify(READER2_CTRL, 0xFF | (1 << 29), 1); // clock /1, not inverted
    mmio::modify(MEAS2_MUX, 0x7 << 28, 4 << 28); // PA power detector cap (PHY default)
    let shift = 2 * (CHANNEL + 2);
    mmio::modify(ATTEN2, 0x3 << shift, ATTEN << shift);
    // RTC controller, software start, channel selected by software.
    mmio::set_bits(MEAS2_MUX, SAR2_RTC_FORCE);
    mmio::modify(MEAS2_CTRL2, 0xFFF << SAR2_EN_PAD_SHIFT, (1 << (CHANNEL + 2)) << SAR2_EN_PAD_SHIFT);
    mmio::set_bits(MEAS2_CTRL2, MEAS2_START_FORCE | SAR2_EN_PAD_FORCE);
}

fn convert() -> Option<u32> {
    mmio::clear_bits(MEAS2_CTRL2, MEAS2_START);
    mmio::set_bits(MEAS2_CTRL2, MEAS2_START);
    let end = systimer::now_ticks() + 1000 * systimer::TICKS_PER_US;
    while mmio::read(MEAS2_CTRL2) & MEAS2_DONE == 0 {
        if systimer::now_ticks() > end {
            return None;
        }
    }
    Some(mmio::read(MEAS2_CTRL2) & 0xFFFF)
}

fn measure(state: &mut State) -> Option<u32> {
    if !state.ready {
        state.cal = calibration();
        setup(state.cal.as_ref());
        state.ready = true;
    }
    let cal = state.cal?;
    let mut sum = 0;
    for _ in 0..SAMPLES {
        sum += convert()?;
    }
    let raw = sum / SAMPLES;
    LAST_RAW.store(raw, Ordering::Relaxed);
    Some(raw_to_mv(raw, &cal) * DIVIDER_NUM / DIVIDER_DEN)
}

/// Battery voltage in millivolts, refreshed at most every few seconds.
/// `None` when the chip has no ADC calibration or the ADC does not answer.
/// Call from threads only.
pub fn millivolts() -> Option<u32> {
    let mut s = STATE.lock();
    let now = crate::time::ticks();
    if s.at == 0 || now.wrapping_sub(s.at) >= CACHE_MS {
        s.mv = measure(&mut s);
        s.at = now.max(1);
    }
    s.mv
}

/// Last raw code and the calibration in use (diagnostics).
pub fn diagnostics() -> (u32, Option<Calibration>) {
    (LAST_RAW.load(Ordering::Relaxed), STATE.lock().cal)
}
