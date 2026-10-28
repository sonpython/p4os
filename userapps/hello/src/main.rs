//! First p4os user-mode app: prints through the console syscall, sleeps,
//! burns some CPU (to be preempted like any thread) and exits cleanly.

#![no_std]
#![no_main]

use p4os_user::{getpid, println, sleep_ms, uptime_ms, yield_now};

p4os_user::entry!(main);

fn main(arg: u32) -> i32 {
    let pid = getpid();
    println!("hello from user mode! pid {} arg {}", pid, arg);
    let main_addr = main as fn(u32) -> i32 as usize;
    println!("running at {:#010x} (loaded wherever the kernel chose)", main_addr);

    let t0 = uptime_ms();
    sleep_ms(100);
    println!("slept {} ms", uptime_ms().wrapping_sub(t0));

    // Busy work for ~200 ms: the timer still preempts us.
    let t1 = uptime_ms();
    let mut primes = 0u32;
    let mut n = 2u32;
    while uptime_ms().wrapping_sub(t1) < 200 {
        if (2..n).take_while(|d| d * d <= n).all(|d| n % d != 0) {
            primes += 1;
        }
        n += 1;
        if n % 1024 == 0 {
            yield_now();
        }
    }
    println!("found {} primes below {} in 200 ms", primes, n);

    // Float use exercises the per-thread FP context.
    let density = primes as f32 * 100.0 / n as f32;
    println!("prime density {:.2}%", density);
    println!("bye");
    0
}
