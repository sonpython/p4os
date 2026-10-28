//! Blocking synchronisation primitives for kernel threads.

use crate::sched;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

/// Counting semaphore. Waiters are kept as a bitmask of thread ids; each
/// release wakes the most important waiter only, which then retries (and
/// re-registers if another thread took the count first).
///
/// `count` and `waiters` are only touched inside `sched::block_unless` and
/// `sched::wake_one`, i.e. under the scheduler lock, which serialises both
/// cores and interrupt handlers and makes "check, then block" atomic.
pub struct Semaphore {
    count: UnsafeCell<u32>,
    waiters: UnsafeCell<u32>,
}

unsafe impl Sync for Semaphore {}

const _: () = assert!(sched::MAX_THREADS <= 32);

impl Semaphore {
    pub const fn new(count: u32) -> Self {
        Self { count: UnsafeCell::new(count), waiters: UnsafeCell::new(0) }
    }

    pub fn acquire(&self) {
        loop {
            let acquired = sched::block_unless(
                || unsafe {
                    let count = &mut *self.count.get();
                    *count > 0 && {
                        *count -= 1;
                        true
                    }
                },
                |id| unsafe { *self.waiters.get() |= 1 << id },
            );
            if acquired {
                return;
            }
            sched::yield_now();
        }
    }

    /// Safe to call from interrupt handlers.
    pub fn release(&self) {
        sched::wake_one(|| unsafe {
            *self.count.get() += 1;
            &mut *self.waiters.get()
        });
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
