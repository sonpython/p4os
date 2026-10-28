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
//!
//! Flash writes suspend the cache both cores fetch code through, so the
//! other core is first parked in a loop in internal SRAM
//! (`with_other_core_parked`), as ESP-IDF does with its IPC "stall other
//! CPU" call.

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
/// Highest CLIC line, kept clear of the driver lines allocated from 16 up.
const IPI_LINE: usize = 47;
const IPI_LEVEL: u32 = 1;
// The handler takes the scheduler lock.
const _: () = assert!(IPI_LEVEL <= irq::KERNEL_LEVEL);

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

/// The sender sets the target's reschedule (or park, or deferred wake-up)
/// flag before raising the line; clearing the line before reading the
/// flags means a request arriving in between is either seen now or raises
/// the line again. The IPI is a kernel-level interrupt, so a core also
/// sends one to itself to run scheduler work a fast handler could not.
fn on_ipi() {
    mmio::write(CPU_INT_FROM_CPU + irq::hart_id() * 4, 0);
    unsafe { asm!("fence iorw, iorw") };
    if PARK_REQUEST.load(Ordering::Acquire) != 0 {
        park();
    }
    sched::wake_deferred();
}

/// Non-zero while a core wants the other one parked: the request number.
static PARK_REQUEST: AtomicU32 = AtomicU32::new(0);
/// Request number the parked core is spinning for, 0 when not parked.
static PARKED: AtomicU32 = AtomicU32::new(0);
static PARK_SEQ: AtomicU32 = AtomicU32::new(0);

/// Spin until the park request is withdrawn. Runs from internal SRAM in
/// trap context (interrupt stack in SRAM, interrupts masked) and touches
/// only flags in `.bss`, so nothing is fetched through the flash or PSRAM
/// cache meanwhile. Re-check the disassembly
/// (`objdump -d --section=.sram_text`) after changing it.
#[unsafe(link_section = ".sram_text")]
#[inline(never)]
fn park() {
    let id = PARK_REQUEST.load(Ordering::Acquire);
    if id == 0 {
        return;
    }
    PARKED.store(id, Ordering::Release);
    while PARK_REQUEST.load(Ordering::Acquire) == id {}
    PARKED.store(0, Ordering::Release);
}

/// The other core did not reach its parking loop in time.
#[derive(Debug)]
pub struct OtherCoreBusy;

/// Run `f` with interrupts masked on this core while the other core (if
/// running) spins in SRAM, so `f` may suspend the flash/PSRAM cache.
/// `f` itself must not touch flash or PSRAM once it has done so.
pub fn with_other_core_parked<R>(f: impl FnOnce() -> R) -> Result<R, OtherCoreBusy> {
    let irq_was_enabled = irq::disable_global();
    let other = 1 - irq::hart_id();
    let park_other = sched::is_online(other);
    // Numbered so a late acknowledgement of an abandoned request is never
    // mistaken for the current one.
    let id = PARK_SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1).max(1);
    if park_other {
        PARK_REQUEST.store(id, Ordering::SeqCst);
        send_ipi(other);
        // The other core takes the IPI as soon as it leaves any critical
        // section; spinlock holders never wait on this core here.
        if !wait_until(500_000, || PARKED.load(Ordering::Acquire) == id) {
            PARK_REQUEST.store(0, Ordering::Release);
            if irq_was_enabled {
                irq::enable_global();
            }
            return Err(OtherCoreBusy);
        }
    }
    let r = f();
    if park_other {
        PARK_REQUEST.store(0, Ordering::Release);
        while PARKED.load(Ordering::Acquire) == id {}
    }
    if irq_was_enabled {
        irq::enable_global();
    }
    Ok(r)
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
    crate::init_fault_handling();
    sched::start()
}
