//! Two-level segregated fit (TLSF) allocator over raw memory regions.
//!
//! Free blocks are kept in one doubly linked list per size class. Classes
//! come in two levels: the first splits sizes by power of two, the second
//! splits each power-of-two range into [`SL`] equal steps. Two bitmaps
//! record which lists are non-empty, so finding a free block that fits is a
//! couple of bit scans, and allocation and free run in constant time no
//! matter how many free blocks the heap holds (Masmano et al., "TLSF: a new
//! dynamic memory allocator for real-time systems", ECRTS 2004).
//!
//! Every block starts with a two-word header: a pointer to the physically
//! preceding block and the block size, whose low bit marks the block free.
//! Free blocks also hold their list links in what would be the payload.
//! Neighbouring free blocks are merged on free, so no two free blocks are
//! ever adjacent. Each region ends with a zero-size "used" sentinel header
//! so merging never runs past it.
//!
//! The search rounds the request up to the next class boundary, so any
//! block in the list it lands on fits without walking the list ("good
//! fit"). The price is that a block in the request's own class that would
//! have fitted can be skipped; when the rounded search finds nothing, the
//! first few blocks of that class are checked as a last resort.
//!
//! The allocator is not thread-safe; wrap it in a lock.

#![cfg_attr(not(test), no_std)]

use core::mem::size_of;
use core::ptr::{self, NonNull};

const WORD: usize = size_of::<usize>();
/// Bytes of header in front of every payload.
pub const HEADER: usize = 2 * WORD;
/// Alignment of every returned pointer and granularity of block sizes:
/// four words, i.e. 16 bytes on the 32-bit target. Tying it to the word
/// size keeps the header at half a granule everywhere, so the 64-bit host
/// tests exercise the same layout arithmetic as the target.
pub const ALIGN: usize = 4 * WORD;
const ALIGN_LOG2: u32 = ALIGN.trailing_zeros();
/// Smallest block: header plus the two free-list links.
pub const MIN_BLOCK: usize = 4 * WORD;
/// Second-level classes per power of two.
pub const SL_LOG2: u32 = 4;
pub const SL: usize = 1 << SL_LOG2;
/// Sizes below `SMALL` all live in first-level row 0, one class per granule.
const FL_SHIFT: u32 = SL_LOG2 + ALIGN_LOG2;
const SMALL: usize = 1 << FL_SHIFT;
const FREE: usize = 1;
const FLAGS: usize = ALIGN - 1;
/// Blocks of the request's own class examined after the good-fit search
/// fails. A constant, so allocation stays O(1).
const EXACT_CLASS_PROBES: usize = 4;
/// Free blocks of the top class examined by `largest_free`.
const LARGEST_FREE_SCAN: usize = 64;

#[repr(C)]
struct Block {
    prev_phys: *mut Block,
    /// Size in bytes including the header; low bits hold flags.
    size: usize,
    /// Free blocks only.
    next_free: *mut Block,
    prev_free: *mut Block,
}

unsafe fn len_of(b: *mut Block) -> usize {
    unsafe { (*b).size & !FLAGS }
}

unsafe fn is_free(b: *mut Block) -> bool {
    unsafe { (*b).size & FREE != 0 }
}

unsafe fn next_phys(b: *mut Block) -> *mut Block {
    unsafe { b.byte_add(len_of(b)) }
}

fn log2(v: usize) -> u32 {
    usize::BITS - 1 - v.leading_zeros()
}

/// Size class (first level, second level) holding blocks of `size` bytes.
fn mapping(size: usize) -> (usize, usize) {
    if size < SMALL {
        (0, size >> ALIGN_LOG2)
    } else {
        let f = log2(size);
        let sl = (size >> (f - SL_LOG2)) ^ SL;
        ((f - FL_SHIFT + 1) as usize, sl)
    }
}

/// Block size (header included) needed for a payload of `size` bytes.
fn block_size(size: usize) -> Option<usize> {
    size.max(1).checked_add(HEADER)?.checked_next_multiple_of(ALIGN)
}

/// TLSF heap with `FL` first-level rows; blocks may be up to
/// [`Tlsf::MAX_BLOCK`] bytes. Larger regions are split into several
/// independent blocks.
pub struct Tlsf<const FL: usize> {
    fl_bitmap: u32,
    sl_bitmap: [u32; FL],
    heads: [[*mut Block; SL]; FL],
    total: usize,
    used: usize,
    peak: usize,
}

// The block pointers refer to memory the heap owns exclusively.
unsafe impl<const FL: usize> Send for Tlsf<FL> {}

impl<const FL: usize> Default for Tlsf<FL> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const FL: usize> Tlsf<FL> {
    /// Largest block (header included) the class table can hold.
    pub const MAX_BLOCK: usize = (1 << (FL as u32 + FL_SHIFT - 1)) - ALIGN;

    pub const fn new() -> Self {
        assert!(FL >= 2 && FL <= 32, "first-level count must be 2..=32");
        Self {
            fl_bitmap: 0,
            sl_bitmap: [0; FL],
            heads: [[ptr::null_mut(); SL]; FL],
            total: 0,
            used: 0,
            peak: 0,
        }
    }

    /// Bytes managed, block headers included.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Bytes in allocated blocks, headers and rounding included.
    pub fn used(&self) -> usize {
        self.used
    }

    /// High-water mark of `used` since the last `take_peak`, which
    /// restarts it from the current usage.
    pub fn take_peak(&mut self) -> usize {
        let peak = self.peak;
        self.peak = self.used;
        peak
    }

    /// Payload size of the largest free block among the first few of the
    /// biggest non-empty class: exact unless that class holds more blocks
    /// than are scanned, and never more than 1/16 below the truth.
    pub fn largest_free(&self) -> usize {
        if self.fl_bitmap == 0 {
            return 0;
        }
        let fl = log2(self.fl_bitmap as usize) as usize;
        let sl = log2(self.sl_bitmap[fl] as usize) as usize;
        let mut best = 0;
        let mut b = self.heads[fl][sl];
        for _ in 0..LARGEST_FREE_SCAN {
            if b.is_null() {
                break;
            }
            unsafe {
                best = best.max(len_of(b));
                b = (*b).next_free;
            }
        }
        best - HEADER
    }

    /// Hand `[start, start + len)` to the heap. Returns the bytes added
    /// (alignment, the end sentinel and region splitting cost a little).
    ///
    /// # Safety
    /// The range must be valid, writable memory that nothing else uses and
    /// that outlives the heap.
    pub unsafe fn add_region(&mut self, start: *mut u8, len: usize) -> usize {
        let Some(end) = start.addr().checked_add(len) else { return 0 };
        let mut added = 0;
        // Payloads must be ALIGN-aligned, so headers sit HEADER below that.
        let mut b = (start.addr() + HEADER).next_multiple_of(ALIGN) - HEADER;
        loop {
            // The block plus the sentinel header that closes it.
            let chunk_end = end.min(b.saturating_add(Self::MAX_BLOCK + HEADER));
            let sentinel = (chunk_end & !(ALIGN - 1)).wrapping_sub(HEADER);
            if chunk_end < b + MIN_BLOCK + HEADER || sentinel < b + MIN_BLOCK {
                return added;
            }
            let block_len = sentinel - b;
            unsafe {
                let blk = start.byte_add(b - start.addr()).cast::<Block>();
                let sen = start.byte_add(sentinel - start.addr()).cast::<Block>();
                blk.write(Block {
                    prev_phys: ptr::null_mut(),
                    size: block_len | FREE,
                    next_free: ptr::null_mut(),
                    prev_free: ptr::null_mut(),
                });
                (&raw mut (*sen).prev_phys).write(blk);
                (&raw mut (*sen).size).write(0);
                self.insert(blk);
            }
            self.total += block_len;
            added += block_len;
            // A further chunk starts after this one's sentinel.
            b = (sentinel + 2 * HEADER).next_multiple_of(ALIGN) - HEADER;
        }
    }

    /// Allocate `size` bytes aligned to `align` (a power of two). Returns
    /// `None` when no free block fits.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<NonNull<u8>> {
        if !align.is_power_of_two() {
            return None;
        }
        let align = align.max(ALIGN);
        let need = block_size(size)?;
        // Room to move the payload up to `align`, leaving a gap big enough
        // to stand as a free block of its own.
        let search = if align == ALIGN { need } else { need.checked_add(align - ALIGN + MIN_BLOCK)? };
        if search > Self::MAX_BLOCK {
            return None;
        }
        unsafe {
            let mut b = self.take_fitting(search)?;
            if align > ALIGN {
                b = self.split_front(b, align);
            }
            self.split_back(b, need);
            let block_len = len_of(b);
            (*b).size = block_len;
            self.used += block_len;
            self.peak = self.peak.max(self.used);
            Some(NonNull::new_unchecked(b.cast::<u8>().byte_add(HEADER)))
        }
    }

    /// Return a block to the heap, merging it with free neighbours.
    ///
    /// # Safety
    /// `ptr` must come from `alloc` on this heap and not be freed already.
    pub unsafe fn free(&mut self, ptr: NonNull<u8>) {
        unsafe {
            let mut b = ptr.as_ptr().byte_sub(HEADER).cast::<Block>();
            debug_assert!(!is_free(b), "double free");
            let mut block_len = len_of(b);
            self.used -= block_len;
            let next = next_phys(b);
            if is_free(next) {
                self.remove(next);
                block_len += len_of(next);
            }
            let prev = (*b).prev_phys;
            if !prev.is_null() && is_free(prev) {
                self.remove(prev);
                block_len += len_of(prev);
                b = prev;
            }
            (*b).size = block_len | FREE;
            (*next_phys(b)).prev_phys = b;
            self.insert(b);
        }
    }

    /// Detach a free block of at least `need` bytes from its list.
    unsafe fn take_fitting(&mut self, need: usize) -> Option<*mut Block> {
        let rounded = if need >= SMALL { need + (1 << (log2(need) - SL_LOG2)) - 1 } else { need };
        unsafe {
            if let Some((fl, sl)) = self.find(rounded) {
                let b = self.heads[fl][sl];
                self.unlink(b, fl, sl);
                return Some(b);
            }
            let (fl, sl) = mapping(need);
            if fl >= FL {
                return None;
            }
            let mut b = self.heads[fl][sl];
            for _ in 0..EXACT_CLASS_PROBES {
                if b.is_null() {
                    break;
                }
                if len_of(b) >= need {
                    self.remove(b);
                    return Some(b);
                }
                b = (*b).next_free;
            }
        }
        None
    }

    /// First non-empty class at or above the one holding `size`.
    fn find(&self, size: usize) -> Option<(usize, usize)> {
        let (fl, sl) = mapping(size);
        if fl >= FL {
            return None;
        }
        let mut sl_map = self.sl_bitmap[fl] & (!0u32 << sl);
        let mut fl = fl;
        if sl_map == 0 {
            let fl_map = self.fl_bitmap & (!0u32).checked_shl(fl as u32 + 1).unwrap_or(0);
            if fl_map == 0 {
                return None;
            }
            fl = fl_map.trailing_zeros() as usize;
            sl_map = self.sl_bitmap[fl];
        }
        Some((fl, sl_map.trailing_zeros() as usize))
    }

    /// `b` is a detached free block. Split off its start so the payload is
    /// `align`-aligned, put the start back as a free block and return the
    /// rest.
    unsafe fn split_front(&mut self, b: *mut Block, align: usize) -> *mut Block {
        unsafe {
            let payload = b.addr() + HEADER;
            let mut aligned = payload.next_multiple_of(align);
            if aligned != payload && aligned - payload < MIN_BLOCK {
                aligned = (payload + MIN_BLOCK).next_multiple_of(align);
            }
            let gap = aligned - payload;
            if gap == 0 {
                return b;
            }
            let block_len = len_of(b);
            let rest = b.byte_add(gap);
            (*rest).prev_phys = b;
            (*rest).size = (block_len - gap) | FREE;
            (*next_phys(rest)).prev_phys = rest;
            (*b).size = gap | FREE;
            // `b`'s physical neighbours are both in use: no merge needed.
            self.insert(b);
            rest
        }
    }

    /// `b` is a detached free block. Keep its first `need` bytes and return
    /// the tail to the heap if it is big enough to be a block.
    unsafe fn split_back(&mut self, b: *mut Block, need: usize) {
        unsafe {
            let block_len = len_of(b);
            if block_len - need < MIN_BLOCK {
                return;
            }
            let rest = b.byte_add(need);
            (*rest).prev_phys = b;
            (*rest).size = (block_len - need) | FREE;
            (*next_phys(rest)).prev_phys = rest;
            (*b).size = need | FREE;
            self.insert(rest);
        }
    }

    unsafe fn insert(&mut self, b: *mut Block) {
        unsafe {
            let (fl, sl) = mapping(len_of(b));
            let head = self.heads[fl][sl];
            (*b).next_free = head;
            (*b).prev_free = ptr::null_mut();
            if !head.is_null() {
                (*head).prev_free = b;
            }
            self.heads[fl][sl] = b;
            self.fl_bitmap |= 1 << fl;
            self.sl_bitmap[fl] |= 1 << sl;
        }
    }

    unsafe fn remove(&mut self, b: *mut Block) {
        let (fl, sl) = mapping(unsafe { len_of(b) });
        unsafe { self.unlink(b, fl, sl) };
    }

    /// Take free block `b` out of list (`fl`, `sl`), which must hold it.
    unsafe fn unlink(&mut self, b: *mut Block, fl: usize, sl: usize) {
        unsafe {
            let (next, prev) = ((*b).next_free, (*b).prev_free);
            if !next.is_null() {
                (*next).prev_free = prev;
            }
            if prev.is_null() {
                self.heads[fl][sl] = next;
                if next.is_null() {
                    self.sl_bitmap[fl] &= !(1 << sl);
                    if self.sl_bitmap[fl] == 0 {
                        self.fl_bitmap &= !(1 << fl);
                    }
                }
            } else {
                (*prev).next_free = next;
            }
        }
    }
}

#[cfg(test)]
mod tests;
