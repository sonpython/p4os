//! A cheap view into a shared buffer.
//!
//! Demuxed packets and access units point into the fetched chunk instead
//! of being copied into thousands of small allocations: the OS heap serves
//! small blocks from the scarce internal SRAM, while a chunk (hundreds of
//! KB) lives in PSRAM.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::ops::{Deref, Range};

#[derive(Clone)]
pub struct Slice {
    buf: Arc<Vec<u8>>,
    start: u32,
    end: u32,
}

impl Slice {
    /// `range` must lie inside `buf`.
    pub fn new(buf: &Arc<Vec<u8>>, range: Range<usize>) -> Self {
        assert!(range.start <= range.end && range.end <= buf.len(), "slice out of range");
        Slice { buf: buf.clone(), start: range.start as u32, end: range.end as u32 }
    }

    /// A slice owning its own copy of `data`.
    pub fn copy_of(data: &[u8]) -> Self {
        let buf = Arc::new(data.to_vec());
        Slice::new(&buf, 0..data.len())
    }
}

impl Deref for Slice {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf[self.start as usize..self.end as usize]
    }
}

impl PartialEq for Slice {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for Slice {}

impl fmt::Debug for Slice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Slice({} bytes)", self.len())
    }
}
