//! Interrupt routing: peripheral source -> interrupt matrix -> CLIC line.
//!
//! ESP32-P4 v1.x implements a pre-standard CLIC. Lines 0..15 are reserved for
//! the CPU, so peripherals use CLIC lines 16..47. The level threshold is a
//! memory-mapped register (not the `mintthresh` CSR), and only the top
//! NLBITS = 3 bits of each line's control byte encode the priority level.
//!
//! Each HP core has its own CLIC, seen at the same addresses from both
//! cores, and its own interrupt matrix map. A source is therefore always
//! attached on the core that calls `attach`, and each core keeps its own
//! row of the handler table. A row is written only by its own core with
//! interrupts masked and read only by that core's trap handler, so no lock
//! is needed across cores.

use crate::{mmio, soc};
use core::cell::UnsafeCell;

pub const FIRST_EXTERNAL_LINE: usize = 16;
pub const LINE_COUNT: usize = 48;
const NLBITS: u32 = 3;

const CLIC_INT_THRESH: usize = soc::CLIC + 0x8;
const ATTR_MODE_MACHINE: u32 = 3 << 22;
const ATTR_TRIG_LEVEL: u32 = 0 << 17;
const IE: u32 = 1 << 8;
const IP: u32 = 1 << 0;

pub type Handler = fn();

pub const HART_COUNT: usize = 2;

struct HandlerTable(UnsafeCell<[[Option<Handler>; LINE_COUNT]; HART_COUNT]>);

// Row `h` is only touched by hart `h` (see the module comment).
unsafe impl Sync for HandlerTable {}

static HANDLERS: HandlerTable = HandlerTable(UnsafeCell::new([[None; LINE_COUNT]; HART_COUNT]));

fn ctrl(line: usize) -> usize {
    soc::CLIC_CTRL + line * 4
}

/// Route `source` to CLIC `line` on the calling core and enable it at
/// `level` (1..=7, higher preempts lower).
pub fn attach(source: usize, line: usize, level: u32, handler: Handler) {
    assert!((FIRST_EXTERNAL_LINE..LINE_COUNT).contains(&line));
    assert!((1..=7).contains(&level));
    let was_enabled = disable_global();
    let hart = hart_id();
    unsafe { (*HANDLERS.0.get())[hart][line] = Some(handler) };

    mmio::write(matrix(hart) + source * 4, line as u32);

    let ctl = (level << (8 - NLBITS)) | ((1 << (8 - NLBITS)) - 1);
    mmio::write(ctrl(line), (ctl << 24) | ATTR_MODE_MACHINE | ATTR_TRIG_LEVEL | IE);
    if was_enabled {
        enable_global();
    }
}

fn matrix(hart: usize) -> usize {
    if hart == 0 { soc::INTR_MATRIX_CORE0 } else { soc::INTR_MATRIX_CORE1 }
}

/// Accept every interrupt level above 0 on the calling core.
pub fn init() {
    mmio::write(CLIC_INT_THRESH, 0);
}

/// Unroute every source from the calling core. A core released from ROM
/// may carry stale matrix entries; line 0 means "not connected".
pub fn clear_matrix() {
    let base = matrix(hart_id());
    for source in 0..soc::source::COUNT {
        mmio::write(base + source * 4, 0);
    }
}

pub fn dispatch(line: usize) {
    let handler = unsafe { (*HANDLERS.0.get())[hart_id()].get(line).copied().flatten() };
    match handler {
        Some(h) => h(),
        None => {
            // Mask the stray line so it cannot storm.
            mmio::clear_bits(ctrl(line), IE | IP);
            crate::kprintln!("*** spurious interrupt on line {} (hart {})", line, hart_id());
        }
    }
}

pub fn hart_id() -> usize {
    let id: usize;
    unsafe { core::arch::asm!("csrr {0}, mhartid", out(reg) id) };
    id
}

unsafe extern "C" {
    static _irq_stack_bottom: u8;
    static _irq_stack_top: u8;
}

/// Whether the caller runs in a trap handler, i.e. on an interrupt stack.
pub fn in_trap() -> bool {
    let sp: usize;
    unsafe { core::arch::asm!("mv {0}, sp", out(reg) sp) };
    (&raw const _irq_stack_bottom as usize..&raw const _irq_stack_top as usize).contains(&sp)
}

pub fn enable_global() {
    unsafe { core::arch::asm!("csrsi mstatus, 0x8") };
}

/// Whether interrupts are enabled on this core (thread context outside
/// any critical section).
pub fn enabled() -> bool {
    let s: u32;
    unsafe { core::arch::asm!("csrr {0}, mstatus", out(reg) s) };
    s & 0x8 != 0
}

/// Mask interrupts and report whether they were enabled before.
pub fn disable_global() -> bool {
    let prev: u32;
    unsafe { core::arch::asm!("csrrci {0}, mstatus, 0x8", out(reg) prev) };
    prev & 0x8 != 0
}

/// Run `f` with interrupts masked on this core. This does not exclude the
/// other core; shared state needs a `spin::SpinLock`.
pub fn free<R>(f: impl FnOnce() -> R) -> R {
    let was_enabled = disable_global();
    let r = f();
    if was_enabled {
        enable_global();
    }
    r
}
