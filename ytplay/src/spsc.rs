//! A bounded lock-free single-producer single-consumer queue.
//!
//! The player hands data between the GUI thread, the audio thread and the
//! decoder thread. A spinlock shared by threads of different priorities can
//! livelock on one core (the high-priority waiter never lets the holder
//! run), and the SDK offers no blocking mutex, so the hand-offs use this
//! queue instead: each end is owned by exactly one thread and never waits.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

struct Ring<T> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    /// Next slot to read; written only by the consumer.
    head: AtomicUsize,
    /// Next slot to write; written only by the producer.
    tail: AtomicUsize,
}

// SAFETY: a slot is accessed by one side at a time, handed over through the
// acquire/release pairs on `head` and `tail`.
unsafe impl<T: Send> Sync for Ring<T> {}
unsafe impl<T: Send> Send for Ring<T> {}

impl<T> Ring<T> {
    fn len(&self) -> usize {
        self.tail.load(Ordering::Acquire).wrapping_sub(self.head.load(Ordering::Acquire))
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        let (mut h, t) = (*self.head.get_mut(), *self.tail.get_mut());
        while h != t {
            // SAFETY: slots between head and tail hold initialised values.
            unsafe { self.slots[h % self.slots.len()].get_mut().assume_init_drop() };
            h = h.wrapping_add(1);
        }
    }
}

pub struct Producer<T> {
    ring: Arc<Ring<T>>,
}

pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
}

/// A queue holding at most `capacity` items.
pub fn channel<T: Send>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    assert!(capacity > 0);
    let slots = (0..capacity).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect();
    let ring = Arc::new(Ring { slots, head: AtomicUsize::new(0), tail: AtomicUsize::new(0) });
    (Producer { ring: ring.clone() }, Consumer { ring })
}

impl<T: Send> Producer<T> {
    /// Append `v`, or hand it back when the queue is full.
    pub fn push(&mut self, v: T) -> Result<(), T> {
        let r = &*self.ring;
        let tail = r.tail.load(Ordering::Relaxed);
        if tail.wrapping_sub(r.head.load(Ordering::Acquire)) == r.slots.len() {
            return Err(v);
        }
        // SAFETY: the slot at `tail` is free (checked above) and only the
        // producer writes free slots.
        unsafe { (*r.slots[tail % r.slots.len()].get()).write(v) };
        r.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.len() == self.ring.slots.len()
    }

    /// True once the consumer has been dropped.
    pub fn is_orphaned(&self) -> bool {
        Arc::strong_count(&self.ring) == 1
    }
}

impl<T: Send> Consumer<T> {
    pub fn pop(&mut self) -> Option<T> {
        let r = &*self.ring;
        let head = r.head.load(Ordering::Relaxed);
        if head == r.tail.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: the slot at `head` was published by the producer's release
        // store of `tail`; only the consumer reads it.
        let v = unsafe { (*r.slots[head % r.slots.len()].get()).assume_init_read() };
        r.head.store(head.wrapping_add(1), Ordering::Release);
        Some(v)
    }

    /// Look at the next item without taking it.
    pub fn peek(&self) -> Option<&T> {
        let r = &*self.ring;
        let head = r.head.load(Ordering::Relaxed);
        if head == r.tail.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: as in `pop`; the item stays in place until `pop`, which
        // needs `&mut self` and so cannot overlap this borrow.
        Some(unsafe { (*r.slots[head % r.slots.len()].get()).assume_init_ref() })
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True once the producer has been dropped.
    pub fn is_orphaned(&self) -> bool {
        Arc::strong_count(&self.ring) == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn preserves_order_and_capacity() {
        let (mut tx, mut rx) = channel(3);
        assert!(rx.pop().is_none());
        for i in 0..3 {
            tx.push(i).unwrap();
        }
        assert!(tx.is_full());
        assert_eq!(tx.push(9), Err(9));
        assert_eq!(rx.peek(), Some(&0));
        assert_eq!(rx.pop(), Some(0));
        tx.push(3).unwrap();
        let got: Vec<_> = core::iter::from_fn(|| rx.pop()).collect();
        assert_eq!(got, [1, 2, 3]);
        assert!(!rx.is_orphaned());
        drop(tx);
        assert!(rx.is_orphaned());
    }

    #[test]
    fn drops_items_left_in_the_queue() {
        let item = Arc::new(());
        let (mut tx, rx) = channel(4);
        tx.push(item.clone()).unwrap();
        tx.push(item.clone()).unwrap();
        assert_eq!(Arc::strong_count(&item), 3);
        drop((tx, rx));
        assert_eq!(Arc::strong_count(&item), 1);
    }

    #[test]
    fn hands_items_across_threads() {
        let (mut tx, mut rx) = channel::<u32>(8);
        let t = std::thread::spawn(move || {
            let mut i = 0;
            while i < 100_000 {
                if tx.push(i).is_ok() {
                    i += 1;
                }
            }
        });
        let mut next = 0;
        while next < 100_000 {
            if let Some(v) = rx.pop() {
                assert_eq!(v, next);
                next += 1;
            }
        }
        t.join().unwrap();
    }
}
