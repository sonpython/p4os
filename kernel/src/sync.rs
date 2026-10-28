//! Blocking synchronisation primitives for kernel threads.

use crate::sched;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

/// Counting semaphore. Taking an available count and releasing without
/// waiters are lock-free; only a thread that has to wait, and a release
/// that finds one, enter the scheduler. Waiters are kept as a bitmask of
/// thread ids; each release wakes the most important waiter only, which
/// then retries (and re-registers if another thread took the count first).
///
/// No wake-up is lost: a waiter publishes its bit before its last look at
/// `count`, and `release` adds to `count` before looking at `waiters`
/// (all sequentially consistent), so at least one of them sees the other.
pub struct Semaphore {
    count: AtomicU32,
    /// Set by the waiting thread and cleared by `sched::wake_one`, both
    /// under the scheduler lock.
    waiters: AtomicU32,
}

const _: () = assert!(sched::MAX_THREADS <= 32);

impl Semaphore {
    pub const fn new(count: u32) -> Self {
        Self { count: AtomicU32::new(count), waiters: AtomicU32::new(0) }
    }

    fn try_take(&self) -> bool {
        let mut count = self.count.load(Ordering::SeqCst);
        while count > 0 {
            match self.count.compare_exchange_weak(count, count - 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(now) => count = now,
            }
        }
        false
    }

    pub fn acquire(&self) {
        while !self.try_take() {
            let taken = sched::block_unless(|id| {
                self.waiters.fetch_or(1 << id, Ordering::SeqCst);
                let taken = self.try_take();
                if taken {
                    self.waiters.fetch_and(!(1 << id), Ordering::Relaxed);
                }
                taken
            });
            if taken {
                return;
            }
            sched::yield_now();
        }
    }

    /// Safe to call from kernel-level interrupt handlers (not fast ones,
    /// see `irq`; they signal threads with `sched::notify`).
    pub fn release(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.waiters.load(Ordering::SeqCst) != 0 {
            sched::wake_one(&self.waiters);
        }
    }
}

pub struct Mutex<T> {
    lock: Semaphore,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { lock: Semaphore::new(1), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.lock.acquire();
        MutexGuard { mutex: self }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.lock.release();
    }
}
