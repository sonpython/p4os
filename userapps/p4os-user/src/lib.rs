//! Runtime for p4os user-mode apps.
//!
//! An app is a `no_std`, `no_main` binary that names its entry function with
//! [`entry!`]. It runs in RISC-V U-mode and can only reach its own memory;
//! everything else goes through the syscalls wrapped here.
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//! p4os_user::entry!(main);
//! fn main(arg: u32) -> i32 {
//!     p4os_user::println!("hello from user mode, arg {}", arg);
//!     0
//! }
//! ```

#![no_std]

use core::arch::{asm, global_asm};
use core::fmt;

pub use p4os_abi::{err, nr};

// The kernel enters at `_start` with sp at the top of the app stack and the
// `run` argument in a0. Returning from the app's main exits with its value.
global_asm!(
    r#"
    .section .text.start, "ax"
    .global _start
_start:
    call __p4os_main
    li a7, {exit}
    ecall
1:  j 1b
"#,
    exit = const nr::EXIT,
);

/// Define the app entry point: `fn main(arg: u32) -> i32`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        extern "C" fn __p4os_main(arg: u32) -> i32 {
            let main: fn(u32) -> i32 = $main;
            main(arg)
        }
    };
}

#[inline(always)]
fn syscall3(n: u32, a0: u32, a1: u32, a2: u32) -> i32 {
    let ret: i32;
    // The kernel preserves every register except a0, but treat the call
    // like a function call so no assumption about FP state leaks into apps.
    unsafe {
        asm!("ecall", in("a7") n, inlateout("a0") a0 => ret, in("a1") a1, in("a2") a2,
             clobber_abi("C"), options(nostack));
    }
    ret
}

/// End the app with `code`.
pub fn exit(code: i32) -> ! {
    syscall3(nr::EXIT, code as u32, 0, 0);
    unreachable!("exit returned")
}

/// Write bytes to a file descriptor (1 = console). Returns the number of
/// bytes written or a negative error.
pub fn write(fd: u32, buf: &[u8]) -> i32 {
    syscall3(nr::WRITE, fd, buf.as_ptr() as u32, buf.len() as u32)
}

pub fn sleep_ms(ms: u32) {
    syscall3(nr::SLEEP_MS, ms, 0, 0);
}

/// Milliseconds since boot (wraps after about 49 days).
pub fn uptime_ms() -> u32 {
    syscall3(nr::UPTIME_MS, 0, 0, 0) as u32
}

pub fn yield_now() {
    syscall3(nr::YIELD, 0, 0, 0);
}

/// The kernel thread id running this app.
pub fn getpid() -> u32 {
    syscall3(nr::GETPID, 0, 0, 0) as u32
}

/// Raw syscall, for testing how the kernel treats bad arguments.
pub fn raw_syscall(n: u32, a0: u32, a1: u32, a2: u32) -> i32 {
    syscall3(n, a0, a1, a2)
}

/// Console writer behind [`print!`].
pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if write(p4os_abi::FD_CONSOLE, s.as_bytes()) < 0 { Err(fmt::Error) } else { Ok(()) }
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        let _ = core::fmt::Write::write_fmt(&mut $crate::Console, format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}

/// Exit code reported when an app panics.
pub const PANIC_EXIT: i32 = 101;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("panic: {}", info);
    exit(PANIC_EXIT)
}
