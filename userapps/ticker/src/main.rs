//! Example user-mode app: a ticker that uses every syscall of the ABI.
//!
//! `run SLOT [N]` prints N ticks (default 5, at most 60), one every 500 ms,
//! with how late each wake-up was, then shows how a syscall reports an
//! error and exits with code 0. Build, flash and run it as described in
//! docs/app-development.md.

#![no_std]
#![no_main]

use p4os_user::{err, exit, getpid, println, sleep_ms, uptime_ms, write, yield_now};

// The kernel enters `_start` (in p4os-user), which calls this function with
// the `run` argument and exits with its return value.
p4os_user::entry!(main);

const PERIOD_MS: u32 = 500;
const DEFAULT_TICKS: u32 = 5;
const MAX_TICKS: u32 = 60;

fn main(arg: u32) -> i32 {
    // Validate input at the boundary: the argument comes from whoever typed
    // the `run` command.
    let ticks = match arg {
        0 => DEFAULT_TICKS,
        n => n.min(MAX_TICKS),
    };
    println!("ticker: pid {}, {} ticks every {} ms", getpid(), ticks, PERIOD_MS);

    let start = uptime_ms();
    let mut worst_late = 0;
    for i in 1..=ticks {
        // Sleep until the next multiple of the period, so small delays do
        // not add up over the run. `uptime_ms` wraps after ~49 days, hence
        // the wrapping arithmetic.
        let target = PERIOD_MS * i;
        let now = uptime_ms().wrapping_sub(start);
        if target > now {
            sleep_ms(target - now);
        }
        let at = uptime_ms().wrapping_sub(start);
        let late = at.saturating_sub(target);
        worst_late = worst_late.max(late);
        println!("tick {:>2} at {:>5} ms (+{} ms)", i, at, late);
    }
    println!("ticker: worst wake-up delay {} ms", worst_late);

    // Syscalls report errors as negative return values (`p4os_abi::err`).
    // There is no file descriptor 2, so this fails with EBADF.
    let r = write(2, b"to stderr?\n");
    if r == err::EBADF {
        println!("ticker: write to fd 2 failed with EBADF, as documented");
    } else {
        println!("ticker: unexpected result {} from write to fd 2", r);
        exit(1);
    }

    // Give other threads a turn before finishing.
    yield_now();
    println!("ticker: done");
    0
}
