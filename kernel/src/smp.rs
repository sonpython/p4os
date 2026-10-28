//! Second HP core bring-up and inter-processor interrupts.
//!
//! After the 2nd-stage bootloader, core 1 is held in reset (or parked in
//! ROM). Releasing it follows ESP-IDF v5.5 `start_other_core` for the P4:
//! clear its PMU software stall, enable its clock and drop its reset in
//! HP_SYS_CLKRST, then give the ROM an entry point with
//! `ets_set_appcpu_boot_addr`. The ROM jumps there in M-mode with
//! interrupts off; `boot.rs` `_start_hart1` sets up a stack, the FPU and
//! the trap vector and calls `kmain_hart1`. Until then the ROM runs core 1
//! on its own stack just below 0x4FF3EFC0 (sp 0x4FF3EFA0 measured at the
//! hand-over), inside the ROM-reserved SRAM above `_heap_end`.
//!
//! IPIs use the CPU-from-CPU interrupt sources: core N listens on source
//! FROM_CPU0 + N, raised by writing 1 to HP_SYS CPU_INT_FROM_CPU_N. The
//! level stays asserted until the receiver writes 0.

use crate::{irq, kprintln, mmio, sched, soc, systimer};
use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};

const PMU_CPU_SW_STALL: usize = soc::PMU + 0x200;
const HPCORE1_STALL_CODE_SHIFT: u32 = 16;
const STALL_CODE_RUN: u32 = 0xFF;
const HP_SYS_CORESTALLED_ST: usize = soc::HP_SYS + 0x64;
const CORE1_STALLED: u32 = 1 << 1;
const CLKRST_SOC_CLK_CTRL0: usize = soc::HP_SYS_CLKRST + 0x14;
const CORE1_CPU_CLK_EN: u32 = 1 << 4;
const CLKRST_HP_RST_EN0: usize = soc::HP_SYS_CLKRST + 0xC0;
const RST_EN_CORE1_GLOBAL: u32 = 1 << 8;
/// `ets_set_appcpu_boot_addr` (esp_rom/esp32p4/ld/esp32p4.rom.ld).
const ROM_SET_APPCPU_BOOT_ADDR: usize = 0x4FC0_00A8;

const CPU_INT_FROM_CPU: usize = soc::HP_SYS + 0x10;
const CLIC_INT_CONFIG: usize = soc::CLIC;
const IPI_LINE: usize = 20;
const IPI_LEVEL: u32 = 1;

/// Core 0 settings core 1 copies so both cores behave the same: the custom
/// MHCR CSR (branch prediction and related features) and the CLIC config.
static CORE0_MHCR: AtomicU32 = AtomicU32::new(0);
static CORE0_CLIC_CONFIG: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" {
    fn _start_hart1();
}

fn read_mhcr() -> u32 {
    let v: u32;
    unsafe { asm!("csrr {0}, 0x7c1", out(reg) v) };
    v
}

fn rom_set_appcpu_boot_addr(addr: u32) {
    let f: extern "C" fn(u32) = unsafe { core::mem::transmute(ROM_SET_APPCPU_BOOT_ADDR) };
    f(addr);
}

fn wait_until(us: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = systimer::now_ticks() + us * systimer::TICKS_PER_US;
    while !done() {
        if systimer::now_ticks() > deadline {
            return false;
        }
    }
    true
}

/// Listen for IPIs on the calling core.
pub fn init_ipi() {
    let hart = irq::hart_id();
    mmio::write(CPU_INT_FROM_CPU + hart * 4, 0);
    irq::attach(soc::source::FROM_CPU0 + hart, IPI_LINE, IPI_LEVEL, on_ipi);
}

/// The sender sets the target's reschedule flag before raising the line;
/// clearing the line before the trap path reads that flag means a request
/// arriving in between is either seen now or raises the line again.
fn on_ipi() {
    mmio::write(CPU_INT_FROM_CPU + irq::hart_id() * 4, 0);
    unsafe { asm!("fence iorw, iorw") };
}

/// Interrupt core `hart`, which reschedules on the way out of the trap.
pub fn send_ipi(hart: usize) {
    // Order the caller's memory writes before the device write.
    unsafe { asm!("fence iorw, iorw") };
    mmio::write(CPU_INT_FROM_CPU + hart * 4, 1);
}

/// Release core 1 from ROM and wait until it has entered the scheduler.
pub fn start_core1() {
    CORE0_MHCR.store(read_mhcr(), Ordering::Relaxed);
    CORE0_CLIC_CONFIG.store(mmio::read(CLIC_INT_CONFIG), Ordering::Relaxed);

    let stall = mmio::read(PMU_CPU_SW_STALL);
    mmio::write(
        PMU_CPU_SW_STALL,
        (stall & !(0xFF << HPCORE1_STALL_CODE_SHIFT)) | (STALL_CODE_RUN << HPCORE1_STALL_CODE_SHIFT),
    );
    if !wait_until(10_000, || mmio::read(HP_SYS_CORESTALLED_ST) & CORE1_STALLED == 0) {
        kprintln!("smp: hart 1 stays stalled");
        return;
    }
    let clk = mmio::read(CLKRST_SOC_CLK_CTRL0);
    let rst = mmio::read(CLKRST_HP_RST_EN0);
    // Leave the core alone if a debugger already started it.
    if clk & CORE1_CPU_CLK_EN == 0 {
        mmio::set_bits(CLKRST_SOC_CLK_CTRL0, CORE1_CPU_CLK_EN);
    }
    if rst & RST_EN_CORE1_GLOBAL != 0 {
        mmio::clear_bits(CLKRST_HP_RST_EN0, RST_EN_CORE1_GLOBAL);
    }
    rom_set_appcpu_boot_addr(_start_hart1 as *const () as u32);

    if !wait_until(200_000, || sched::is_online(1)) {
        kprintln!("smp: hart 1 did not come up (clk_en {}, in reset {})", clk & CORE1_CPU_CLK_EN != 0, rst & RST_EN_CORE1_GLOBAL != 0);
        return;
    }
    kprintln!("smp: hart 1 online");
}

#[unsafe(no_mangle)]
extern "C" fn kmain_hart1() -> ! {
    // Do not jump to the kernel again if core 1 is ever reset alone.
    rom_set_appcpu_boot_addr(0);
    let mhcr = CORE0_MHCR.load(Ordering::Relaxed);
    unsafe { asm!("csrw 0x7c1, {0}", in(reg) mhcr) };
    mmio::write(CLIC_INT_CONFIG, CORE0_CLIC_CONFIG.load(Ordering::Relaxed));
    irq::clear_matrix();
    irq::init();
    init_ipi();
    sched::start()
}
