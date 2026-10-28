//! Host tests: invariants after every kind of operation, a randomized
//! stress run checked against a model of the live allocations, alignment,
//! and fragmentation behaviour.

use super::*;
use std::alloc::{Layout, alloc as sys_alloc, dealloc as sys_dealloc};
use std::collections::{BTreeMap, HashSet};

type Heap = Tlsf<14>;

/// Backing memory for a test heap, freed on drop.
struct Arena {
    ptr: *mut u8,
    layout: Layout,
}

impl Arena {
    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, 4096).unwrap();
        let ptr = unsafe { sys_alloc(layout) };
        assert!(!ptr.is_null());
        Arena { ptr, layout }
    }

    fn range(&self) -> (usize, usize) {
        (self.ptr.addr(), self.ptr.addr() + self.layout.size())
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        unsafe { sys_dealloc(self.ptr, self.layout) };
    }
}

fn heap_over(arenas: &[(&Arena, usize, usize)]) -> Heap {
    let mut h = Heap::new();
    for &(a, off, len) in arenas {
        unsafe { h.add_region(a.ptr.add(off), len) };
    }
    h
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Every free block reachable from the class lists: (address, size).
fn free_list_blocks<const FL: usize>(h: &Tlsf<FL>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for fl in 0..FL {
        for sl in 0..SL {
            let listed = h.heads[fl][sl].is_null();
            let bit = h.sl_bitmap[fl] & (1 << sl) != 0;
            assert_eq!(!listed, bit, "sl bitmap out of sync at ({fl}, {sl})");
            let mut b = h.heads[fl][sl];
            let mut prev = ptr::null_mut();
            while !b.is_null() {
                unsafe {
                    assert!(is_free(b), "used block in a free list");
                    assert_eq!((*b).prev_free, prev, "broken back link");
                    assert_eq!(mapping(len_of(b)), (fl, sl), "block filed in the wrong class");
                    out.push((b.addr(), len_of(b)));
                    prev = b;
                    b = (*b).next_free;
                }
            }
        }
        assert_eq!(h.sl_bitmap[fl] != 0, h.fl_bitmap & (1 << fl) != 0, "fl bitmap out of sync at {fl}");
    }
    out
}

/// Walk every block of every region physically and check the heap
/// invariants. `regions` are the ranges given to `add_region`.
fn check<const FL: usize>(h: &Tlsf<FL>, regions: &[(usize, usize)]) {
    let listed = free_list_blocks(h);
    let listed_set: HashSet<usize> = listed.iter().map(|&(a, _)| a).collect();
    assert_eq!(listed_set.len(), listed.len(), "block listed twice");

    let (mut free_seen, mut used_sum, mut total_sum) = (0, 0, 0);
    for &(start, end) in regions {
        let mut b = (start + HEADER).next_multiple_of(ALIGN) - HEADER;
        while b + MIN_BLOCK + HEADER <= end {
            let mut prev: *mut Block = ptr::null_mut();
            let mut prev_free = false;
            let mut cur = b as *mut Block;
            unsafe {
                if len_of(cur) == 0 {
                    break; // region too small to have been used
                }
                loop {
                    assert_eq!((*cur).prev_phys, prev, "prev_phys link broken");
                    let len = len_of(cur);
                    if len == 0 {
                        break; // sentinel
                    }
                    assert!(len >= MIN_BLOCK && len % ALIGN == 0 && len <= Tlsf::<FL>::MAX_BLOCK);
                    assert_eq!((cur.addr() + HEADER) % ALIGN, 0, "misaligned payload");
                    assert!(cur.addr() + len + HEADER <= end, "block runs past its region");
                    total_sum += len;
                    if is_free(cur) {
                        assert!(!prev_free, "two adjacent free blocks");
                        assert!(listed_set.contains(&cur.addr()), "free block missing from lists");
                        free_seen += 1;
                    } else {
                        used_sum += len;
                    }
                    prev_free = is_free(cur);
                    prev = cur;
                    cur = next_phys(cur);
                }
            }
            b = (cur.addr() + 2 * HEADER).next_multiple_of(ALIGN) - HEADER;
        }
    }
    assert_eq!(free_seen, listed.len(), "free list holds blocks not in any region");
    assert_eq!(used_sum, h.used(), "used counter drifted");
    assert_eq!(total_sum, h.total(), "total counter drifted");
}

fn fill(p: NonNull<u8>, len: usize, tag: u8) {
    unsafe { ptr::write_bytes(p.as_ptr(), tag, len) };
}

fn verify(p: NonNull<u8>, len: usize, tag: u8) {
    let s = unsafe { std::slice::from_raw_parts(p.as_ptr(), len) };
    assert!(s.iter().all(|&b| b == tag), "allocation contents overwritten");
}

#[test]
fn mapping_is_monotonic_and_covers_max_block() {
    let mut last = (0, 0);
    for size in (MIN_BLOCK..=Heap::MAX_BLOCK).step_by(ALIGN).filter(|s| s % 4096 == 0 || *s < 8192) {
        let c = mapping(size);
        assert!(c >= last, "class order broke at {size}");
        assert!(c.0 < 14 && c.1 < SL);
        last = c;
    }
    assert_eq!(mapping(Heap::MAX_BLOCK).0, 13);
}

#[test]
fn block_sizes_are_whole_granules() {
    assert_eq!(HEADER * 2, ALIGN, "tests rely on the target's header/granule ratio");
    for size in 0..4 * ALIGN {
        let b = block_size(size).unwrap();
        assert!(b % ALIGN == 0 && b >= MIN_BLOCK && b - HEADER >= size, "size {size} -> block {b}");
        assert!(b - HEADER - size.max(1) < ALIGN, "size {size} wastes a granule");
    }
}

#[test]
fn single_alloc_free_restores_the_heap() {
    let a = Arena::new(64 * 1024);
    let mut h = heap_over(&[(&a, 0, 64 * 1024)]);
    let r = [a.range()];
    let total = h.total();
    let largest = h.largest_free();
    assert_eq!(largest, total - HEADER);
    check(&h, &r);

    let p = h.alloc(100, 1).unwrap();
    assert_eq!(h.used(), block_size(100).unwrap());
    fill(p, 100, 0xAB);
    check(&h, &r);
    unsafe { h.free(p) };
    check(&h, &r);
    assert_eq!(h.used(), 0);
    assert_eq!(h.largest_free(), largest);
}

#[test]
fn whole_region_can_be_allocated_in_one_block() {
    let a = Arena::new(64 * 1024);
    let mut h = heap_over(&[(&a, 0, 64 * 1024)]);
    let largest = h.largest_free();
    let p = h.alloc(largest, 16).expect("exact-size request must fit");
    assert!(h.alloc(1, 1).is_none(), "heap should be full");
    unsafe { h.free(p) };
    check(&h, &[a.range()]);
}

#[test]
fn rejects_impossible_requests() {
    let a = Arena::new(16 * 1024);
    let mut h = heap_over(&[(&a, 0, 16 * 1024)]);
    assert!(h.alloc(16, 3).is_none(), "non power-of-two alignment");
    assert!(h.alloc(usize::MAX, 16).is_none());
    assert!(h.alloc(usize::MAX - 64, 16).is_none());
    assert!(h.alloc(16, 1 << (usize::BITS - 1)).is_none());
    assert!(h.alloc(Heap::MAX_BLOCK, 16).is_none(), "larger than any block");
    assert!(h.alloc(32 * 1024, 16).is_none(), "larger than the heap");
    assert_eq!(h.used(), 0);
}

#[test]
fn tiny_and_misaligned_regions() {
    let a = Arena::new(4096);
    let mut h = Heap::new();
    assert_eq!(unsafe { h.add_region(a.ptr.add(3), MIN_BLOCK) }, 0, "too small to hold a block");
    let added = unsafe { h.add_region(a.ptr.add(5), 1000) };
    assert!(added > 0 && added <= 1000);
    check(&h, &[(a.ptr.addr() + 5, a.ptr.addr() + 1005)]);
    let p = h.alloc(16, 1).unwrap();
    assert_eq!(p.as_ptr().addr() % ALIGN, 0);
}

#[test]
fn regions_larger_than_max_block_are_split() {
    let len = 3 * Heap::MAX_BLOCK + 12_345;
    let a = Arena::new(len);
    let mut h = heap_over(&[(&a, 0, len)]);
    let r = [a.range()];
    check(&h, &r);
    assert!(h.total() > 3 * Heap::MAX_BLOCK - 4 * ALIGN);
    // Blocks from neighbouring chunks never merge past MAX_BLOCK.
    let big: Vec<_> = (0..3).map(|_| h.alloc(Heap::MAX_BLOCK - HEADER, 16).expect("one chunk each")).collect();
    check(&h, &r);
    for p in big {
        unsafe { h.free(p) };
    }
    check(&h, &r);
    assert_eq!(h.largest_free(), Heap::MAX_BLOCK - HEADER);
}

#[test]
fn alignment_up_to_page_size() {
    let a = Arena::new(1 << 20);
    let mut h = heap_over(&[(&a, 0, 1 << 20)]);
    let r = [a.range()];
    let mut rng = Rng(7);
    let mut live = Vec::new();
    for i in 0..2000 {
        let align = 1usize << rng.below(13);
        let size = 1 + rng.below(700) as usize;
        let Some(p) = h.alloc(size, align) else { continue };
        assert_eq!(p.as_ptr().addr() % align, 0, "align {align} not honoured");
        fill(p, size, i as u8);
        live.push((p, size, i as u8));
        if rng.below(3) == 0 {
            let (p, size, tag) = live.swap_remove(rng.below(live.len() as u64) as usize);
            verify(p, size, tag);
            unsafe { h.free(p) };
        }
    }
    check(&h, &r);
    for (p, size, tag) in live {
        verify(p, size, tag);
        unsafe { h.free(p) };
    }
    check(&h, &r);
    assert_eq!(h.used(), 0);
    assert_eq!(h.largest_free(), h.total() - HEADER, "free space did not coalesce");
}

/// Random mix of sizes (tiny to hundreds of KiB) and alignments over three
/// regions, checked after every step against a model of live allocations:
/// no overlap, contents intact, counters exact, invariants hold.
#[test]
fn randomized_stress_against_model() {
    let big = 2 * Heap::MAX_BLOCK + 5000;
    let arenas = [Arena::new(256 * 1024), Arena::new(200 * 1024 + 8), Arena::new(big)];
    let regions: Vec<(usize, usize)> = vec![
        (arenas[0].ptr.addr() + 8, arenas[0].ptr.addr() + 256 * 1024),
        (arenas[1].ptr.addr(), arenas[1].ptr.addr() + 200 * 1024 + 8),
        (arenas[2].ptr.addr() + 100, arenas[2].ptr.addr() + big - 3),
    ];
    let mut h = Heap::new();
    for &(s, e) in &regions {
        unsafe { h.add_region(s as *mut u8, e - s) };
    }
    let total = h.total();
    // The big region exceeds MAX_BLOCK and starts out as three blocks.
    let initial_blocks = free_list_blocks(&h).len();
    assert_eq!(initial_blocks, 5);
    for seed in 1..=4u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        // address -> (requested len, block len, fill byte)
        let mut model: BTreeMap<usize, (usize, usize, u8)> = BTreeMap::new();
        let mut used = 0;
        for step in 0..30_000u32 {
            let alloc_now = model.is_empty() || rng.below(100) < 55;
            if alloc_now {
                let size = match rng.below(100) {
                    0..=59 => 1 + rng.below(256),
                    60..=89 => 1 + rng.below(4096),
                    90..=98 => 1 + rng.below(64 * 1024),
                    _ => 1 + rng.below(512 * 1024),
                } as usize;
                let align = if rng.below(10) == 0 { 1 << rng.below(10) } else { 4 };
                let Some(p) = h.alloc(size, align) else {
                    // Good fit: failure means no free block of the rounded size.
                    let mut need = block_size(size).unwrap();
                    if align > ALIGN {
                        need += align - ALIGN + MIN_BLOCK;
                    }
                    let rounded = if need >= SMALL { need + (1 << (log2(need) - SL_LOG2)) - 1 } else { need };
                    let biggest = free_list_blocks(&h).iter().map(|&(_, s)| s).max().unwrap_or(0);
                    assert!(biggest < rounded.max(need), "alloc of {size} failed with a {biggest} B block free");
                    continue;
                };
                let addr = p.as_ptr().addr();
                assert_eq!(addr % align.max(ALIGN), 0);
                let blk = unsafe { len_of(p.as_ptr().byte_sub(HEADER).cast()) };
                assert!(blk >= size + HEADER, "block too small for the request");
                if let Some((&a, &(len, _, _))) = model.range(..addr).next_back() {
                    assert!(a + len <= addr, "overlaps the allocation below");
                }
                if let Some((&a, _)) = model.range(addr..).next() {
                    assert!(addr + size <= a, "overlaps the allocation above");
                }
                let tag = step as u8 | 1;
                fill(p, size, tag);
                model.insert(addr, (size, blk, tag));
                used += blk;
            } else {
                let k = rng.below(model.len() as u64) as usize;
                let (&addr, &(len, blk, tag)) = model.iter().nth(k).unwrap();
                let p = NonNull::new(addr as *mut u8).unwrap();
                verify(p, len, tag);
                unsafe { h.free(p) };
                model.remove(&addr);
                used -= blk;
            }
            assert_eq!(h.used(), used);
            if step % 500 == 0 {
                check(&h, &regions);
            }
        }
        check(&h, &regions);
        for (&addr, &(len, _, tag)) in &model {
            let p = NonNull::new(addr as *mut u8).unwrap();
            verify(p, len, tag);
            unsafe { h.free(p) };
        }
        check(&h, &regions);
        assert_eq!(h.used(), 0);
        assert_eq!(h.total(), total);
        assert_eq!(free_list_blocks(&h).len(), initial_blocks, "free space did not coalesce back");
    }
}

/// Holes left by freed blocks are reused for same-size requests, and a
/// fully fragmented heap still recovers one block per region on free.
#[test]
fn fragmentation_holes_are_reused() {
    let a = Arena::new(256 * 1024);
    let mut h = heap_over(&[(&a, 0, 256 * 1024)]);
    let r = [a.range()];
    let mut blocks = Vec::new();
    while let Some(p) = h.alloc(48, 8) {
        blocks.push(p);
    }
    let n = blocks.len();
    assert!(n > 3000);
    // Free every other block: n/2 isolated holes.
    let mut kept = Vec::new();
    for (i, p) in blocks.into_iter().enumerate() {
        if i % 2 == 1 { unsafe { h.free(p) } } else { kept.push(p) }
    }
    check(&h, &r);
    // One free block per hole, plus possibly the unallocatable tail.
    let free_blocks = free_list_blocks(&h).len();
    assert!(free_blocks == n / 2 || free_blocks == n / 2 + 1, "{free_blocks} free blocks for {n} allocations");
    // A request bigger than any hole fails although half the heap is free.
    assert!(h.alloc(200, 8).is_none());
    // Same-size requests fill the holes exactly.
    let refill: Vec<_> = (0..n / 2).map(|_| h.alloc(48, 8).expect("hole reuse")).collect();
    assert!(h.alloc(48, 8).is_none());
    check(&h, &r);
    for p in kept.into_iter().chain(refill) {
        unsafe { h.free(p) };
    }
    check(&h, &r);
    assert_eq!(h.largest_free(), h.total() - HEADER);
}

/// Mixed-size churn on a small heap: the heap keeps serving requests up
/// to a bounded share of its size, and frees always coalesce completely.
#[test]
fn mixed_churn_keeps_fragmentation_bounded() {
    let len = 512 * 1024;
    let a = Arena::new(len);
    let mut h = heap_over(&[(&a, 0, len)]);
    let r = [a.range()];
    let mut rng = Rng(42);
    let mut live: Vec<(NonNull<u8>, usize)> = Vec::new();
    let mut live_bytes = 0;
    let mut failures_below_half = 0;
    for _ in 0..200_000 {
        let size = 16 + rng.below(2048) as usize;
        if live_bytes + size < len / 2 {
            match h.alloc(size, 4) {
                Some(p) => {
                    live.push((p, size));
                    live_bytes += size;
                }
                None => failures_below_half += 1,
            }
        } else {
            let (p, s) = live.swap_remove(rng.below(live.len() as u64) as usize);
            unsafe { h.free(p) };
            live_bytes -= s;
        }
    }
    assert_eq!(failures_below_half, 0, "allocation failed with the heap less than half full");
    check(&h, &r);
    for (p, _) in live {
        unsafe { h.free(p) };
    }
    check(&h, &r);
    assert_eq!(h.largest_free(), h.total() - HEADER);
}

#[test]
fn peak_tracks_high_water_mark() {
    let a = Arena::new(64 * 1024);
    let mut h = heap_over(&[(&a, 0, 64 * 1024)]);
    let p = h.alloc(1000, 4).unwrap();
    let q = h.alloc(2000, 4).unwrap();
    let high = h.used();
    unsafe { h.free(q) };
    assert_eq!(h.take_peak(), high);
    assert_eq!(h.take_peak(), h.used(), "peak restarts from current usage");
    unsafe { h.free(p) };
    assert_eq!(h.take_peak(), high - block_size(2000).unwrap());
}
