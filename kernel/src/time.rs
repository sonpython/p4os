//! Kernel tick built on the systimer alarm.
//!
//! The alarm is re-armed relative to the previous deadline rather than to
//! "now", so interrupt latency does not accumulate as drift.

use crate::{irq, sched, soc, systimer};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

pub const TICK_HZ: u64 = 1000;
const TICK_PERIOD: u64 = systimer::TICKS_PER_US * 1_000_000 / TICK_HZ;
const LINE: usize = 16;
const LEVEL: u32 = 3;

/// Next alarm deadline. RV32 has no 64-bit atomics; this is only touched by
/// `init` (before the interrupt is attached) and by the alarm handler.
struct Deadline(UnsafeCell<u64>);
unsafe impl Sync for Deadline {}
static NEXT_DEADLINE: Deadline = Deadline(UnsafeCell::new(0));
static TICKS: AtomicU32 = AtomicU32::new(0);
static MAX_LATENCY_TICKS: AtomicU32 = AtomicU32::new(0);

/// Program the next deadline. The comparator only fires when the counter
/// equals the target, so a deadline already in the past would never fire;
/// skip whole periods until it lies ahead and return how many were skipped.
fn arm_after(mut deadline: u64) -> u32 {
    let mut skipped = 0;
    loop {
        systimer::alarm_set(deadline);
        // Leave a few microseconds of margin for the write to land.
        if systimer::now_ticks() + 4 * systimer::TICKS_PER_US < deadline {
            unsafe { *NEXT_DEADLINE.0.get() = deadline };
            return skipped;
        }
        deadline += TICK_PERIOD;
        skipped += 1;
    }
}

fn on_alarm() {
    let now = systimer::now_ticks();
    systimer::alarm_ack();
    let deadline = unsafe { *NEXT_DEADLINE.0.get() };
    let latency = now.saturating_sub(deadline) as u32;
    MAX_LATENCY_TICKS.fetch_max(latency, Ordering::Relaxed);

    // Ticks missed while interrupts were masked still count as elapsed time.
    let missed = arm_after(deadline + TICK_PERIOD);
    TICKS.fetch_add(missed, Ordering::Relaxed);
    let now_tick = TICKS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if sched::on_tick(now_tick) {
        sched::request_resched();
    }
}

pub fn init() {
    systimer::alarm_init();
    arm_after(systimer::now_ticks() + TICK_PERIOD);
    irq::attach(soc::source::SYSTIMER_TARGET0, LINE, LEVEL, on_alarm);
}

/// Re-arm the tick after a long stretch with interrupts masked (boot-time
/// driver init), so the comparator target is not already in the past.
pub fn resync() {
    irq::free(|| {
        let deadline = unsafe { *NEXT_DEADLINE.0.get() };
        let missed = arm_after(deadline);
        TICKS.fetch_add(missed, Ordering::Relaxed);
    });
}

pub fn ticks() -> u32 {
    TICKS.load(Ordering::Relaxed)
}

/// Worst observed delay between an alarm deadline and its handler, in µs.
pub fn max_latency_us() -> f32 {
    MAX_LATENCY_TICKS.load(Ordering::Relaxed) as f32 / systimer::TICKS_PER_US as f32
}

/// Early-boot sleep (before the scheduler runs): halt in `wfi` until `ms` ticks have elapsed.
pub fn sleep_ms(ms: u32) {
    let end = ticks().wrapping_add(ms);
    while (end.wrapping_sub(ticks()) as i32) > 0 {
        unsafe { core::arch::asm!("wfi") };
    }
}
