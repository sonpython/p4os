//! Isolation test: touches memory outside the app's own region. The kernel
//! must kill the app with an access fault and keep running. The `run`
//! argument picks the attempt:
//!
//! | arg | attempt                                   | expected        |
//! |-----|-------------------------------------------|-----------------|
//! | 0   | store to kernel SRAM                      | store fault     |
//! | 1   | store to a GPIO register                  | store fault     |
//! | 2   | load from kernel SRAM                     | load fault      |
//! | 3   | jump into kernel code                     | fetch fault     |
//! | 4   | store into the app's own code             | store fault     |
//! | 5   | load kernel rodata (flash window)         | load fault      |
//! | 6   | hand a kernel pointer to `write`          | EFAULT, exit 0  |
//!
//! Not tested because they are known to be reachable: the ranges of PMP
//! entries the bootloader locks ahead of the app entries (CPU subsystem with
//! the CLIC, CPU peripherals, mask ROM); see kernel/src/pmp.rs.

#![no_std]
#![no_main]

use p4os_user::{err, nr, println, raw_syscall};

p4os_user::entry!(main);

/// Start of the kernel's .data in HP L2 SRAM.
const KERNEL_SRAM: usize = 0x4FF0_0000;
/// GPIO_OUT_W1TS: writing 0 would be harmless even if it got through.
const GPIO_OUT_W1TS: usize = 0x500E_0008;
/// Kernel .text in the flash-mapped window.
const KERNEL_TEXT: usize = 0x4100_0040;
/// Kernel read-only data (app descriptor) in the flash-mapped window.
const KERNEL_RODATA: usize = 0x4000_0020;

fn main(arg: u32) -> i32 {
    println!("crash test {}: about to break isolation", arg);
    unsafe {
        match arg {
            0 => (KERNEL_SRAM as *mut u32).write_volatile(0xDEAD_BEEF),
            1 => (GPIO_OUT_W1TS as *mut u32).write_volatile(0),
            2 => println!("read {:#x}", (KERNEL_SRAM as *const u32).read_volatile()),
            3 => {
                let f: extern "C" fn() = core::mem::transmute(KERNEL_TEXT);
                f();
            }
            4 => {
                let own_code = main as fn(u32) -> i32 as usize as *mut u16;
                own_code.write_volatile(0);
            }
            5 => println!("read {:#x}", (KERNEL_RODATA as *const u32).read_volatile()),
            6 => {
                let r = raw_syscall(nr::WRITE, 1, KERNEL_SRAM as u32, 16);
                println!("write(kernel pointer) = {} (expected {})", r, err::EFAULT);
                return if r == err::EFAULT { 0 } else { 1 };
            }
            _ => {
                println!("unknown test");
                return 2;
            }
        }
    }
    println!("isolation FAILED: the access went through");
    1
}
