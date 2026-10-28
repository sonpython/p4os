//! Heap attribution for the simulator, mirroring the kernel's owner tags:
//! a counting global allocator charges every allocation to the owner tag
//! of the allocating thread. Blocks of 64 KiB and more count as PSRAM, the
//! rest as SRAM, like the board's heap places them.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicIsize, Ordering};

pub const OWNERS: usize = 16;
const LARGE: usize = 64 * 1024;

static USAGE: [[AtomicIsize; 2]; OWNERS] = [const { [const { AtomicIsize::new(0) }; 2] }; OWNERS];

thread_local! {
    static OWNER: Cell<u8> = const { Cell::new(0) };
}

fn owner() -> usize {
    // During thread teardown the slot may be gone; count as the system.
    OWNER.try_with(|o| o.get() as usize).unwrap_or(0).min(OWNERS - 1)
}

/// Set the calling thread's owner tag; returns the previous one.
pub fn set_owner(tag: u8) -> u8 {
    OWNER.with(|o| o.replace(tag.min(OWNERS as u8 - 1)))
}

/// Bytes (SRAM, PSRAM) attributed to `tag`; frees by other owners can
/// drive a counter below zero, which reads as zero.
pub fn usage(tag: u8) -> (usize, usize) {
    let u = &USAGE[(tag as usize).min(OWNERS - 1)];
    (u[0].load(Ordering::Relaxed).max(0) as usize, u[1].load(Ordering::Relaxed).max(0) as usize)
}

pub fn clear(tag: u8) {
    for c in &USAGE[(tag as usize).min(OWNERS - 1)] {
        c.store(0, Ordering::Relaxed);
    }
}

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            USAGE[owner()][(layout.size() >= LARGE) as usize].fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        USAGE[owner()][(layout.size() >= LARGE) as usize].fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }
}
