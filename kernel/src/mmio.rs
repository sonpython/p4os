//! Raw memory-mapped register access.

#[inline(always)]
pub fn read(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

#[inline(always)]
pub fn write(addr: usize, value: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, value) }
}

/// Read-modify-write sequences on shared registers (clock and reset
/// control, GPIO matrix, ...) are serialised so the two cores cannot lose
/// each other's bits.
static RMW: crate::spin::SpinLock<()> = crate::spin::SpinLock::new(());

#[inline(always)]
pub fn set_bits(addr: usize, mask: u32) {
    let _rmw = RMW.lock();
    write(addr, read(addr) | mask);
}

#[inline(always)]
pub fn clear_bits(addr: usize, mask: u32) {
    let _rmw = RMW.lock();
    write(addr, read(addr) & !mask);
}

/// Replace the bits in `mask` with `bits`.
#[inline(always)]
pub fn modify(addr: usize, mask: u32, bits: u32) {
    let _rmw = RMW.lock();
    write(addr, (read(addr) & !mask) | (bits & mask));
}
