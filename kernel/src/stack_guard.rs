//! Hardware stack guard: the assist_debug SP monitor of each HP core
//! (ESP-IDF `CONFIG_ESP_SYSTEM_HW_STACK_GUARD`, `hw_stack_guard.c`).
//!
//! The monitor compares the core's `sp` with a lower bound on every cycle
//! and raises the ASSIST_DEBUG interrupt as soon as it drops below. It is
//! programmed with the running thread's stack on every switch, so an
//! overflow is caught when it happens instead of when the canary is next
//! checked. No PMP entry is used.
//!
//! - The bound sits `RESERVE` bytes above the stack bottom. The trap entry
//!   pushes its 144-byte frame onto the thread stack while the monitor is
//!   still on, so a thread whose usage plus one trap frame reaches into the
//!   reserve is reported as overflowing. The reserve also gives the trap
//!   taken for the overflow somewhere to store its frame.
//! - Trap handlers run on the per-core interrupt stack, outside the
//!   thread's bounds: the trap entry in `boot.rs` turns the monitor off and
//!   the exit writes `STACK_GUARD_ENA[hart]` back (0 until the core runs a
//!   thread). It stays off while a user app runs in U-mode on its own
//!   stack; the app's kernel stack (syscalls) is guarded like any other.
//! - The interrupt is masked like any other (`mstatus.MIE`), so an overflow
//!   inside a critical section is reported when it ends. Both cores' monitors
//!   feed one interrupt source; `fired_here` tells whose it is from the
//!   trapped frame and the per-core recorded PC.

use crate::irq::{self, HART_COUNT};
use crate::{mmio, soc};
use core::sync::atomic::{AtomicU32, Ordering};

/// DR_REG_ASSIST_DEBUG_BASE (the CPU bus monitor in the HP CPU peripheral
/// space). Duplicated as an immediate in `boot.rs`.
pub const BASE: usize = 0x3FF0_6000;
/// Core 1's registers follow core 0's at this offset. `boot.rs` uses `<< 7`.
const CORE_STRIDE: usize = 0x80;
const _: () = assert!(BASE == 0x3FF0_6000 && CORE_STRIDE == 1 << 7, "update boot.rs");
const INTR_ENA: usize = 0x00;
const INTR_RLS: usize = 0x08;
const INTR_CLR: usize = 0x0C;
const SP_MIN: usize = 0x38;
const SP_MAX: usize = 0x3C;
const SP_PC: usize = 0x40;
const RCD_EN: usize = 0x44;
const CLOCK_GATE: usize = BASE + 0x108;

const SP_SPILL_MIN: u32 = 1 << 8;
const SP_SPILL_MAX: u32 = 1 << 9;
const RCD_RECORDEN: u32 = 1 << 0;
const RCD_PDEBUGEN: u32 = 1 << 1;
const CLK_EN: u32 = 1 << 0;

const CLKRST_SOC_CLK_CTRL0: usize = soc::HP_SYS_CLKRST + 0x14;
const BUSMON_CPU_CLK_EN: u32 = 1 << 6;

/// CLIC line for the ASSIST_DEBUG source on each core, at the highest
/// level so a pending overflow is served before anything else.
pub const LINE: usize = 45;
const LEVEL: u32 = 7;
const CLIC_IE: u32 = 1 << 8;

/// Bytes at the bottom of every thread stack below the monitored bound.
pub const RESERVE: usize = 256;

/// Monitor enable bits the trap exit path writes for each core.
#[unsafe(no_mangle)]
pub static STACK_GUARD_ENA: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];
/// Lower bound currently programmed on each core.
static LIMIT: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];
/// SP_PC as found after reset: a different value means this core's
/// monitor has fired since.
static PC_AT_INIT: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];

fn reg(hart: usize, offset: usize) -> usize {
    BASE + hart * CORE_STRIDE + offset
}

/// Clock the monitor. Must run on core 0 before the first trap, because
/// the trap entry writes to the (otherwise gated) monitor registers.
pub fn init_clock() {
    mmio::set_bits(CLKRST_SOC_CLK_CTRL0, BUSMON_CPU_CLK_EN);
    mmio::write(CLOCK_GATE, mmio::read(CLOCK_GATE) | CLK_EN);
}

/// Reset the calling core's monitor and route its interrupt. The monitor
/// stays off until the core switches to a thread.
pub fn init_core() {
    let hart = irq::hart_id();
    STACK_GUARD_ENA[hart].store(0, Ordering::Relaxed);
    mmio::write(reg(hart, INTR_ENA), 0);
    mmio::write(reg(hart, INTR_CLR), SP_SPILL_MIN | SP_SPILL_MAX);
    mmio::write(reg(hart, SP_MIN), 0);
    mmio::write(reg(hart, SP_MAX), u32::MAX);
    mmio::write(reg(hart, RCD_EN), RCD_RECORDEN | RCD_PDEBUGEN);
    // "RLS" is the interrupt enable; "ENA" arms the comparison.
    mmio::write(reg(hart, INTR_RLS), SP_SPILL_MIN);
    PC_AT_INIT[hart].store(mmio::read(reg(hart, SP_PC)), Ordering::Relaxed);
    irq::attach(soc::source::ASSIST_DEBUG, LINE, LEVEL, on_irq);
}

/// Program the bound for the thread about to run on `hart`. Called from
/// the scheduler with the monitor off (in a trap); the trap exit arms it.
/// Only the lower bound is armed, so SP_MAX keeps its value from init.
pub fn set_thread(hart: usize, bottom: usize) {
    let limit = (bottom + RESERVE) as u32;
    mmio::write(reg(hart, SP_MIN), limit);
    LIMIT[hart].store(limit, Ordering::Relaxed);
    STACK_GUARD_ENA[hart].store(SP_SPILL_MIN, Ordering::Relaxed);
}

/// Bound programmed on `hart` (0 when the core runs no thread yet).
pub fn limit(hart: usize) -> u32 {
    LIMIT[hart].load(Ordering::Relaxed)
}

/// Whether the interrupted thread overflowed, judged at trap entry.
/// `frame` is where the trap saved the interrupted context: below the
/// bound means the thread (or the frame push itself) crossed it. That
/// check is plain arithmetic, so ordinary traps pay no register read. The
/// guard interrupt itself (`mcause`) can also arrive after the thread came
/// back up, when it crossed with interrupts masked: then a changed recorded
/// PC says it was this core's monitor, not the other one's.
pub fn fired_here(frame: usize, mcause: u32) -> bool {
    let hart = irq::hart_id();
    let limit = LIMIT[hart].load(Ordering::Relaxed);
    if limit != 0 && frame < limit as usize {
        return true;
    }
    mcause & (1 << 31) != 0
        && (mcause & 0xFFF) as usize == LINE
        && mmio::read(reg(hart, SP_PC)) != PC_AT_INIT[hart].load(Ordering::Relaxed)
}

/// PC recorded when `hart`'s monitor fired.
pub fn fired_pc(hart: usize) -> u32 {
    mmio::read(reg(hart, SP_PC))
}

/// Reached only when `fired_here` said the event is the other core's:
/// stop taking it here. The other core reports it on its next trap and
/// the crash path resets the chip, so the line is never needed again.
fn on_irq() {
    let ctrl = soc::CLIC_CTRL + LINE * 4;
    mmio::write(ctrl, mmio::read(ctrl) & !CLIC_IE);
}
