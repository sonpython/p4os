//! Random bytes for TLS key exchange and nonces.
//!
//! Entropy comes from the LP_SYSTEM RNG register, sampled together with the
//! CPU cycle counter and the systimer. ESP-IDF only enables the RNG's SAR
//! ADC noise source temporarily during boot, so at run time the register is
//! fed by RC oscillator jitter alone; its output is therefore conditioned
//! with SHA-256 into a running pool, and each output block is a hash of the
//! pool and a counter (the pool is ratcheted forward after every request).
//! It backs `getrandom`, which the RustCrypto provider uses.

use crate::{irq, mmio, systimer};
use core::arch::asm;
use core::cell::UnsafeCell;
use sha2::{Digest, Sha256};

/// LP_SYSTEM_REG_RNG_DATA_REG (DR_REG_LP_SYS_BASE 0x5011_0000 + 0x1A4).
const RNG_DATA: usize = 0x5011_01A4;
/// Register samples mixed into the pool per request.
const SAMPLES: usize = 32;

struct Pool(UnsafeCell<([u8; 32], u64)>);
unsafe impl Sync for Pool {}

static POOL: Pool = Pool(UnsafeCell::new(([0; 32], 0)));

fn mcycle() -> u32 {
    let v: u32;
    unsafe { asm!("csrr {0}, mcycle", out(reg) v) };
    v
}

/// Fill `out` with random bytes.
pub fn fill(out: &mut [u8]) {
    // Gather fresh input outside the critical section; the register needs
    // a little time between reads to accumulate new bits.
    let mut fresh = Sha256::new();
    for _ in 0..SAMPLES {
        fresh.update(mmio::read(RNG_DATA).to_le_bytes());
        fresh.update(mcycle().to_le_bytes());
        systimer::delay_us(1);
    }
    fresh.update(systimer::now_ticks().to_le_bytes());
    let fresh = fresh.finalize();

    let (key, base) = irq::free(|| unsafe {
        let (pool, counter) = &mut *POOL.0.get();
        let mut h = Sha256::new();
        h.update(*pool);
        h.update(fresh);
        pool.copy_from_slice(&h.finalize());
        let base = *counter;
        *counter += out.len().div_ceil(32) as u64 + 1;
        (*pool, base)
    });
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let mut h = Sha256::new();
        h.update(key);
        h.update((base + i as u64).to_le_bytes());
        chunk.copy_from_slice(&h.finalize()[..chunk.len()]);
    }
    // Ratchet the pool so the output just produced cannot be recomputed
    // from a later pool state.
    irq::free(|| unsafe {
        let (pool, _) = &mut *POOL.0.get();
        let mut h = Sha256::new();
        h.update(*pool);
        h.update(b"ratchet");
        pool.copy_from_slice(&h.finalize());
    });
}

fn getrandom_hook(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    fill(buf);
    Ok(())
}

getrandom::register_custom_getrandom!(getrandom_hook);
