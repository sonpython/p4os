//! `bench` shell command: micro-benchmarks of the kernel primitives
//! (context switch, yield, mutex, interrupt latency, heap) and of
//! OS-independent memory and CPU work, printed as a table of medians.
//!
//! The same tests, with the same parameters and output format, exist as an
//! ESP-IDF/FreeRTOS app in `bench/freertos`, so the two systems can be
//! compared on one board. Times come from `mcycle` (one CPU cycle) except
//! the alarm-to-handler latency, which is measured against the systimer
//! counter the alarm compares with (62.5 ns resolution).

use crate::sync::{Mutex, Semaphore};
use crate::{irq, kprintln, mmio, sched, soc, systimer};
use alloc::alloc::{alloc, dealloc};
use core::alloc::Layout;
use core::hint::black_box;
use core::sync::atomic::{AtomicU32, Ordering};

const RUNS: usize = 5;
/// Above every system thread, so only interrupts compete with the bench.
const PRIORITY: u8 = 10;
const STACK: usize = 8 * 1024;

const PINGPONG_ROUNDS: u32 = 10_000;
const YIELDS: u32 = 10_000;
const MUTEX_OPS: u32 = 100_000;
const IRQ_SAMPLES: u32 = 1_000;
/// Alarm lead time: long enough for the waiter to block (or start spinning).
const IRQ_LEAD_TICKS: u64 = 50 * systimer::TICKS_PER_US;
const HEAP_OPS: u32 = 10_000;
const MIB: usize = 1 << 20;
const SRAM_COPY: usize = 128 * 1024;
const CRC_BUF: usize = 64 * 1024;
const INT_ITERS: u32 = 1_000_000;
const SEED: u32 = 0x1234_5678;

/// Systimer comparator 1 (comparator 0 drives the kernel tick).
const ALARM_LINE: usize = 20;
const ALARM_LEVEL: u32 = 3;
const ST_CONF: usize = soc::SYSTIMER;
const ST_TARGET1_HI: usize = soc::SYSTIMER + 0x24;
const ST_TARGET1_LO: usize = soc::SYSTIMER + 0x28;
const ST_TARGET1_CONF: usize = soc::SYSTIMER + 0x38;
const ST_COMP1_LOAD: usize = soc::SYSTIMER + 0x54;
const ST_INT_ENA: usize = soc::SYSTIMER + 0x64;
const ST_INT_CLR: usize = soc::SYSTIMER + 0x6C;
const ST_TARGET1_WORK_EN: u32 = 1 << 23;
const ST_TARGET1_INT: u32 = 1 << 1;

static RUNNER_DONE: Semaphore = Semaphore::new(0);
static HELPER_DONE: Semaphore = Semaphore::new(0);
static PING: Semaphore = Semaphore::new(0);
static PONG: Semaphore = Semaphore::new(0);
static ISR_SEM: Semaphore = Semaphore::new(0);
static BENCH_MUTEX: Mutex<u32> = Mutex::new(0);

static CPU_MHZ: AtomicU32 = AtomicU32::new(360);
static ISR_CYCLES: AtomicU32 = AtomicU32::new(0);
static ISR_TICKS: AtomicU32 = AtomicU32::new(0);
static SPIN_CYCLES: AtomicU32 = AtomicU32::new(0);
static ENTRY_CYCLES: AtomicU32 = AtomicU32::new(0);
static FIRED: AtomicU32 = AtomicU32::new(0);
static WAKE_THREAD: AtomicU32 = AtomicU32::new(0);

#[inline(always)]
fn cycles() -> u32 {
    let v: u32;
    unsafe { core::arch::asm!("csrr {0}, mcycle", out(reg) v, options(nomem, nostack)) };
    v
}

fn cycles_to_ns(c: u32) -> f32 {
    c as f32 * 1000.0 / CPU_MHZ.load(Ordering::Relaxed) as f32
}

fn ticks_to_ns(t: u32) -> f32 {
    t as f32 * 1000.0 / systimer::TICKS_PER_US as f32
}

/// Bytes per microsecond, i.e. MB/s (10^6 bytes).
fn mb_per_s(bytes: usize, c: u32) -> f32 {
    bytes as f32 * CPU_MHZ.load(Ordering::Relaxed) as f32 / c.max(1) as f32
}

/// Entry point for the shell: run the suite on a high-priority thread and
/// wait for it, so lower-priority system threads do not disturb it.
pub fn run() {
    sched::spawn("bench", PRIORITY, STACK, runner, 0);
    RUNNER_DONE.acquire();
}

fn runner(_: usize) {
    CPU_MHZ.store(crate::measure_cpu_mhz(), Ordering::Relaxed);
    kprintln!("bench: p4os, cpu {} MHz, {} runs per test, median/min/max over runs", CPU_MHZ.load(Ordering::Relaxed), RUNS);
    kprintln!("{:<40} {:<9} {:>10} {:>10} {:>10}", "test", "unit", "median", "min", "max");

    row("ctx switch (sem ping-pong)", "ns", repeat(pingpong));
    row("yield (2 equal-prio threads)", "ns", repeat(yield_pair));
    row("mutex lock+unlock (uncontended)", "ns", repeat(mutex_pair));

    alarm_init();
    let mut idle = [[0f32; 4]; RUNS];
    let mut busy = [[0f32; 2]; RUNS];
    for r in 0..RUNS {
        idle[r] = irq_wake_run();
        busy[r] = irq_busy_run();
    }
    alarm_stop();
    row("irq latency avg (thread blocked)", "ns", column(&idle, 0));
    row("irq latency max (thread blocked)", "ns", column(&idle, 1));
    row("irq latency avg (cpu busy)", "ns", column(&busy, 0));
    row("irq entry avg (cpu busy, mcycle)", "ns", column(&busy, 1));
    row("isr->thread wake avg (semaphore)", "ns", column(&idle, 2));
    row("isr->thread wake max (semaphore)", "ns", column(&idle, 3));

    heap_row("heap alloc+free 32 B (sram)", 32, false);
    heap_row("heap alloc+free 1 KiB (sram)", 1024, false);
    heap_row("heap alloc+free 256 KiB (psram)", 256 * 1024, true);

    memcpy_rows();
    cpu_rows();
    kprintln!("kernel tick worst alarm latency since boot: {:.2} us", crate::time::max_latency_us());
    kprintln!("bench: done");
    RUNNER_DONE.release();
}

fn repeat(mut f: impl FnMut() -> f32) -> [f32; RUNS] {
    core::array::from_fn(|_| f())
}

fn column<const N: usize>(runs: &[[f32; N]; RUNS], i: usize) -> [f32; RUNS] {
    core::array::from_fn(|r| runs[r][i])
}

fn row(name: &str, unit: &str, mut v: [f32; RUNS]) {
    v.sort_unstable_by(f32::total_cmp);
    kprintln!("{:<40} {:<9} {:>10.1} {:>10.1} {:>10.1}", name, unit, v[RUNS / 2], v[0], v[RUNS - 1]);
}

// --- scheduler and synchronisation --------------------------------------

fn pong(_: usize) {
    for _ in 0..PINGPONG_ROUNDS {
        PING.acquire();
        PONG.release();
    }
    HELPER_DONE.release();
}

/// Each round trip is two context switches plus one give and one take on
/// each side; reported per switch.
fn pingpong() -> f32 {
    sched::spawn("bench-pong", PRIORITY, STACK, pong, 0);
    let t0 = cycles();
    for _ in 0..PINGPONG_ROUNDS {
        PING.release();
        PONG.acquire();
    }
    let dt = cycles().wrapping_sub(t0);
    HELPER_DONE.acquire();
    cycles_to_ns(dt) / (2 * PINGPONG_ROUNDS) as f32
}

fn yielder(_: usize) {
    for _ in 0..YIELDS {
        sched::yield_now();
    }
    HELPER_DONE.release();
}

/// Two equal-priority threads yielding to each other; every yield switches.
fn yield_pair() -> f32 {
    sched::spawn("bench-yield", PRIORITY, STACK, yielder, 0);
    let t0 = cycles();
    for _ in 0..YIELDS {
        sched::yield_now();
    }
    let dt = cycles().wrapping_sub(t0);
    HELPER_DONE.acquire();
    cycles_to_ns(dt) / (2 * YIELDS) as f32
}

fn mutex_pair() -> f32 {
    let t0 = cycles();
    for _ in 0..MUTEX_OPS {
        *BENCH_MUTEX.lock() += 1;
    }
    cycles_to_ns(cycles().wrapping_sub(t0)) / MUTEX_OPS as f32
}

// --- interrupt latency ---------------------------------------------------

fn alarm_init() {
    irq::free(|| {
        mmio::write(ST_TARGET1_CONF, 0); // one-shot, compares with unit 0
        mmio::set_bits(ST_CONF, ST_TARGET1_WORK_EN);
        mmio::write(ST_INT_CLR, ST_TARGET1_INT);
        mmio::set_bits(ST_INT_ENA, ST_TARGET1_INT);
    });
    irq::attach(soc::source::SYSTIMER_TARGET1, ALARM_LINE, ALARM_LEVEL, on_alarm);
}

fn alarm_stop() {
    irq::free(|| {
        mmio::clear_bits(ST_INT_ENA, ST_TARGET1_INT);
        mmio::clear_bits(ST_CONF, ST_TARGET1_WORK_EN);
        mmio::write(ST_INT_CLR, ST_TARGET1_INT);
    });
}

/// Arm comparator 1 `IRQ_LEAD_TICKS` ahead and return the deadline.
fn alarm_arm() -> u32 {
    let deadline = systimer::now_ticks() + IRQ_LEAD_TICKS;
    mmio::write(ST_TARGET1_HI, (deadline >> 32) as u32 & 0xF_FFFF);
    mmio::write(ST_TARGET1_LO, deadline as u32);
    mmio::write(ST_COMP1_LOAD, 1);
    deadline as u32
}

fn on_alarm() {
    let c = cycles();
    let t = systimer::now_ticks() as u32;
    ENTRY_CYCLES.store(c.wrapping_sub(SPIN_CYCLES.load(Ordering::Relaxed)), Ordering::Relaxed);
    mmio::write(ST_INT_CLR, ST_TARGET1_INT);
    ISR_CYCLES.store(c, Ordering::Relaxed);
    ISR_TICKS.store(t, Ordering::Relaxed);
    FIRED.store(1, Ordering::Release);
    if WAKE_THREAD.load(Ordering::Relaxed) != 0 {
        ISR_SEM.release();
    }
}

/// The bench thread blocks on a semaphore the alarm handler releases.
/// Returns [avg irq latency, max irq latency, avg wake, max wake] in ns.
fn irq_wake_run() -> [f32; 4] {
    WAKE_THREAD.store(1, Ordering::Relaxed);
    let (mut lat_sum, mut lat_max, mut wake_sum, mut wake_max) = (0u64, 0u32, 0u64, 0u32);
    for _ in 0..IRQ_SAMPLES {
        let deadline = alarm_arm();
        ISR_SEM.acquire();
        let woke = cycles();
        let lat = ISR_TICKS.load(Ordering::Relaxed).wrapping_sub(deadline);
        let wake = woke.wrapping_sub(ISR_CYCLES.load(Ordering::Relaxed));
        lat_sum += lat as u64;
        lat_max = lat_max.max(lat);
        wake_sum += wake as u64;
        wake_max = wake_max.max(wake);
    }
    WAKE_THREAD.store(0, Ordering::Relaxed);
    let n = IRQ_SAMPLES as f32;
    [
        ticks_to_ns(lat_sum as u32) / n,
        ticks_to_ns(lat_max),
        cycles_to_ns(wake_sum as u32) / n,
        cycles_to_ns(wake_max),
    ]
}

/// The bench thread spins storing `mcycle`; the handler subtracts the last
/// stored value from its own first `mcycle` read, giving the trap entry
/// cost from the interrupted instruction stream to the handler body.
/// Returns [avg irq latency (systimer), avg entry (mcycle)] in ns.
fn irq_busy_run() -> [f32; 2] {
    let (mut lat_sum, mut entry_sum) = (0u64, 0u64);
    for _ in 0..IRQ_SAMPLES {
        FIRED.store(0, Ordering::Relaxed);
        let deadline = alarm_arm();
        while FIRED.load(Ordering::Acquire) == 0 {
            SPIN_CYCLES.store(cycles(), Ordering::Relaxed);
        }
        lat_sum += ISR_TICKS.load(Ordering::Relaxed).wrapping_sub(deadline) as u64;
        entry_sum += ENTRY_CYCLES.load(Ordering::Relaxed) as u64;
    }
    let n = IRQ_SAMPLES as f32;
    [ticks_to_ns(lat_sum as u32) / n, cycles_to_ns(entry_sum as u32) / n]
}

// --- heap -----------------------------------------------------------------

fn heap_row(name: &str, size: usize, want_psram: bool) {
    let layout = Layout::from_size_align(size, 4).expect("valid layout");
    let probe = unsafe { alloc(layout) };
    if probe.is_null() {
        kprintln!("{:<40} allocation failed", name);
        return;
    }
    let in_psram = (crate::psram::BASE..crate::psram::BASE + 0x0400_0000).contains(&(probe as usize));
    unsafe { dealloc(probe, layout) };
    if in_psram != want_psram {
        kprintln!("{:<40} landed in the wrong memory ({:#x})", name, probe as usize);
        return;
    }
    row(name, "ns", repeat(|| {
        let t0 = cycles();
        for _ in 0..HEAP_OPS {
            let p = unsafe { alloc(layout) };
            unsafe { dealloc(black_box(p), layout) };
        }
        cycles_to_ns(cycles().wrapping_sub(t0)) / HEAP_OPS as f32
    }));
}

// --- memory and CPU kernels (OS independent) ------------------------------

/// Owned buffer from a specific memory, freed on drop.
struct Buf {
    ptr: *mut u8,
    layout: Layout,
}

impl Buf {
    fn new(len: usize, sram: bool) -> Option<Self> {
        let layout = Layout::from_size_align(len, 64).ok()?;
        let ptr = if sram { crate::heap::alloc_sram(layout) } else { unsafe { alloc(layout) } };
        if ptr.is_null() {
            return None;
        }
        let mut b = Buf { ptr, layout };
        fill_pattern(b.as_mut());
        Some(b)
    }

    fn as_ref(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }

    fn as_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

fn xorshift(x: &mut u32) -> u32 {
    *x ^= *x << 13;
    *x ^= *x >> 17;
    *x ^= *x << 5;
    *x
}

fn fill_pattern(buf: &mut [u8]) {
    let mut x = SEED;
    for b in buf.iter_mut() {
        *b = xorshift(&mut x) as u8;
    }
}

/// Copy `MIB` bytes in total from `src` to `dst` (repeating the buffer
/// when it is smaller than 1 MiB). Returns MB/s.
fn copy_mib(src: &Buf, dst: &mut Buf) -> f32 {
    let len = src.layout.size();
    let passes = MIB / len;
    let t0 = cycles();
    for _ in 0..passes {
        dst.as_mut().copy_from_slice(black_box(src.as_ref()));
        black_box(dst.ptr);
    }
    mb_per_s(passes * len, cycles().wrapping_sub(t0))
}

fn memcpy_rows() {
    match (Buf::new(SRAM_COPY, true), Buf::new(SRAM_COPY, true)) {
        (Some(src), Some(mut dst)) => row("memcpy sram->sram (128 KiB x8)", "MB/s", repeat(|| copy_mib(&src, &mut dst))),
        _ => kprintln!("memcpy sram->sram: not enough SRAM for 2 x {} KiB", SRAM_COPY / 1024),
    }
    match (Buf::new(MIB, false), Buf::new(MIB, false)) {
        (Some(src), Some(mut dst)) => row("memcpy psram->psram (1 MiB)", "MB/s", repeat(|| copy_mib(&src, &mut dst))),
        _ => kprintln!("memcpy psram->psram: not enough PSRAM"),
    }
}

fn crc32_table() -> [u32; 256] {
    core::array::from_fn(|n| {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        c
    })
}

/// Reflected CRC-32 (IEEE), raw state in and out (no pre/post inversion).
fn crc32_update(table: &[u32; 256], mut c: u32, data: &[u8]) -> u32 {
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

/// Fixed integer mix: xorshift, multiply, rotate, divide and a data
/// dependent branch, so no single instruction class dominates.
fn int_kernel(n: u32, seed: u32) -> u32 {
    let mut x = seed;
    let mut acc = 0u32;
    for i in 0..n {
        xorshift(&mut x);
        acc = acc.rotate_left(5) ^ x.wrapping_mul(0x9E37_79B9);
        acc = acc.wrapping_add(x / ((i & 0xFF) + 1));
        if x & 1 != 0 {
            acc ^= i;
        }
    }
    acc
}

fn cpu_rows() {
    let table = crc32_table();
    let mut crc = 0;
    match Buf::new(CRC_BUF, true) {
        Some(buf) => row("crc32 1 MiB (64 KiB sram x16)", "MB/s", repeat(|| {
            let t0 = cycles();
            let mut c = !0u32;
            for _ in 0..MIB / CRC_BUF {
                c = crc32_update(&table, c, black_box(buf.as_ref()));
            }
            let dt = cycles().wrapping_sub(t0);
            crc = !c;
            mb_per_s(MIB, dt)
        })),
        None => kprintln!("crc32: not enough SRAM"),
    }
    let mut sum = 0;
    row("integer kernel (per iteration)", "ns", repeat(|| {
        let t0 = cycles();
        sum = int_kernel(black_box(INT_ITERS), black_box(SEED));
        cycles_to_ns(cycles().wrapping_sub(t0)) / INT_ITERS as f32
    }));
    kprintln!("checks: crc32={:#010x} int={:#010x}", crc, sum);
}
