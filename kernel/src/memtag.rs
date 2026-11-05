//! Heap attribution by owner tag.
//!
//! Every thread carries an owner tag (0 = the system). The heap charges
//! each allocation to the tag of the thread that makes it and credits each
//! free to the tag of the thread that makes it, per pool (SRAM, PSRAM).
//! The desktop sets the GUI thread's tag around every call into an app, so
//! an app's live heap shows up under its tag.
//!
//! This costs two atomic operations per allocation and needs no per-block
//! header, so it is independent of the allocator's internals. The price is
//! that memory freed by a different owner than the one that allocated it
//! moves between counters: such a counter can go negative (it reads as
//! zero) or stay high. Known cross-owner frees are routed back to their
//! owner (the audio thread drops a finished source under the tag of the app
//! that queued it), and a tag's counters are cleared when its app is
//! destroyed.

use crate::{irq, sched};
use core::sync::atomic::{AtomicIsize, AtomicU8, Ordering};

pub const OWNERS: usize = 16;
pub const SYSTEM: u8 = 0;

/// Owner tag per scheduler thread id.
static TAG: [AtomicU8; sched::MAX_THREADS] = [const { AtomicU8::new(SYSTEM) }; sched::MAX_THREADS];
/// Live bytes per owner: [SRAM, PSRAM].
static USAGE: [[AtomicIsize; 2]; OWNERS] = [const { [const { AtomicIsize::new(0) }; 2] }; OWNERS];

/// Owner of the code running on this core right now. Trap handlers count
/// as the thread they interrupted; before the scheduler starts it is the
/// system.
#[inline]
pub fn current() -> u8 {
    // Masked so the thread cannot move to the other core between reading
    // the core id and that core's running thread.
    irq::free(|| match sched::running_id(irq::hart_id()) {
        Some(id) => TAG[id].load(Ordering::Relaxed),
        None => SYSTEM,
    })
}

/// Set the calling thread's tag; returns the previous one. Tags outside
/// `0..OWNERS` are clamped to the last one.
pub fn set_current(tag: u8) -> u8 {
    let tag = tag.min(OWNERS as u8 - 1);
    match sched::current_id() {
        Some(id) => TAG[id].swap(tag, Ordering::Relaxed),
        None => SYSTEM,
    }
}

/// A new thread starts as the system, whatever its slot held before.
pub fn reset_thread(id: usize) {
    if let Some(t) = TAG.get(id) {
        t.store(SYSTEM, Ordering::Relaxed);
    }
}

#[inline]
pub fn charge(psram: bool, size: usize) {
    USAGE[current() as usize][psram as usize].fetch_add(size as isize, Ordering::Relaxed);
}

#[inline]
pub fn credit(psram: bool, size: usize) {
    USAGE[current() as usize][psram as usize].fetch_sub(size as isize, Ordering::Relaxed);
}

/// Live (SRAM, PSRAM) bytes attributed to `tag`.
pub fn usage(tag: u8) -> (usize, usize) {
    let u = &USAGE[(tag as usize).min(OWNERS - 1)];
    let read = |c: &AtomicIsize| c.load(Ordering::Relaxed).max(0) as usize;
    (read(&u[0]), read(&u[1]))
}

/// Forget what is attributed to `tag` (its app is gone).
pub fn clear(tag: u8) {
    if tag == SYSTEM {
        return;
    }
    for c in &USAGE[(tag as usize).min(OWNERS - 1)] {
        c.store(0, Ordering::Relaxed);
    }
}
