//! Raw memory-mapped register access.

#[inline(always)]
pub fn read(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

#[inline(always)]
pub fn write(addr: usize, value: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, value) }
}

#[inline(always)]
pub fn set_bits(addr: usize, mask: u32) {
    write(addr, read(addr) | mask);
}

#[inline(always)]
pub fn clear_bits(addr: usize, mask: u32) {
    write(addr, read(addr) & !mask);
}
