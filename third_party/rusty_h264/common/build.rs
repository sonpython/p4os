//! Vendored copy: upstream turns on the `accel` cfgs (SIMD kernels from the
//! rusty_h264-accel crate) for x86-64 and aarch64 when the `asm` feature is
//! set. That crate is not vendored, so the cfgs are only declared here and
//! the codec always uses its portable scalar Rust path.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(accel)");
    println!("cargo::rustc-check-cfg=cfg(accel_x86)");
}
