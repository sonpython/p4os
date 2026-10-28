//! Plain GPIO outputs and inputs through the IO MUX and GPIO matrix.

use crate::{mmio, soc};

const OUT_W1TS: usize = soc::GPIO + 0x08;
const OUT_W1TC: usize = soc::GPIO + 0x0C;
const OUT1_W1TS: usize = soc::GPIO + 0x14;
const OUT1_W1TC: usize = soc::GPIO + 0x18;
const ENABLE_W1TS: usize = soc::GPIO + 0x24;
const ENABLE1_W1TS: usize = soc::GPIO + 0x30;
const IN: usize = soc::GPIO + 0x3C;
const IN1: usize = soc::GPIO + 0x40;
const FUNC_OUT_SEL: usize = soc::GPIO + 0x558;
/// Output select value that connects the pad to the GPIO_OUT register.
const OUT_SEL_GPIO: u32 = 0x100;

const MCU_SEL_SHIFT: u32 = 12;
const FUN_IE: u32 = 1 << 9;
const FUN_WPU: u32 = 1 << 8;
const FUN_WPD: u32 = 1 << 7;
const MCU_SEL_GPIO: u32 = 1;

fn pad(pin: u32) -> usize {
    // IO MUX pad registers start at offset 4 (GPIO0).
    soc::IO_MUX + 4 + 4 * pin as usize
}

/// GPIO26/27 double as the internal USB OTG 1.1 PHY. While that PHY owns
/// the pads, GPIO writes have no effect, so release them first.
const USB_WRAP_OTG_CONF: usize = 0x5008_0000;
const USB_PAD_ENABLE: u32 = 1 << 18;

fn select_gpio_function(pin: u32) {
    if pin == 26 || pin == 27 {
        mmio::clear_bits(USB_WRAP_OTG_CONF, USB_PAD_ENABLE);
    }
    let v = mmio::read(pad(pin)) & !(0x7 << MCU_SEL_SHIFT);
    mmio::write(pad(pin), v | (MCU_SEL_GPIO << MCU_SEL_SHIFT));
}

pub fn output(pin: u32, high: bool) {
    select_gpio_function(pin);
    mmio::write(FUNC_OUT_SEL + 4 * pin as usize, OUT_SEL_GPIO);
    set(pin, high);
    if pin < 32 {
        mmio::write(ENABLE_W1TS, 1 << pin);
    } else {
        mmio::write(ENABLE1_W1TS, 1 << (pin - 32));
    }
}

pub fn set(pin: u32, high: bool) {
    let (s, c, bit) = if pin < 32 { (OUT_W1TS, OUT_W1TC, pin) } else { (OUT1_W1TS, OUT1_W1TC, pin - 32) };
    mmio::write(if high { s } else { c }, 1 << bit);
}

pub fn input(pin: u32, pull_up: bool) {
    select_gpio_function(pin);
    let v = mmio::read(pad(pin)) & !(FUN_WPU | FUN_WPD);
    mmio::write(pad(pin), v | FUN_IE | if pull_up { FUN_WPU } else { 0 });
}

pub fn read(pin: u32) -> bool {
    if pin < 32 { mmio::read(IN) & (1 << pin) != 0 } else { mmio::read(IN1) & (1 << (pin - 32)) != 0 }
}
