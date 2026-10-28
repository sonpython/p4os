//! Peripheral base addresses for ESP32-P4 rev v1.x (from ESP-IDF v5.5 `reg_base.h`).

pub const HPPERIPH1: usize = 0x500C_0000;
pub const LPAON: usize = 0x5011_0000;
pub const LPPERIPH: usize = 0x5012_0000;

pub const TIMG0: usize = HPPERIPH1 + 0x2000;
pub const TIMG1: usize = HPPERIPH1 + 0x3000;
pub const USB_SERIAL_JTAG: usize = HPPERIPH1 + 0x12000;
pub const SYSTIMER: usize = HPPERIPH1 + 0x22000;
pub const LP_WDT: usize = LPAON + 0x6000;
pub const EFUSE: usize = LPPERIPH + 0xD000;
pub const HP_SYS_CLKRST: usize = HPPERIPH1 + 0x26000;
pub const LP_CLKRST: usize = LPAON + 0x1000;
pub const INTR_MATRIX_CORE0: usize = HPPERIPH1 + 0x16000;
pub const INTR_MATRIX_CORE1: usize = INTR_MATRIX_CORE0 + 0x800;
pub const CLIC: usize = 0x2080_0000;
pub const CLIC_CTRL: usize = 0x2080_1000;

/// Peripheral interrupt sources (index into the interrupt matrix).
pub mod source {
    pub const USB_SERIAL_JTAG: usize = 22;
    pub const SYSTIMER_TARGET0: usize = 53;
    pub const DW_GDMA: usize = 24;
    pub const AHB_PDMA_OUT_CH0: usize = 59;
}
pub const GPIO: usize = HPPERIPH1 + 0x20000;
pub const IO_MUX: usize = HPPERIPH1 + 0x21000;
pub const PMU: usize = LPAON + 0x5000;
pub const DSI_HOST: usize = 0x500A_0000;
pub const DSI_BRG: usize = 0x500A_0800;
pub const DSI_BRG_FIFO: usize = 0x5010_5000;
pub const DW_GDMA: usize = 0x5008_1000;
