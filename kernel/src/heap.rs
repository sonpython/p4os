//! Kernel heap: one TLSF allocator (`p4os-tlsf`) over internal SRAM and one
//! over PSRAM. Small allocations prefer fast SRAM; large ones (framebuffers,
//! images, audio buffers) prefer PSRAM so SRAM stays available for stacks
//! and hot kernel data. Either falls back to the other memory when full.
//!
//! TLSF allocates and frees in constant time however fragmented the heap
//! is, so the time each heap lock is held (with interrupts masked, see
//! `spin.rs`) is short and bounded. Each memory has its own lock so both
//! cores can allocate.
//!
//! The heap must not be used from interrupt handlers: a handler that
//! allocates while its own core holds a heap lock would deadlock, and the
//! bounded lock hold time relies on handlers staying out. Every entry point
//! checks this and panics (see `ISR_CHECK`).
//!
//! Thread stacks of common sizes come from per-size pools (`Stack`): a
//! stack freed by an exiting thread is kept for the next thread of that
//! size instead of going back to the heap, so spawning and reaping threads
//! does not punch holes into SRAM.

use crate::spin::SpinLock;
use core::alloc::{GlobalAlloc, Layout};
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use p4os_tlsf::Tlsf;

/// 19 first-level classes: blocks up to 64 MiB, more than PSRAM holds.
type Region = SpinLock<Tlsf<19>>;

static SRAM: Region = SpinLock::new(Tlsf::new());
static PSRAM: Region = SpinLock::new(Tlsf::new());

/// Allocations at least this large go to PSRAM first.
const LARGE: usize = 64 * 1024;

/// Panic when the heap is entered from trap context. The check is a stack
/// pointer comparison, a few cycles per call, so it stays on in release
/// builds; set to `false` only to measure its cost.
const ISR_CHECK: bool = true;

pub struct Heap;

#[global_allocator]
static HEAP: Heap = Heap;

unsafe extern "C" {
    static _heap_start: u8;
    static _heap_end: u8;
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
    unsafe { region_of(start).lock().add_region(start as *mut u8, size) };
}

fn is_psram(addr: usize) -> bool {
    (crate::psram::BASE..crate::psram::BASE + 0x0400_0000).contains(&addr)
}

fn region_of(addr: usize) -> &'static Region {
    if is_psram(addr) { &PSRAM } else { &SRAM }
}

/// (used, total) bytes of internal SRAM, block headers included.
pub fn usage() -> (usize, usize) {
    let h = SRAM.lock();
    (h.used(), h.total())
}

/// (used, total) bytes of PSRAM, block headers included.
pub fn psram_usage() -> (usize, usize) {
    let h = PSRAM.lock();
    (h.used(), h.total())
}

/// Peak (SRAM, PSRAM) bytes used since the previous call; restarts the
/// measurement from the current usage.
pub fn take_peaks() -> (usize, usize) {
    (SRAM.lock().take_peak(), PSRAM.lock().take_peak())
}

/// Largest SRAM allocation that would succeed right now (to within 1/16),
/// a quick fragmentation indicator.
pub fn largest_free() -> usize {
    SRAM.lock().largest_free()
}

fn check_thread_context() {
    if ISR_CHECK && crate::irq::in_trap() {
        // The console bypasses its capture buffer in trap context, so the
        // panic message itself does not allocate.
        panic!("heap used from interrupt context");
    }
}

fn region_alloc(region: &Region, layout: &Layout) -> *mut u8 {
    check_thread_context();
    region.lock().alloc(layout.size(), layout.align()).map_or(ptr::null_mut(), NonNull::as_ptr)
}

/// Allocate from internal SRAM only, whatever the size (the global
/// allocator would send large blocks to PSRAM). Returns null when SRAM has
/// no fitting block. Free with the global allocator (`alloc::alloc::dealloc`).
pub fn alloc_sram(layout: Layout) -> *mut u8 {
    region_alloc(&SRAM, &layout)
}

/// Allocate preferring SRAM, falling back to PSRAM.
fn alloc_sram_first(layout: &Layout) -> *mut u8 {
    let p = region_alloc(&SRAM, layout);
    if p.is_null() { region_alloc(&PSRAM, layout) } else { p }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() >= LARGE {
            let p = region_alloc(&PSRAM, &layout);
            if p.is_null() { region_alloc(&SRAM, &layout) } else { p }
        } else {
            alloc_sram_first(&layout)
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        check_thread_context();
        let Some(p) = NonNull::new(ptr) else { return };
        unsafe { region_of(ptr as usize).lock().free(p) };
    }
}

// --- thread stacks ----------------------------------------------------------

/// Stack sizes served from pools; other sizes use the heap directly.
const STACK_CLASSES: [usize; 7] = [2048, 4096, 8192, 16384, 24576, 32768, 49152];

/// Idle SRAM stacks per class, linked through their first word.
struct StackPool {
    head: [*mut u8; STACK_CLASSES.len()],
    count: [u16; STACK_CLASSES.len()],
}

// The idle stacks are memory owned by the pool, reached only under its lock.
unsafe impl Send for StackPool {}

static STACKS: SpinLock<StackPool> = SpinLock::new(StackPool {
    head: [ptr::null_mut(); STACK_CLASSES.len()],
    count: [0; STACK_CLASSES.len()],
});

/// Number and total bytes of idle stacks kept in the pools.
pub fn stack_pool_idle() -> (usize, usize) {
    let p = STACKS.lock();
    p.count.iter().zip(STACK_CLASSES).fold((0, 0), |(n, b), (&c, size)| (n + c as usize, b + c as usize * size))
}

/// A thread stack: `len` bytes of `u32` words, from a stack pool when the
/// size is one of the common ones, otherwise from the heap. Returned to
/// where it came from on drop.
pub struct Stack {
    ptr: NonNull<u32>,
    words: usize,
    layout: Layout,
    /// Pool class to return to; `None` for heap stacks and PSRAM fallbacks.
    class: Option<usize>,
}

// The stack memory is owned exclusively by this handle.
unsafe impl Send for Stack {}

impl Stack {
    /// A stack of `len` bytes (a multiple of 16) with every word set to
    /// `fill`. Out of memory is fatal, as for any other allocation.
    pub fn new(len: usize, fill: u32) -> Stack {
        let class = STACK_CLASSES.iter().position(|&c| c >= len);
        let layout = Layout::from_size_align(class.map_or(len, |c| STACK_CLASSES[c]), 16).expect("stack size");
        check_thread_context();
        let reused = class.and_then(|c| {
            let mut pool = STACKS.lock();
            let p = pool.head[c];
            if p.is_null() {
                return None;
            }
            pool.head[c] = unsafe { p.cast::<*mut u8>().read() };
            pool.count[c] -= 1;
            Some(p)
        });
        let p = match reused {
            Some(p) => p,
            None if class.is_some() => alloc_sram_first(&layout),
            None => unsafe { HEAP.alloc(layout) },
        };
        let Some(ptr) = NonNull::new(p.cast::<u32>()) else { alloc::alloc::handle_alloc_error(layout) };
        // Only SRAM stacks are worth keeping; a PSRAM fallback goes back
        // to the heap so a later spawn can get SRAM.
        let class = class.filter(|_| !is_psram(p as usize));
        let mut stack = Stack { ptr, words: len / 4, layout, class };
        stack.fill(fill);
        stack
    }
}

impl Deref for Stack {
    type Target = [u32];
    fn deref(&self) -> &[u32] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.words) }
    }
}

impl DerefMut for Stack {
    fn deref_mut(&mut self) -> &mut [u32] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.words) }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let p = self.ptr.as_ptr().cast::<u8>();
        match self.class {
            Some(c) => {
                check_thread_context();
                let mut pool = STACKS.lock();
                unsafe { p.cast::<*mut u8>().write(pool.head[c]) };
                pool.head[c] = p;
                pool.count[c] += 1;
            }
            None => unsafe { HEAP.dealloc(p, self.layout) },
        }
    }
}
