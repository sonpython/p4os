//! System calls of user-mode apps (ABI in `p4os_abi`). They run in the
//! app's thread, in M-mode on its kernel stack, with interrupts enabled.
//! `exit` is handled by `usermode` because it never returns.

use crate::usermode::UserApp;
use crate::{kprint, sched, time};
use p4os_abi::{FD_CONSOLE, err, nr};

/// Serve syscall `number` and return the value for the app's a0.
pub fn dispatch(app: &UserApp, number: u32, args: [u32; 6]) -> i32 {
    match number {
        nr::WRITE => write(app, args[0], args[1], args[2]),
        nr::SLEEP_MS => {
            sched::sleep_ms(args[0]);
            0
        }
        nr::UPTIME_MS => time::ticks() as i32,
        nr::YIELD => {
            sched::yield_now();
            0
        }
        nr::GETPID => sched::current_id().map_or(err::EINVAL, |id| id as i32),
        _ => err::ENOSYS,
    }
}

fn write(app: &UserApp, fd: u32, ptr: u32, len: u32) -> i32 {
    if fd != FD_CONSOLE {
        return err::EBADF;
    }
    // Every byte must lie inside the app's own memory; this is the only
    // place user pointers are dereferenced.
    let Some(bytes) = app.user_bytes(ptr, len) else {
        return err::EFAULT;
    };
    for chunk in bytes.utf8_chunks() {
        kprint!("{}", chunk.valid());
        if !chunk.invalid().is_empty() {
            kprint!("\u{FFFD}");
        }
    }
    // An app region is far below 2 GiB, so the length fits.
    bytes.len() as i32
}
