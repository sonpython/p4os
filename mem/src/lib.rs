//! `memcpy`, `memmove`, `memset` and `memcmp` for the p4os kernel.
//!
//! `compiler_builtins` ships portable versions that copy one word per loop
//! iteration and compare byte by byte. These versions keep the same structure
//! (align the destination with byte copies, then move whole words, then finish
//! the tail with bytes) but move a full 64-byte L1 cache line per iteration,
//! issuing a group of loads before the matching stores so load latency
//! overlaps, and handle a source that cannot be aligned together with the
//! destination by merging two aligned loads with shifts, so the core never
//! issues a misaligned access.
//!
//! The algorithms work on `u32` words and are plain Rust so they can be
//! differential-tested on the host. The C symbols are only exported for the
//! bare-metal riscv32 target, where they take precedence over the weak ones
//! in `compiler_builtins`.
//!
//! `no_builtins` stops LLVM from recognising the byte and word loops below as
//! `memcpy`/`memset` idioms, which would otherwise turn them into calls to
//! themselves.
//!
//! The shift-merge paths read the aligned word that contains the first (or
//! last) source byte, which can include up to three bytes just outside the
//! source range. An aligned word never crosses a PMP region, cache line or
//! page, so the access cannot fault; those edge words are read with
//! `read_volatile` so the compiler treats them as opaque hardware loads.

#![cfg_attr(not(test), no_std)]
#![no_builtins]

use core::ptr;

const WORD: usize = core::mem::size_of::<u32>();
const MASK: usize = WORD - 1;
/// Below this many bytes the alignment set-up costs more than it saves.
const SMALL: usize = 16;
/// Words per unrolled iteration: one 64-byte L1 cache line.
const LINE_WORDS: usize = 16;
/// Words per group of loads issued before their stores. Eight values plus
/// the loop pointers stay in registers, so the loop never touches the stack.
const GROUP: usize = 8;

/// Copies `n` bytes from `src` to `dst` and returns `dst`. Same contract as
/// C `memcpy`; also correct when the regions overlap with `dst <= src`, which
/// `copy` relies on.
///
/// # Safety
/// `src` must be valid for reads and `dst` for writes of `n` bytes.
#[inline(always)]
pub unsafe fn copy_nonoverlapping(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe { copy_forward(dst, src, n) }
}

/// Copies `n` bytes from `src` to `dst` and returns `dst`; the regions may
/// overlap. Same contract as C `memmove`.
///
/// # Safety
/// `src` must be valid for reads and `dst` for writes of `n` bytes.
#[inline(always)]
pub unsafe fn copy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    // Unsigned distance: `dst` lies inside `(src, src + n)` exactly when it is
    // smaller than `n`. Only then would a forward copy clobber unread input.
    if dst.addr().wrapping_sub(src.addr()) >= n {
        unsafe { copy_forward(dst, src, n) }
    } else {
        unsafe { copy_backward(dst, src, n) }
    }
}

/// Fills `n` bytes at `dst` with `c`. Same contract as C `memset`.
///
/// # Safety
/// `dst` must be valid for writes of `n` bytes.
#[inline(always)]
pub unsafe fn set(mut dst: *mut u8, c: u8, mut n: usize) {
    unsafe {
        if n >= SMALL {
            let head = dst.addr().wrapping_neg() & MASK;
            set_bytes(dst, c, head);
            dst = dst.add(head);
            n -= head;

            let w = u32::from(c) * 0x0101_0101;
            let mut p = dst.cast::<u32>();
            let end = p.add(n / WORD);
            while words_between(p, end) >= LINE_WORDS {
                let mut k = 0;
                while k < LINE_WORDS {
                    p.add(k).write(w);
                    k += 1;
                }
                p = p.add(LINE_WORDS);
            }
            while p < end {
                p.write(w);
                p = p.add(1);
            }
            dst = p.cast();
            n &= MASK;
        }
        set_bytes(dst, c, n);
    }
}

/// Lexicographic comparison of `n` bytes as unsigned values. Same contract
/// as C `memcmp`: negative, zero or positive.
///
/// # Safety
/// `a` and `b` must be valid for reads of `n` bytes.
#[inline(always)]
pub unsafe fn compare(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    unsafe {
        if n >= SMALL && (a.addr() ^ b.addr()) & MASK == 0 {
            let head = a.addr().wrapping_neg() & MASK;
            while i < head {
                let (x, y) = (*a.add(i), *b.add(i));
                if x != y {
                    return i32::from(x) - i32::from(y);
                }
                i += 1;
            }
            let words_end = i + ((n - i) & !MASK);
            // On a mismatching word, stop and let the byte loop find the
            // first differing byte inside it.
            while i < words_end
                && a.add(i).cast::<u32>().read() == b.add(i).cast::<u32>().read()
            {
                i += WORD;
            }
        }
        while i < n {
            let (x, y) = (*a.add(i), *b.add(i));
            if x != y {
                return i32::from(x) - i32::from(y);
            }
            i += 1;
        }
    }
    0
}

#[inline(always)]
fn words_between(from: *const u32, to: *const u32) -> usize {
    (to.addr() - from.addr()) / WORD
}

#[inline(always)]
unsafe fn set_bytes(d: *mut u8, c: u8, n: usize) {
    let mut i = 0;
    while i < n {
        unsafe { d.add(i).write(c) };
        i += 1;
    }
}

#[inline(always)]
unsafe fn copy_bytes_fwd(d: *mut u8, s: *const u8, n: usize) {
    let mut i = 0;
    while i < n {
        unsafe { d.add(i).write(s.add(i).read()) };
        i += 1;
    }
}

#[inline(always)]
unsafe fn copy_bytes_bwd(d: *mut u8, s: *const u8, n: usize) {
    let mut i = n;
    while i > 0 {
        i -= 1;
        unsafe { d.add(i).write(s.add(i).read()) };
    }
}

/// Short copies stay inline in the caller; longer ones go to an out-of-line
/// function. Inlined, the word loops' register pressure makes LLVM save
/// callee-saved registers on entry to `memcpy` itself, a cost every short
/// copy would then pay (riscv32 gets no shrink-wrapping here).
#[inline(always)]
unsafe fn copy_forward(d: *mut u8, s: *const u8, n: usize) -> *mut u8 {
    unsafe {
        if n < SMALL {
            copy_bytes_fwd(d, s, n);
            d
        } else {
            copy_forward_long(d, s, n)
        }
    }
}

#[inline(always)]
unsafe fn copy_backward(d: *mut u8, s: *const u8, n: usize) -> *mut u8 {
    unsafe {
        if n < SMALL {
            copy_bytes_bwd(d, s, n);
            d
        } else {
            copy_backward_long(d, s, n)
        }
    }
}

/// Front-to-back copy of at least `SMALL` bytes; returns `dst`. Every group
/// of loads completes before its stores, and later groups only read higher
/// addresses, so this is also a correct `memmove` whenever `dst <= src`.
#[inline(never)]
unsafe fn copy_forward_long(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe {
        let head = dst.addr().wrapping_neg() & MASK;
        copy_bytes_fwd(dst, src, head);
        let d = dst.add(head);
        let s = src.add(head);
        let n = n - head;

        let words = n / WORD;
        if s.addr() & MASK == 0 {
            copy_words_fwd(d.cast(), s.cast(), words);
        } else {
            copy_words_fwd_shifted(d.cast(), s, words);
        }
        let done = words * WORD;
        copy_bytes_fwd(d.add(done), s.add(done), n - done);
    }
    dst
}

/// Back-to-front copy of at least `SMALL` bytes, the mirror of
/// `copy_forward_long`: correct for any overlap with `dst >= src`.
#[inline(never)]
unsafe fn copy_backward_long(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe {
        let tail = dst.add(n).addr() & MASK;
        let n = n - tail;
        copy_bytes_bwd(dst.add(n), src.add(n), tail);

        let words = n / WORD;
        let de = dst.add(n);
        let se = src.add(n);
        if se.addr() & MASK == 0 {
            copy_words_bwd(de.cast(), se.cast(), words);
        } else {
            copy_words_bwd_shifted(de.cast(), se, words);
        }
        // What is left is the head of the region.
        copy_bytes_bwd(dst, src, n - words * WORD);
    }
    dst
}

/// Loads `GROUP` words from `s + base` and then stores them to `d + base`.
macro_rules! group {
    ($d:expr, $s:expr, $base:expr) => {{
        let mut v = [0u32; GROUP];
        let mut k = 0;
        while k < GROUP {
            v[k] = $s.offset($base + k as isize).read();
            k += 1;
        }
        let mut k = 0;
        while k < GROUP {
            $d.offset($base + k as isize).write(v[k]);
            k += 1;
        }
    }};
}

#[inline(always)]
unsafe fn copy_words_fwd(mut d: *mut u32, mut s: *const u32, n: usize) {
    unsafe {
        let end = d.add(n);
        while words_between(d, end) >= LINE_WORDS {
            group!(d, s, 0);
            group!(d, s, GROUP as isize);
            d = d.add(LINE_WORDS);
            s = s.add(LINE_WORDS);
        }
        while d < end {
            d.write(s.read());
            d = d.add(1);
            s = s.add(1);
        }
    }
}

#[inline(always)]
unsafe fn copy_words_bwd(mut de: *mut u32, mut se: *const u32, n: usize) {
    unsafe {
        let start = de.sub(n);
        while words_between(start, de) >= LINE_WORDS {
            group!(de, se, -(GROUP as isize));
            group!(de, se, -(LINE_WORDS as isize));
            de = de.sub(LINE_WORDS);
            se = se.sub(LINE_WORDS);
        }
        while de > start {
            de = de.sub(1);
            se = se.sub(1);
            de.write(se.read());
        }
    }
}

/// Word copy when `s` is not word-aligned but `d` is. Each destination word
/// is assembled from the two aligned source words it straddles
/// (little-endian: low bytes from the lower word).
#[inline(always)]
unsafe fn copy_words_fwd_shifted(mut d: *mut u32, s: *const u8, n: usize) {
    unsafe {
        let off = s.addr() & MASK;
        let shr = (off * 8) as u32;
        let shl = 32 - shr;
        let end = d.add(n);
        let mut sa = s.sub(off).cast::<u32>();
        // Straddles the start of the source range.
        let mut prev = ptr::read_volatile(sa);
        // The last aligned word needed is `sa + n`, which can straddle the
        // end of the range; the unrolled loop stops short of it so all its
        // loads stay inside the range.
        while words_between(d, end) > GROUP {
            let mut v = [0u32; GROUP];
            let mut k = 0;
            while k < GROUP {
                v[k] = sa.add(k + 1).read();
                k += 1;
            }
            let mut k = 0;
            while k < GROUP {
                d.add(k).write((prev >> shr) | (v[k] << shl));
                prev = v[k];
                k += 1;
            }
            d = d.add(GROUP);
            sa = sa.add(GROUP);
        }
        while d < end {
            let next = ptr::read_volatile(sa.add(1));
            d.write((prev >> shr) | (next << shl));
            prev = next;
            d = d.add(1);
            sa = sa.add(1);
        }
    }
}

/// Mirror of `copy_words_fwd_shifted` walking down from the end pointers.
#[inline(always)]
unsafe fn copy_words_bwd_shifted(mut de: *mut u32, se: *const u8, n: usize) {
    unsafe {
        let off = se.addr() & MASK;
        let shr = (off * 8) as u32;
        let shl = 32 - shr;
        let start = de.sub(n);
        let mut sa = se.sub(off).cast::<u32>();
        // Straddles the end of the source range.
        let mut prev = ptr::read_volatile(sa);
        // The last aligned word needed is `sa - n`, which can straddle the
        // start of the range.
        while words_between(start, de) > GROUP {
            let mut v = [0u32; GROUP];
            let mut k = 0;
            while k < GROUP {
                v[k] = sa.sub(k + 1).read();
                k += 1;
            }
            let mut k = 0;
            while k < GROUP {
                de.sub(k + 1).write((v[k] >> shr) | (prev << shl));
                prev = v[k];
                k += 1;
            }
            de = de.sub(GROUP);
            sa = sa.sub(GROUP);
        }
        while de > start {
            let next = ptr::read_volatile(sa.sub(1));
            de = de.sub(1);
            de.write((next >> shr) | (prev << shl));
            prev = next;
            sa = sa.sub(1);
        }
    }
}

/// The C ABI entry points. Strong definitions here override the weak ones in
/// `compiler_builtins`, so LLVM's `memcpy`/`memmove`/`memset`/`memcmp`/`bcmp`
/// calls (including `copy_from_slice`, `ptr::copy`, slice equality) land here.
#[cfg(all(target_arch = "riscv32", target_os = "none"))]
mod c_abi {
    use core::ffi::{c_int, c_void};

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        unsafe { super::copy_nonoverlapping(dst.cast(), src.cast(), n).cast() }
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        unsafe { super::copy(dst.cast(), src.cast(), n).cast() }
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
        // C converts the fill value to `unsigned char`.
        unsafe { super::set(dst.cast(), c as u8, n) };
        dst
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
        unsafe { super::compare(a.cast(), b.cast(), n) }
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
        unsafe { super::compare(a.cast(), b.cast(), n) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Room around every region so out-of-range writes are caught and the
    /// shift-merge edge loads stay inside the test allocation.
    const PAD: usize = 32;
    const CANARY: u8 = 0xA5;

    fn lengths() -> impl Iterator<Item = usize> {
        (0..=200).chain([255, 256, 257, 511, 1000, 1023, 1024, 1025, 4093, 4096, 4099])
    }

    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    #[test]
    fn copy_nonoverlapping_matches_reference() {
        for len in lengths() {
            let src_buf = pattern(len + 2 * PAD, len as u32);
            for so in 0..8 {
                for dof in 0..8 {
                    let mut dst_buf = vec![CANARY; len + 2 * PAD];
                    let s = &src_buf[PAD + so..];
                    unsafe { copy_nonoverlapping(dst_buf.as_mut_ptr().add(PAD + dof), s.as_ptr(), len) };
                    let mut want = vec![CANARY; len + 2 * PAD];
                    want[PAD + dof..PAD + dof + len].copy_from_slice(&s[..len]);
                    assert_eq!(dst_buf, want, "len {len} src_off {so} dst_off {dof}");
                }
            }
        }
    }

    #[test]
    fn copy_handles_every_overlap() {
        // Source and destination in one buffer, at every relative alignment
        // and every distance up to well past a full unrolled block.
        for len in (0..=160).chain([255, 256, 257, 1000]) {
            let size = len + 2 * PAD + 2 * 140;
            let base = pattern(size, 0xC0FFEE ^ len as u32);
            for so in 0..8 {
                let src = PAD + 140 + so;
                for delta in -140isize..=140 {
                    let dst = (src as isize + delta) as usize;
                    let mut got = base.clone();
                    unsafe {
                        let p = got.as_mut_ptr();
                        copy(p.add(dst), p.add(src), len);
                    }
                    let mut want = base.clone();
                    want.copy_within(src..src + len, dst);
                    assert_eq!(got, want, "len {len} src {src} dst {dst}");
                }
            }
        }
    }

    #[test]
    fn set_matches_reference() {
        for len in lengths() {
            for off in 0..8 {
                for c in [0x00u8, 0x5A, 0xFF] {
                    let mut got = vec![CANARY; len + 2 * PAD];
                    unsafe { set(got.as_mut_ptr().add(PAD + off), c, len) };
                    let mut want = vec![CANARY; len + 2 * PAD];
                    want[PAD + off..PAD + off + len].fill(c);
                    assert_eq!(got, want, "len {len} off {off} c {c:#x}");
                }
            }
        }
    }

    #[test]
    fn compare_matches_reference() {
        for len in (0..=80).chain([255, 1024]) {
            let a_buf = pattern(len + 2 * PAD, 7 + len as u32);
            for ao in 0..8 {
                for bo in 0..8 {
                    let a = &a_buf[PAD + ao..PAD + ao + len];
                    let mut b_buf = vec![0u8; len + 2 * PAD];
                    b_buf[PAD + bo..PAD + bo + len].copy_from_slice(a);
                    let cmp = |b_buf: &[u8]| unsafe {
                        compare(a.as_ptr(), b_buf.as_ptr().add(PAD + bo), len)
                    };
                    assert_eq!(cmp(&b_buf), 0, "equal len {len} ao {ao} bo {bo}");
                    // One differing byte at every position, both directions,
                    // including differences that only the high bit decides.
                    for pos in 0..len {
                        for delta in [1u8, 0x80] {
                            let mut b2 = b_buf.clone();
                            b2[PAD + bo + pos] = a[pos].wrapping_add(delta);
                            let b = &b2[PAD + bo..PAD + bo + len];
                            let got = cmp(&b2);
                            assert_eq!(got.cmp(&0), a.cmp(b), "len {len} ao {ao} bo {bo} pos {pos}");
                            assert_eq!(got, i32::from(a[pos]) - i32::from(b[pos]));
                        }
                    }
                }
            }
        }
    }
}
