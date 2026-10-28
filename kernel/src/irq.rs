//! Interrupt routing: peripheral source -> interrupt matrix -> CLIC line.
//!
//! ESP32-P4 v1.x implements a pre-standard CLIC. Lines 0..15 are reserved for
//! the CPU, so peripherals use CLIC lines 16..47. The level threshold is a
//! memory-mapped register (not the `mintthresh` CSR), and only the top
//! NLBITS = 3 bits of each line's control byte encode the priority level.

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

struct HandlerTable(UnsafeCell<[Option<Handler>; LINE_COUNT]>);

// Written only with interrupts masked during setup, read from trap context.
unsafe impl Sync for HandlerTable {}

static HANDLERS: HandlerTable = HandlerTable(UnsafeCell::new([None; LINE_COUNT]));

fn ctrl(line: usize) -> usize {
    soc::CLIC_CTRL + line * 4
}

/// Route `source` to CLIC `line` on the calling core and enable it at
/// `level` (1..=7, higher preempts lower).
pub fn attach(source: usize, line: usize, level: u32, handler: Handler) {
    assert!((FIRST_EXTERNAL_LINE..LINE_COUNT).contains(&line));
    assert!((1..=7).contains(&level));
    let was_enabled = disable_global();
    unsafe { (*HANDLERS.0.get())[line] = Some(handler) };

    let matrix = if hart_id() == 0 { soc::INTR_MATRIX_CORE0 } else { soc::INTR_MATRIX_CORE1 };
    mmio::write(matrix + source * 4, line as u32);

    let ctl = (level << (8 - NLBITS)) | ((1 << (8 - NLBITS)) - 1);
    mmio::write(ctrl(line), (ctl << 24) | ATTR_MODE_MACHINE | ATTR_TRIG_LEVEL | IE);
    if was_enabled {
        enable_global();
    }
}

/// Accept every interrupt level above 0.
pub fn init() {
    mmio::write(CLIC_INT_THRESH, 0);
}

pub fn dispatch(line: usize) {
    let handler = unsafe { (*HANDLERS.0.get()).get(line).copied().flatten() };
    match handler {
        Some(h) => h(),
        None => {
            // Mask the stray line so it cannot storm.
            mmio::clear_bits(ctrl(line), IE | IP);
            crate::kprintln!("*** spurious interrupt on line {}", line);
        }
    }
}

pub fn hart_id() -> usize {
    let id: usize;
    unsafe { core::arch::asm!("csrr {0}, mhartid", out(reg) id) };
    id
}

pub fn enable_global() {
    unsafe { core::arch::asm!("csrsi mstatus, 0x8") };
}

/// Mask interrupts and report whether they were enabled before.
pub fn disable_global() -> bool {
    let prev: u32;
    unsafe { core::arch::asm!("csrrci {0}, mstatus, 0x8", out(reg) prev) };
    prev & 0x8 != 0
}

/// Run `f` with interrupts masked on this core.
pub fn free<R>(f: impl FnOnce() -> R) -> R {
    let was_enabled = disable_global();
    let r = f();
    if was_enabled {
        enable_global();
    }
    r
}
