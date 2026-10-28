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
//!
//! Interrupt levels come in two classes:
//!
//! - **Kernel level** (1..=`KERNEL_LEVEL`): the handler may use the kernel
//!   services meant for interrupt handlers (spinlocks, `Semaphore::release`,
//!   `sched::notify`). Kernel critical sections (`spin::SpinLock`) mask
//!   these levels on the calling core by raising the CLIC threshold.
//! - **Fast** (above `KERNEL_LEVEL`): never masked by kernel critical
//!   sections, so their latency does not depend on how long a thread holds
//!   a lock. In exchange the handler must not take any lock (no spinlock,
//!   semaphore, mutex, allocation or `mmio::set_bits`/`modify`): it touches
//!   only registers and atomics and signals threads with `sched::notify`,
//!   which defers the scheduler work to a kernel-level interrupt when the
//!   interrupted code may hold the scheduler lock. Display page flips
//!   (level 6) and audio DMA refills (level 5) are fast.
//!
//! Traps do not nest, so a fast interrupt still waits for a handler that is
//! already running; it only stops waiting for thread-side critical sections.

use crate::{mmio, soc};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

pub const FIRST_EXTERNAL_LINE: usize = 16;
pub const LINE_COUNT: usize = 48;
const NLBITS: u32 = 3;

/// Highest level whose handlers may take kernel locks (see the module
/// comment). Levels above it are fast interrupts.
pub const KERNEL_LEVEL: u32 = 3;

/// Threshold register (pre-standard CLIC of the P4 v1, ESP-IDF
/// `CLIC_INT_THRESH_REG`): an interrupt is taken only if its level byte is
/// above the threshold byte in bits 31:24.
const CLIC_INT_THRESH: usize = soc::CLIC + 0x8;
const THRESH_SHIFT: u32 = 24;
/// Threshold that masks every level up to and including `level`.
const fn threshold(level: u32) -> u32 {
    ((level << (8 - NLBITS)) | ((1 << (8 - NLBITS)) - 1)) << THRESH_SHIFT
}
const KERNEL_THRESH: u32 = threshold(KERNEL_LEVEL);
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
    let slot = unsafe { &mut (*HANDLERS.0.get())[hart][line] };
    // Re-attaching the same handler (e.g. to change its level) is fine;
    // silently replacing another driver's handler is not.
    assert!(
        slot.is_none_or(|h| core::ptr::fn_addr_eq(h, handler)),
        "CLIC line {} on hart {} already has another handler",
        line,
        hart
    );
    *slot = Some(handler);

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
            // Mask the stray line so it cannot storm. The register belongs
            // to this core's CLIC, so no cross-core lock is needed (and none
            // may be taken: the line may be a fast one).
            mmio::write(ctrl(line), mmio::read(ctrl(line)) & !(IE | IP));
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

/// Whether every interrupt level is enabled on this core, i.e. the caller
/// runs in thread context outside any critical section and may block.
pub fn enabled() -> bool {
    mie() && !kernel_masked()
}

/// Whether kernel-level interrupts are masked by the threshold on this
/// core. Interrupt handlers do not change the threshold, so in a handler
/// this describes the interrupted code: true means it may hold a kernel
/// lock, and the handler must not take one.
#[inline(always)]
pub fn kernel_masked() -> bool {
    mmio::read(CLIC_INT_THRESH) >= KERNEL_THRESH
}

/// Whether `mask_kernel` raised the threshold, i.e. `unmask_kernel` has
/// work to do.
#[derive(Clone, Copy)]
#[must_use]
pub struct KernelMask(bool);

/// Nesting depth of kernel critical sections in thread context, per core.
/// Only its own core changes an entry, with kernel-level interrupts masked.
static KERNEL_MASK_DEPTH: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];

/// Mask kernel-level interrupts on this core, leaving fast ones enabled.
/// Nests. Does nothing where every interrupt is already masked (trap
/// handlers, `free`, boot), so trap paths pay no register access.
///
/// Thread context runs at threshold 0 outside critical sections (threads
/// are only switched while kernel-level interrupts are unmasked, see
/// `sched::take_resched`), so the outermost unmask restores 0 and no old
/// value has to be read. The threshold is raised before the core id is
/// read: from then on the thread cannot migrate. Reading the register back
/// makes the new threshold effective before the caller enters its critical
/// section (ESP-IDF `rv_utils_restore_intlevel_regval`).
#[inline(always)]
pub fn mask_kernel() -> KernelMask {
    if !mie() {
        return KernelMask(false);
    }
    mmio::write(CLIC_INT_THRESH, KERNEL_THRESH);
    mmio::read(CLIC_INT_THRESH);
    let depth = &KERNEL_MASK_DEPTH[hart_id()];
    depth.store(depth.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    KernelMask(true)
}

/// Undo the matching `mask_kernel`.
#[inline(always)]
pub fn unmask_kernel(mask: KernelMask) {
    if !mask.0 {
        return;
    }
    let depth = &KERNEL_MASK_DEPTH[hart_id()];
    let left = depth.load(Ordering::Relaxed) - 1;
    depth.store(left, Ordering::Relaxed);
    if left == 0 {
        mmio::write(CLIC_INT_THRESH, 0);
    }
}

fn mie() -> bool {
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

/// Run `f` with every interrupt, fast ones included, masked on this core.
/// Meant for short device sequences; kernel state uses `spin::SpinLock`,
/// which leaves fast interrupts running. This does not exclude the other
/// core.
pub fn free<R>(f: impl FnOnce() -> R) -> R {
    let was_enabled = disable_global();
    let r = f();
    if was_enabled {
        enable_global();
    }
    r
}
