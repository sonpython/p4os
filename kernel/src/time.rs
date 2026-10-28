//! Kernel tick built on the systimer alarm, with tickless idle.
//!
//! Tick `k` falls at `EPOCH + k * TICK_PERIOD` on the systimer, which runs
//! from the XTAL and is unaffected by CPU frequency changes. The alarm is
//! always armed on that grid, so interrupt latency never accumulates as
//! drift, and ticks missed while interrupts were masked are counted from
//! the grid when the handler runs. The alarm interrupt is attached on
//! core 0 and drives scheduling for both cores.
//!
//! Tickless idle: when every core runs its idle thread, `stop` moves the
//! alarm to the tick the earliest sleeper is due (at most `MAX_IDLE_TICKS`
//! ahead). Meanwhile `ticks()` reads the grid position straight from the
//! systimer. The periodic tick comes back either when that alarm fires or
//! when a core reschedules (`resume`, called by the scheduler); both
//! return the ticks that passed so the scheduler can account for them.

use crate::spin::SpinLock;
use crate::{irq, sched, soc, systimer};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const TICK_HZ: u64 = 1000;
const TICK_PERIOD: u64 = systimer::TICKS_PER_US * 1_000_000 / TICK_HZ;
const LINE: usize = 16;
const LEVEL: u32 = 3;
/// Longest stretch without a tick while idle, so per-second statistics
/// (CPU load) keep updating.
const MAX_IDLE_TICKS: u64 = 1000;

/// Comparator state. Lock order: the scheduler lock, then this one.
struct Alarm {
    /// Systimer value the comparator is set to, and its grid tick.
    deadline: u64,
    armed: u64,
    /// Last grid tick the scheduler has been told about.
    accounted: u64,
}

static ALARM: SpinLock<Alarm> = SpinLock::new(Alarm { deadline: 0, armed: 0, accounted: 0 });
/// Systimer value of tick 0, written once by `init` before interrupts run.
static EPOCH_LO: AtomicU32 = AtomicU32::new(0);
static EPOCH_HI: AtomicU32 = AtomicU32::new(0);
/// Low 32 bits of `Alarm::accounted`, readable without the lock.
static TICKS: AtomicU32 = AtomicU32::new(0);
/// The periodic tick is stopped for tickless idle.
static STOPPED: AtomicBool = AtomicBool::new(false);
static MAX_LATENCY_TICKS: AtomicU32 = AtomicU32::new(0);
/// Tick interrupts taken, and times the tick was stopped (statistics).
static ALARM_IRQS: AtomicU32 = AtomicU32::new(0);
static STOPS: AtomicU32 = AtomicU32::new(0);

fn epoch() -> u64 {
    ((EPOCH_HI.load(Ordering::Relaxed) as u64) << 32) | EPOCH_LO.load(Ordering::Relaxed) as u64
}

/// Grid ticks elapsed at systimer value `now`.
fn grid(now: u64) -> u64 {
    now.saturating_sub(epoch()) / TICK_PERIOD
}

/// Arm the comparator for grid tick `tick`, or the first later one still
/// ahead: it only fires when the counter equals the comparator, so a
/// deadline already in the past would never fire.
fn arm(a: &mut Alarm, mut tick: u64) {
    loop {
        let deadline = epoch() + tick * TICK_PERIOD;
        systimer::alarm_set(deadline);
        // Leave a few microseconds of margin for the write to land.
        if systimer::now_ticks() + 4 * systimer::TICKS_PER_US < deadline {
            a.deadline = deadline;
            a.armed = tick;
            return;
        }
        tick += 1;
    }
}

/// Mark every grid tick up to `now_tick` as handled; returns how many
/// were not yet accounted for.
fn catch_up(a: &mut Alarm, now_tick: u64) -> u32 {
    let elapsed = now_tick.saturating_sub(a.accounted);
    a.accounted = a.accounted.max(now_tick);
    TICKS.store(a.accounted as u32, Ordering::Release);
    elapsed.min(u32::MAX as u64) as u32
}

fn on_alarm() {
    ALARM_IRQS.fetch_add(1, Ordering::Relaxed);
    let (now_tick, elapsed) = {
        let mut a = ALARM.lock();
        let now = systimer::now_ticks();
        systimer::alarm_ack();
        let late = now.wrapping_sub(a.deadline);
        if now >= a.deadline {
            MAX_LATENCY_TICKS.fetch_max(late as u32, Ordering::Relaxed);
        }
        // Usually the armed tick itself; the division only after a stall.
        let now_tick = if now >= a.deadline && late < TICK_PERIOD { a.armed } else { grid(now) };
        let elapsed = catch_up(&mut a, now_tick);
        arm(&mut a, now_tick + 1);
        STOPPED.store(false, Ordering::Release);
        (now_tick as u32, elapsed)
    };
    if elapsed > 0 {
        sched::on_tick(now_tick, elapsed);
    }
}

pub fn init() {
    systimer::alarm_init();
    let now = systimer::now_ticks();
    EPOCH_LO.store(now as u32, Ordering::Relaxed);
    EPOCH_HI.store((now >> 32) as u32, Ordering::Relaxed);
    arm(&mut ALARM.lock(), 1);
    irq::attach(soc::source::SYSTIMER_TARGET0, LINE, LEVEL, on_alarm);
}

/// Re-arm the tick after a long stretch with interrupts masked (boot-time
/// driver init), so the comparator is not already in the past.
pub fn resync() {
    let mut a = ALARM.lock();
    let now_tick = grid(systimer::now_ticks());
    catch_up(&mut a, now_tick);
    arm(&mut a, now_tick + 1);
}

/// Stop the periodic tick until tick `wake` (the earliest sleeper; `None`
/// if nothing sleeps). Called by the scheduler with its lock held, once it
/// has checked that every core is idle. Returns false when the next tick
/// is due too soon (or still unhandled) for stopping to help.
pub fn stop(wake: Option<u32>) -> bool {
    let mut a = ALARM.lock();
    let now_tick = grid(systimer::now_ticks());
    if a.accounted < now_tick {
        return false; // a tick is pending; let its handler run first
    }
    let ahead = match wake {
        Some(w) => (w.wrapping_sub(now_tick as u32) as i32).max(0) as u64,
        None => MAX_IDLE_TICKS,
    };
    let ahead = ahead.min(MAX_IDLE_TICKS);
    if ahead < 2 {
        return false;
    }
    arm(&mut a, now_tick + ahead);
    if !STOPPED.swap(true, Ordering::AcqRel) {
        STOPS.fetch_add(1, Ordering::Relaxed);
    }
    true
}

/// Restart the periodic tick if it was stopped. Returns `(now, elapsed)`:
/// the current tick and the ticks that passed unaccounted, which the
/// caller (the scheduler, under its lock) must account for.
pub fn resume() -> Option<(u32, u32)> {
    if !STOPPED.load(Ordering::Acquire) {
        return None;
    }
    let mut a = ALARM.lock();
    if !STOPPED.load(Ordering::Acquire) {
        return None;
    }
    let now_tick = grid(systimer::now_ticks());
    let elapsed = catch_up(&mut a, now_tick);
    arm(&mut a, now_tick + 1);
    STOPPED.store(false, Ordering::Release);
    Some((now_tick as u32, elapsed))
}

/// Milliseconds since boot (wraps after ~49 days).
pub fn ticks() -> u32 {
    if STOPPED.load(Ordering::Acquire) {
        grid(systimer::now_ticks()) as u32
    } else {
        TICKS.load(Ordering::Acquire)
    }
}

/// Worst observed delay between an alarm deadline and its handler, in µs.
pub fn max_latency_us() -> f32 {
    MAX_LATENCY_TICKS.load(Ordering::Relaxed) as f32 / systimer::TICKS_PER_US as f32
}

/// (tick interrupts taken, times the tick was stopped) since boot.
pub fn stats() -> (u32, u32) {
    (ALARM_IRQS.load(Ordering::Relaxed), STOPS.load(Ordering::Relaxed))
}

/// Early-boot sleep (before the scheduler runs): halt in `wfi` until `ms` ticks have elapsed.
pub fn sleep_ms(ms: u32) {
    let end = ticks().wrapping_add(ms);
    while (end.wrapping_sub(ticks()) as i32) > 0 {
        unsafe { core::arch::asm!("wfi") };
    }
}
