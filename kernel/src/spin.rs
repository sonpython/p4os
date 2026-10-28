//! Spinlock for state shared between the two HP cores and their interrupt
//! handlers.
//!
//! Taking the lock masks interrupts on the calling core first, so a handler
//! on the same core can never spin on a lock its own thread holds, then
//! spins on an atomic owner word (LR/SC) until the other core lets go.
//! Hold times must stay short: the other core busy-waits meanwhile.
//!
//! Locks must live in internal SRAM (statics): atomics are not guaranteed on
//! PSRAM.

use crate::irq;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct SpinLock<T> {
    /// 0 when free, otherwise the owning hart id + 1.
    owner: AtomicUsize,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { owner: AtomicUsize::new(0), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> SpinGuard<'_, T> {
        let irq_was_enabled = irq::disable_global();
        let me = irq::hart_id() + 1;
        while self.owner.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed).is_err() {
            loop {
                match self.owner.load(Ordering::Relaxed) {
                    0 => break,
                    // Only an exception taken inside the critical section can
                    // get here; spinning would hang this core forever.
                    o if o == me => panic!("spinlock re-entered on hart {}", me - 1),
                    _ => core::hint::spin_loop(),
                }
            }
        }
        SpinGuard { lock: self, irq_was_enabled }
    }

    /// Like `lock`, but give up after `us` microseconds. For diagnostics that
    /// must not hang when another core is stuck holding the lock.
    pub fn try_lock_for(&self, us: u64) -> Option<SpinGuard<'_, T>> {
        let irq_was_enabled = irq::disable_global();
        let me = irq::hart_id() + 1;
        let deadline = crate::systimer::now_ticks() + us * crate::systimer::TICKS_PER_US;
        loop {
            if self.owner.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return Some(SpinGuard { lock: self, irq_was_enabled });
            }
            if self.owner.load(Ordering::Relaxed) == me || crate::systimer::now_ticks() > deadline {
                if irq_was_enabled {
                    irq::enable_global();
                }
                return None;
            }
            core::hint::spin_loop();
        }
    }

    /// Hart currently holding the lock, if any (diagnostics only).
    pub fn owner(&self) -> Option<usize> {
        self.owner.load(Ordering::Relaxed).checked_sub(1)
    }
}

pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    irq_was_enabled: bool,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.owner.store(0, Ordering::Release);
        if self.irq_was_enabled {
            irq::enable_global();
        }
    }
}
