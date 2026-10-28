//! Kernel heap: first-fit free-list allocators over internal SRAM and
//! PSRAM. Small allocations prefer fast SRAM; large ones (framebuffers,
//! images, audio buffers) prefer PSRAM so SRAM stays available for stacks
//! and hot kernel data.
//!
//! Free blocks are kept in an address-ordered singly linked list whose
//! nodes live inside the free memory itself, so the allocator needs no
//! metadata of its own. `dealloc` receives the original `Layout`, which
//! lets allocated blocks carry no header either. Neighbouring free blocks
//! are merged on free to fight fragmentation.

use crate::irq;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};

const MIN_BLOCK: usize = core::mem::size_of::<FreeBlock>();
const GRANULE: usize = 16;

#[repr(C)]
struct FreeBlock {
    size: usize,
    next: Option<NonNull<FreeBlock>>,
}

struct State {
    head: Option<NonNull<FreeBlock>>,
    total: usize,
    used: usize,
}

struct Pool(UnsafeCell<State>);

unsafe impl Sync for Pool {}

impl Pool {
    const fn new() -> Self {
        Pool(UnsafeCell::new(State { head: None, total: 0, used: 0 }))
    }
}

static SRAM: Pool = Pool::new();
static PSRAM: Pool = Pool::new();

/// Allocations at least this large go to PSRAM first.
const LARGE: usize = 64 * 1024;

pub struct Heap;

#[global_allocator]
static HEAP: Heap = Heap;

unsafe extern "C" {
    static _heap_start: u8;
    static _heap_end: u8;
}

fn round_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// L2 SRAM above the ROM-reserved area and below the 256 KiB L2 cache that
/// the bootloader configured at the top of the 768 KiB block.
const SRAM_HIGH: (usize, usize) = (0x4FF4_0000, 0x4FF8_0000);

/// Hand the SRAM left after the kernel image to the heap.
pub fn init() {
    let start = &raw const _heap_start as usize;
    let end = &raw const _heap_end as usize;
    unsafe {
        add_region(start, end - start);
        add_region(SRAM_HIGH.0, SRAM_HIGH.1 - SRAM_HIGH.0);
    }
}

/// Add `[start, start+size)` to the heap (PSRAM or SRAM chosen by address).
///
/// # Safety
/// The range must be valid, unused RAM that stays mapped forever.
pub unsafe fn add_region(start: usize, size: usize) {
    let pool = if is_psram(start) { &PSRAM } else { &SRAM };
    let base = round_up(start, GRANULE);
    let size = (start + size - base) & !(GRANULE - 1);
    if size < MIN_BLOCK {
        return;
    }
    irq::free(|| unsafe {
        let st = &mut *pool.0.get();
        st.total += size;
        insert_free(st, base, size);
    });
}

fn is_psram(addr: usize) -> bool {
    (crate::psram::BASE..crate::psram::BASE + 0x0400_0000).contains(&addr)
}

/// (used, total) bytes of internal SRAM.
pub fn usage() -> (usize, usize) {
    pool_usage(&SRAM)
}

/// (used, total) bytes of PSRAM.
pub fn psram_usage() -> (usize, usize) {
    pool_usage(&PSRAM)
}

fn pool_usage(pool: &Pool) -> (usize, usize) {
    irq::free(|| unsafe {
        let st = &*pool.0.get();
        (st.used, st.total)
    })
}

/// Size of the largest free SRAM block, a quick fragmentation indicator.
pub fn largest_free() -> usize {
    irq::free(|| unsafe {
        let st = &*SRAM.0.get();
        let mut best = 0;
        let mut cur = st.head;
        while let Some(b) = cur {
            best = best.max(b.as_ref().size);
            cur = b.as_ref().next;
        }
        best
    })
}

/// Insert a free range keeping address order, merging with neighbours.
unsafe fn insert_free(st: &mut State, addr: usize, size: usize) {
    unsafe {
        let mut prev: Option<NonNull<FreeBlock>> = None;
        let mut cur = st.head;
        while let Some(c) = cur {
            if c.as_ptr() as usize > addr {
                break;
            }
            prev = cur;
            cur = c.as_ref().next;
        }

        let node = addr as *mut FreeBlock;
        node.write(FreeBlock { size, next: cur });
        let mut node = NonNull::new_unchecked(node);

        // Merge with the following block.
        if let Some(c) = cur {
            if addr + size == c.as_ptr() as usize {
                node.as_mut().size += c.as_ref().size;
                node.as_mut().next = c.as_ref().next;
            }
        }
        // Merge with the preceding block, or link after it.
        match prev {
            Some(mut p) if p.as_ptr() as usize + p.as_ref().size == addr => {
                p.as_mut().size += node.as_ref().size;
                p.as_mut().next = node.as_ref().next;
            }
            Some(mut p) => p.as_mut().next = Some(node),
            None => st.head = Some(node),
        }
    }
}

fn block_size(layout: &Layout) -> usize {
    round_up(layout.size().max(MIN_BLOCK), GRANULE)
}

unsafe fn pool_alloc(pool: &Pool, size: usize, align: usize) -> *mut u8 {
    irq::free(|| unsafe {
        let st = &mut *pool.0.get();
        let mut prev: Option<NonNull<FreeBlock>> = None;
        let mut cur = st.head;
        while let Some(c) = cur {
            let start = c.as_ptr() as usize;
            let end = start + c.as_ref().size;
            let base = round_up(start, align);
            if base + size <= end {
                let next = c.as_ref().next;
                match prev {
                    Some(mut p) => p.as_mut().next = next,
                    None => st.head = next,
                }
                // Return the unused head (alignment gap) and tail.
                if base > start {
                    insert_free(st, start, base - start);
                }
                if end > base + size {
                    insert_free(st, base + size, end - base - size);
                }
                st.used += size;
                return base as *mut u8;
            }
            prev = cur;
            cur = c.as_ref().next;
        }
        ptr::null_mut()
    })
}

/// Allocate from internal SRAM only, whatever the size (the global
/// allocator would send large blocks to PSRAM). Returns null when SRAM has
/// no fitting block. Free with the global allocator (`alloc::alloc::dealloc`).
pub fn alloc_sram(layout: Layout) -> *mut u8 {
    unsafe { pool_alloc(&SRAM, block_size(&layout), layout.align().max(GRANULE)) }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = block_size(&layout);
        let align = layout.align().max(GRANULE);
        let (first, second) = if size >= LARGE { (&PSRAM, &SRAM) } else { (&SRAM, &PSRAM) };
        unsafe {
            let p = pool_alloc(first, size, align);
            if p.is_null() { pool_alloc(second, size, align) } else { p }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = block_size(&layout);
        let pool = if is_psram(ptr as usize) { &PSRAM } else { &SRAM };
        irq::free(|| unsafe {
            let st = &mut *pool.0.get();
            st.used -= size;
            insert_free(st, ptr as usize, size);
        });
    }
}
