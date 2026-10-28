//! Blocking synchronisation primitives for kernel threads.

use crate::{irq, sched};
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

/// Counting semaphore. Waiters are kept as a bitmask of thread ids and all
/// of them retry on release, which is simple and fair enough for a small
/// fixed thread table.
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
            let acquired = irq::free(|| unsafe {
                if *self.count.get() > 0 {
                    *self.count.get() -= 1;
                    true
                } else {
                    let id = sched::mark_current_blocked();
                    *self.waiters.get() |= 1 << id;
                    false
                }
            });
            if acquired {
                return;
            }
            sched::yield_now();
        }
    }

    /// Safe to call from interrupt handlers.
    pub fn release(&self) {
        let woken = irq::free(|| unsafe {
            *self.count.get() += 1;
            core::mem::take(&mut *self.waiters.get())
        });
        for id in 0..32 {
            if woken & (1 << id) != 0 {
                sched::unblock(id);
            }
        }
        if woken != 0 {
            sched::request_resched();
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
