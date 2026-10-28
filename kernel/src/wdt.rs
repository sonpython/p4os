//! Hardware watchdogs and the supervisor thread that feeds them.
//!
//! - RWDT (LP watchdog): armed at boot for 15 s, stage 0 resets the HP
//!   system (LP RAM, and with it the crash report, survives), stage 1 the
//!   whole chip 5 s later. Covers boot hangs and anything that stops the
//!   supervisor without the MWDT noticing.
//! - MWDT0 (timer group 0): armed when the supervisor starts. Stage 0 after
//!   5 s interrupts both cores; whichever takes it records a crash report
//!   and resets. Stage 1, 2 s later, resets the HP system if neither core
//!   can take interrupts (the next boot reports the reset reason).
//! - MWDT1: off.
//! - Super watchdog: left in auto-feed. Its fixed timeout is about a
//!   second, shorter than legitimate stretches where the supervisor waits
//!   for CPU time (`smpdemo` keeps both cores busy at a higher priority for
//!   over a second), and it would reset without a report.
//!
//! The supervisor runs at priority 1, just above idle, every 500 ms. It
//! feeds both watchdogs, checks that every registered `Watch` checked in
//! within its timeout and that each online core still takes interrupts.
//! A thread starved of CPU for 5 s therefore trips the MWDT, a hung
//! critical thread or a core stuck with interrupts masked is reported by
//! the supervisor. Long but legitimate work inside a watched thread runs
//! under `busy`, which suspends its deadline. Flash erases mask interrupts
//! for about 45 ms and park the other core, far below every timeout.

use crate::spin::SpinLock;
use crate::{fault, irq, kprintln, mmio, sched, smp, soc, time};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

const WKEY: u32 = 0x50D8_3AA1;

const LP_WDT_CONFIG0: usize = soc::LP_WDT;
const LP_WDT_CONFIG1: usize = soc::LP_WDT + 0x04;
const LP_WDT_CONFIG2: usize = soc::LP_WDT + 0x08;
const LP_WDT_FEED: usize = soc::LP_WDT + 0x14;
const LP_WDT_WPROTECT: usize = soc::LP_WDT + 0x18;
const LP_WDT_SWD_CONFIG: usize = soc::LP_WDT + 0x1C;
const LP_WDT_SWD_WPROTECT: usize = soc::LP_WDT + 0x20;
const LP_WDT_EN: u32 = 1 << 31;
const LP_WDT_STG0_SHIFT: u32 = 28;
const LP_WDT_STG1_SHIFT: u32 = 25;
/// Reset signal lengths, kept as the ROM set them.
const LP_WDT_LENGTHS: u32 = 0x3F << 13;
const LP_WDT_FEED_BIT: u32 = 1 << 31;
const SWD_AUTO_FEED_EN: u32 = 1 << 18;
/// RC_SLOW_CLK, approximately (SOC_CLK_RC_SLOW_FREQ_APPROX).
const SLOW_CLK_HZ: u32 = 136_000;

const TIMG_WDTCONFIG0: usize = 0x48;
const TIMG_WDTCONFIG1: usize = 0x4C;
const TIMG_WDTCONFIG2: usize = 0x50;
const TIMG_WDTCONFIG3: usize = 0x54;
const TIMG_WDTFEED: usize = 0x60;
const TIMG_WDTWPROTECT: usize = 0x64;
const TIMG_INT_ENA: usize = 0x70;
const TIMG_INT_CLR: usize = 0x7C;
const TIMG_WDT_EN: u32 = 1 << 31;
const TIMG_WDT_STG0_SHIFT: u32 = 29;
const TIMG_WDT_STG1_SHIFT: u32 = 27;
const TIMG_WDT_CONF_UPDATE: u32 = 1 << 22;
const TIMG_WDT_LENGTHS: u32 = 0x3F << 15;
const TIMG_WDT_PRESCALE_SHIFT: u32 = 16;
const TIMG_WDT_DIVCNT_RST: u32 = 1 << 0;
const TIMG_WDT_INT: u32 = 1 << 2;
/// The MWDT counts XTAL (40 MHz) / 20000: one tick per 500 µs.
const MWDT_PRESCALE: u32 = 20_000;
const MWDT_TICKS_PER_MS: u32 = 2;

const ACTION_INT: u32 = 1;
const ACTION_RESET_SYSTEM: u32 = 3;
const ACTION_RESET_RTC: u32 = 4;

pub const MWDT_TIMEOUT_MS: u32 = 5_000;
const MWDT_RESET_AFTER_MS: u32 = 2_000;
const RWDT_TIMEOUT_MS: u32 = 15_000;
const RWDT_CHIP_RESET_AFTER_MS: u32 = 5_000;

/// CLIC line for the MWDT0 interrupt on both cores, highest level.
const MWDT_LINE: usize = 46;
const MWDT_LEVEL: u32 = 7;

const SUPERVISOR_PERIOD_MS: u32 = 500;
/// A core that takes no trap for this long, even after being sent IPIs,
/// has interrupts masked for good.
const CORE_STUCK_MS: u32 = 3_000;

fn lp_unlocked(f: impl FnOnce()) {
    mmio::write(LP_WDT_WPROTECT, WKEY);
    f();
    mmio::write(LP_WDT_WPROTECT, 0);
}

fn timg_unlocked(base: usize, f: impl FnOnce()) {
    mmio::write(base + TIMG_WDTWPROTECT, WKEY);
    f();
    mmio::write(base + TIMG_WDTWPROTECT, 0);
}

/// Boot: take over from the ROM and bootloader. Both timer-group
/// watchdogs off, the super watchdog in auto-feed, the RWDT re-armed with
/// our own timeout so a hang before the supervisor runs still resets.
pub fn init() {
    mmio::write(LP_WDT_SWD_WPROTECT, WKEY);
    mmio::write(LP_WDT_SWD_CONFIG, mmio::read(LP_WDT_SWD_CONFIG) | SWD_AUTO_FEED_EN);
    mmio::write(LP_WDT_SWD_WPROTECT, 0);

    for base in [soc::TIMG0, soc::TIMG1] {
        timg_unlocked(base, || {
            let lengths = mmio::read(base + TIMG_WDTCONFIG0) & TIMG_WDT_LENGTHS;
            mmio::write(base + TIMG_WDTCONFIG0, lengths | TIMG_WDT_CONF_UPDATE);
        });
    }

    let ms = |ms: u32| (ms as u64 * SLOW_CLK_HZ as u64 / 1000) as u32;
    lp_unlocked(|| {
        let lengths = mmio::read(LP_WDT_CONFIG0) & LP_WDT_LENGTHS;
        mmio::write(LP_WDT_CONFIG0, lengths);
        mmio::write(LP_WDT_CONFIG1, ms(RWDT_TIMEOUT_MS));
        mmio::write(LP_WDT_CONFIG2, ms(RWDT_CHIP_RESET_AFTER_MS));
        mmio::write(LP_WDT_FEED, LP_WDT_FEED_BIT);
        mmio::write(
            LP_WDT_CONFIG0,
            lengths
                | LP_WDT_EN
                | (ACTION_RESET_SYSTEM << LP_WDT_STG0_SHIFT)
                | (ACTION_RESET_RTC << LP_WDT_STG1_SHIFT),
        );
    });
}

/// Route the MWDT interrupt to the calling core. Run on both cores.
pub fn init_core() {
    irq::attach(soc::source::TG0_WDT, MWDT_LINE, MWDT_LEVEL, on_mwdt);
}

fn arm_mwdt() {
    let base = soc::TIMG0;
    timg_unlocked(base, || {
        let lengths = mmio::read(base + TIMG_WDTCONFIG0) & TIMG_WDT_LENGTHS;
        mmio::write(base + TIMG_WDTCONFIG0, lengths);
        mmio::write(base + TIMG_WDTCONFIG1, (MWDT_PRESCALE << TIMG_WDT_PRESCALE_SHIFT) | TIMG_WDT_DIVCNT_RST);
        mmio::write(base + TIMG_WDTCONFIG2, MWDT_TIMEOUT_MS * MWDT_TICKS_PER_MS);
        mmio::write(base + TIMG_WDTCONFIG3, MWDT_RESET_AFTER_MS * MWDT_TICKS_PER_MS);
        mmio::write(base + TIMG_INT_CLR, TIMG_WDT_INT);
        mmio::write(base + TIMG_INT_ENA, mmio::read(base + TIMG_INT_ENA) | TIMG_WDT_INT);
        mmio::write(base + TIMG_WDTFEED, 1);
        mmio::write(
            base + TIMG_WDTCONFIG0,
            lengths
                | TIMG_WDT_EN
                | (ACTION_INT << TIMG_WDT_STG0_SHIFT)
                | (ACTION_RESET_SYSTEM << TIMG_WDT_STG1_SHIFT)
                | TIMG_WDT_CONF_UPDATE,
        );
    });
}

/// Restart both watchdog countdowns. Plain register writes only, so the
/// crash path may call it too.
pub fn feed() {
    timg_unlocked(soc::TIMG0, || mmio::write(soc::TIMG0 + TIMG_WDTFEED, 1));
    lp_unlocked(|| mmio::write(LP_WDT_FEED, LP_WDT_FEED_BIT));
}

/// MWDT stage 0: the supervisor has not fed it for `MWDT_TIMEOUT_MS`.
fn on_mwdt() {
    fault::watchdog_expired()
}

const MAX_WATCHES: usize = 8;

struct WatchInfo {
    name: &'static str,
    timeout_ms: u32,
}

static WATCHES: SpinLock<[Option<WatchInfo>; MAX_WATCHES]> = SpinLock::new([const { None }; MAX_WATCHES]);
static LAST_CHECKIN: [AtomicU32; MAX_WATCHES] = [const { AtomicU32::new(0) }; MAX_WATCHES];
/// Nesting depth of `busy` sections per watch.
static BUSY: [AtomicU32; MAX_WATCHES] = [const { AtomicU32::new(0) }; MAX_WATCHES];
/// Test hook (`crash hang`): watch slot + 1 whose thread should hang at
/// its next check-in, 0 for none.
static HANG: AtomicUsize = AtomicUsize::new(0);

/// A thread's promise to call `checkin` at least every `timeout_ms`.
pub struct Watch {
    slot: usize,
}

/// Register the calling thread as critical. `name` must be its thread name
/// (that is how `busy` finds the watch).
pub fn watch(name: &'static str, timeout_ms: u32) -> Watch {
    let mut w = WATCHES.lock();
    let slot = w.iter().position(Option::is_none).expect("too many watched threads");
    LAST_CHECKIN[slot].store(time::ticks(), Ordering::Relaxed);
    BUSY[slot].store(0, Ordering::Relaxed);
    w[slot] = Some(WatchInfo { name, timeout_ms });
    Watch { slot }
}

impl Drop for Watch {
    fn drop(&mut self) {
        WATCHES.lock()[self.slot] = None;
    }
}

impl Watch {
    pub fn checkin(&self) {
        LAST_CHECKIN[self.slot].store(time::ticks(), Ordering::Relaxed);
        if HANG.load(Ordering::Relaxed) == self.slot + 1 {
            HANG.store(0, Ordering::Relaxed);
            kprintln!("wdt: thread hangs on request (crash hang); the supervisor should reset");
            loop {
                sched::sleep_ms(1000);
            }
        }
    }
}

/// Run long but legitimate work (a shell command from the Terminal app,
/// formatting the SD card...) without the calling thread's deadline
/// running out. The hardware watchdog stays armed: the supervisor keeps
/// feeding it as long as it gets CPU time.
pub fn busy<R>(f: impl FnOnce() -> R) -> R {
    let slot = fault::current_thread().and_then(|me| {
        WATCHES.lock().iter().position(|w| w.as_ref().is_some_and(|w| w.name == me))
    });
    if let Some(i) = slot {
        BUSY[i].fetch_add(1, Ordering::AcqRel);
    }
    let r = f();
    if let Some(i) = slot {
        LAST_CHECKIN[i].store(time::ticks(), Ordering::Relaxed);
        BUSY[i].fetch_sub(1, Ordering::AcqRel);
    }
    r
}

/// Make the watched thread `name` stop checking in (test hook).
pub fn request_hang(name: &str) -> Result<u32, ()> {
    let w = WATCHES.lock();
    let slot = w.iter().position(|w| w.as_ref().is_some_and(|w| w.name == name)).ok_or(())?;
    HANG.store(slot + 1, Ordering::Relaxed);
    Ok(w[slot].as_ref().map_or(0, |w| w.timeout_ms))
}

/// Names and timeouts of the watched threads, for `crash`.
pub fn watched() -> alloc::vec::Vec<(&'static str, u32)> {
    WATCHES.lock().iter().flatten().map(|w| (w.name, w.timeout_ms)).collect()
}

pub fn start_supervisor() {
    sched::spawn("supervisor", 1, 4096, supervisor, 0);
}

fn supervisor(_: usize) {
    arm_mwdt();
    kprintln!(
        "wdt: MWDT0 {} s (+{} s reset), RWDT {} s, supervisor every {} ms",
        MWDT_TIMEOUT_MS / 1000,
        MWDT_RESET_AFTER_MS / 1000,
        RWDT_TIMEOUT_MS / 1000,
        SUPERVISOR_PERIOD_MS
    );
    let mut cores = [CoreProgress::default(); irq::HART_COUNT];
    let mut last_round = time::ticks();
    loop {
        feed();
        let now = time::ticks();
        // After the supervisor itself waited long for CPU, the cores'
        // trap counts say nothing about the time in between: start over.
        if now.wrapping_sub(last_round) > 2 * SUPERVISOR_PERIOD_MS {
            cores = [CoreProgress::default(); irq::HART_COUNT];
        }
        last_round = now;
        check_watches();
        for (hart, progress) in cores.iter_mut().enumerate() {
            progress.check(hart);
        }
        sched::sleep_ms(SUPERVISOR_PERIOD_MS);
    }
}

fn check_watches() {
    let now = time::ticks();
    let mut late: Option<(&'static str, u32, u32)> = None;
    {
        let w = WATCHES.lock();
        for (i, info) in w.iter().enumerate() {
            let Some(info) = info else { continue };
            if BUSY[i].load(Ordering::Acquire) != 0 {
                continue;
            }
            let silent = now.wrapping_sub(LAST_CHECKIN[i].load(Ordering::Relaxed));
            if silent > info.timeout_ms {
                late = Some((info.name, silent, info.timeout_ms));
                break;
            }
        }
    }
    if let Some((name, silent, limit)) = late {
        fault::thread_hung(name, silent, limit);
    }
}

/// Whether a core keeps taking traps. Core 0 takes the 1 kHz tick; an idle
/// core 1 may sleep in `wfi` for long, so it is sent an IPI (which only
/// makes it reschedule) whenever its count has not moved, and is declared
/// hung only when it has not answered IPIs for `CORE_STUCK_MS`.
#[derive(Clone, Copy, Default)]
struct CoreProgress {
    traps: u32,
    /// When the first unanswered IPI was sent.
    pinged_at: Option<u32>,
}

impl CoreProgress {
    fn check(&mut self, hart: usize) {
        let traps = fault::trap_count(hart);
        if !sched::is_online(hart) || hart == irq::hart_id() || traps != self.traps {
            *self = CoreProgress { traps, pinged_at: None };
            return;
        }
        let now = time::ticks();
        match self.pinged_at {
            Some(t) if now.wrapping_sub(t) > CORE_STUCK_MS => fault::core_stuck(hart, now.wrapping_sub(t)),
            Some(_) => {}
            None => self.pinged_at = Some(now),
        }
        smp::send_ipi(hart);
    }
}
