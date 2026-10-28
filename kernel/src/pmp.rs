//! PMP (physical memory protection) setup for user apps.
//!
//! The HP core has 16 PMP entries with static priority (the lowest matching
//! index decides) and a 128-byte granule. M-mode ignores unlocked entries,
//! so the kernel keeps full access; U-mode may only touch memory an entry
//! grants, and faults everywhere else.
//!
//! The ESP-IDF v5.5 bootloader leaves some entries *locked* (they apply to
//! M-mode too and cannot change until reset). On this board, per hart 0:
//!
//! | entry | range                      | grants | effect for apps                |
//! |-------|----------------------------|--------|--------------------------------|
//! | 0     | 0x2000_0000.. CPU subsystem (CLIC, debug) | RW | reachable, cannot be fenced |
//! | 1     | 0x3FF0_0000.. CPU peripherals (cache ctl) | RW | reachable, cannot be fenced |
//! | 2     | 0x4FC0_0000.. mask ROM     | RX     | reachable, cannot be fenced    |
//! | 15    | 0x5000_0000.. HP peripherals | RW   | fenced off by a deny entry     |
//!
//! A locked entry that sits *after* the app entries is overridden by an
//! unlocked copy without permissions at a lower index ("fence"). Locked
//! entries *before* them win regardless; [`check`] reports them once.
//!
//! The plan is computed per hart from that hart's CSRs, because PMP state
//! is per hart. Core 1 never runs the bootloader: all its entries are
//! unlocked (`pmpcfg0..3 = 0`, measured), so apps are pinned there
//! ([`app_hart`]) and reach nothing but their own memory. Measured on core 1:
//! loads from the cache controller and ROM fault as expected, but a U-mode
//! access to the CPU subsystem window (0x2000_0000.., CLIC) raises no
//! exception and stalls the core until reset, with or without an explicit
//! no-access entry. That is a hardware limit PMP cannot fix.

use crate::{irq, kprintln, sched};
use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering};

const ENTRIES: usize = 16;
const R: u32 = 1 << 0;
const W: u32 = 1 << 1;
const X: u32 = 1 << 2;
const A_MASK: u32 = 3 << 3;
const A_OFF: u32 = 0;
const A_TOR: u32 = 1 << 3;
const A_NA4: u32 = 2 << 3;
const A_NAPOT: u32 = 3 << 3;
const L: u32 = 1 << 7;
const MISA_U: u32 = 1 << 20;
const HARTS: usize = irq::HART_COUNT;
macro_rules! pmpaddr_access {
    ($($n:literal),*) => {
        fn read_addr(i: usize) -> u32 {
            let v: u32;
            match i {
                $($n => unsafe { asm!(concat!("csrr {0}, pmpaddr", stringify!($n)), out(reg) v, options(nostack)) },)*
                _ => unreachable!("no PMP entry {}", i),
            }
            v
        }

        fn write_addr(i: usize, v: u32) {
            match i {
                $($n => unsafe { asm!(concat!("csrw pmpaddr", stringify!($n), ", {0}"), in(reg) v, options(nostack)) },)*
                _ => unreachable!("no PMP entry {}", i),
            }
        }
    };
}
pmpaddr_access!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);

fn read_cfg() -> [u32; 4] {
    let (a, b, c, d): (u32, u32, u32, u32);
    unsafe {
        asm!("csrr {0}, pmpcfg0", "csrr {1}, pmpcfg1", "csrr {2}, pmpcfg2", "csrr {3}, pmpcfg3",
             out(reg) a, out(reg) b, out(reg) c, out(reg) d, options(nostack));
    }
    [a, b, c, d]
}

fn write_cfg(w: [u32; 4]) {
    unsafe {
        asm!("csrw pmpcfg0, {0}", "csrw pmpcfg1, {1}", "csrw pmpcfg2, {2}", "csrw pmpcfg3, {3}",
             in(reg) w[0], in(reg) w[1], in(reg) w[2], in(reg) w[3], options(nostack));
    }
}

fn cfg_byte(words: &[u32; 4], i: usize) -> u32 {
    (words[i / 4] >> (8 * (i % 4))) & 0xFF
}

fn set_cfg_byte(words: &mut [u32; 4], i: usize, value: u32) {
    let shift = 8 * (i % 4);
    words[i / 4] = (words[i / 4] & !(0xFF << shift)) | (value << shift);
}

/// How to program this hart's PMP for an app.
#[derive(Clone, Copy)]
struct Plan {
    /// Entry holding the app base; `first+1` grants R+X up to the end of
    /// code, `first+2` R+W up to the end of the stack (both TOR).
    first: usize,
    /// Config words written after the addresses (locked bytes included;
    /// the hardware ignores writes to them).
    cfg: [u32; 4],
    /// Fence entries: (index, pmpaddr) copies of locked ranges.
    fences: [(usize, u32); ENTRIES],
    fence_count: usize,
}

impl Plan {
    fn compute() -> Result<Plan, &'static str> {
        let current = read_cfg();
        let locked = |i: usize| cfg_byte(&current, i) & L != 0;
        let first = (0..ENTRIES).take_while(|&i| locked(i)).count();
        if first + 3 > ENTRIES || (first..first + 3).any(locked) {
            return Err("the PMP entries needed for apps are locked");
        }

        let mut cfg = [0u32; 4];
        for i in (0..ENTRIES).filter(|&i| locked(i)) {
            set_cfg_byte(&mut cfg, i, cfg_byte(&current, i));
        }
        set_cfg_byte(&mut cfg, first + 1, A_TOR | R | X);
        set_cfg_byte(&mut cfg, first + 2, A_TOR | R | W);

        let mut plan = Plan { first, cfg, fences: [(0, 0); ENTRIES], fence_count: 0 };
        let mut next = first + 3;
        for i in (first + 3..ENTRIES).filter(|&i| locked(i)) {
            let mode = cfg_byte(&current, i) & A_MASK;
            if mode == A_TOR {
                // It would also freeze pmpaddr[i-1]; not worth supporting.
                return Err("a locked TOR PMP entry follows the app entries");
            }
            if mode == A_OFF || cfg_byte(&current, i) & (R | W | X) == 0 {
                continue;
            }
            while next < i && locked(next) {
                next += 1;
            }
            if next >= i {
                return Err("no free PMP entry to fence off a locked one");
            }
            set_cfg_byte(&mut plan.cfg, next, mode); // same range, no access
            plan.fences[plan.fence_count] = (next, read_addr(i));
            plan.fence_count += 1;
            next += 1;
        }
        Ok(plan)
    }

    fn apply(&self, base: usize, rx_end: usize, end: usize) {
        // Turn every unlocked entry off before moving addresses around.
        write_cfg([0; 4]);
        write_addr(self.first, (base >> 2) as u32);
        write_addr(self.first + 1, (rx_end >> 2) as u32);
        write_addr(self.first + 2, (end >> 2) as u32);
        for &(i, addr) in &self.fences[..self.fence_count] {
            write_addr(i, addr);
        }
        write_cfg(self.cfg);
    }
}

/// Per-hart plan, touched only by its own hart with interrupts masked.
struct Plans(UnsafeCell<[Option<Result<Plan, &'static str>>; HARTS]>);
unsafe impl Sync for Plans {}
static PLANS: Plans = Plans(UnsafeCell::new([None; HARTS]));

fn with_plan<T>(f: impl FnOnce(Result<&Plan, &'static str>) -> T) -> T {
    irq::free(|| {
        let slot = unsafe { &mut (*PLANS.0.get())[irq::hart_id() % HARTS] };
        let plan = slot.get_or_insert_with(Plan::compute);
        f(plan.as_ref().map_err(|e| *e))
    })
}

/// What a hart's PMP allows for apps, found by [`survey`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    /// Only the app's own memory is reachable from U-mode.
    Full,
    /// Locked entries ahead of the app entries stay reachable.
    Partial,
    Unusable,
}

const UNKNOWN: u8 = 0;
static VERDICT: [AtomicU8; HARTS] = [const { AtomicU8::new(UNKNOWN) }; HARTS];
const PROBE_NAMES: [&str; HARTS] = ["pmp-probe0", "pmp-probe1"];
const PROBE_PRIORITY: u8 = 5;

fn encode(v: Isolation) -> u8 {
    v as u8 + 1
}

fn decode_verdict(v: u8) -> Option<Isolation> {
    [Isolation::Full, Isolation::Partial, Isolation::Unusable].get((v as usize).wrapping_sub(1)).copied()
}

/// Inspect the calling hart: log its PMP lock state, record the verdict.
fn probe_this_hart() {
    let hart = irq::hart_id();
    let misa: u32;
    unsafe { asm!("csrr {0}, misa", out(reg) misa, options(nostack)) };
    let cfg = read_cfg();
    kprintln!("pmp: hart {} pmpcfg0..3 = {:#010x} {:#010x} {:#010x} {:#010x}", hart, cfg[0], cfg[1], cfg[2], cfg[3]);
    let verdict = if misa & MISA_U == 0 {
        kprintln!("pmp: hart {}: no user mode", hart);
        Isolation::Unusable
    } else {
        match with_plan(|p| p.map(|p| p.first)) {
            Err(e) => {
                kprintln!("pmp: hart {}: {}", hart, e);
                Isolation::Unusable
            }
            Ok(first) => {
                let mut exposed = false;
                for i in 0..first {
                    let c = cfg_byte(&cfg, i);
                    if c & (R | W | X) != 0 && c & A_MASK != A_OFF {
                        exposed = true;
                        let (lo, size) = decode(c & A_MASK, read_addr(i));
                        kprintln!(
                            "pmp: hart {}: locked entry {} ({:#010x}+{:#x} {}{}{}) stays reachable from apps",
                            hart, i, lo, size,
                            if c & R != 0 { "r" } else { "-" },
                            if c & W != 0 { "w" } else { "-" },
                            if c & X != 0 { "x" } else { "-" },
                        );
                    }
                }
                if exposed { Isolation::Partial } else { Isolation::Full }
            }
        }
    };
    VERDICT[hart].store(encode(verdict), Ordering::Release);
}

fn probe_thread(_: usize) {
    probe_this_hart();
}

/// Verdict per hart. Each online hart is probed once by a short thread
/// pinned to it, since PMP CSRs are per hart.
pub fn survey() -> [Option<Isolation>; HARTS] {
    for hart in 0..HARTS {
        if VERDICT[hart].load(Ordering::Acquire) != UNKNOWN || !sched::is_online(hart) {
            continue;
        }
        sched::spawn_on(Some(hart), PROBE_NAMES[hart], PROBE_PRIORITY, 2048, probe_thread, 0);
        for _ in 0..100 {
            if VERDICT[hart].load(Ordering::Acquire) != UNKNOWN {
                break;
            }
            sched::sleep_ms(2);
        }
    }
    core::array::from_fn(|h| decode_verdict(VERDICT[h].load(Ordering::Acquire)))
}

/// The core user apps are pinned to: one with full isolation if any, else
/// one with partial isolation.
pub fn app_hart() -> Result<usize, &'static str> {
    let verdicts = survey();
    [Isolation::Full, Isolation::Partial]
        .iter()
        .find_map(|want| verdicts.iter().position(|v| *v == Some(*want)))
        .ok_or("no core can confine user mode")
}

/// Range of a NAPOT/NA4 entry as (base, size); TOR is shown as its top.
fn decode(mode: u32, addr: u32) -> (usize, usize) {
    match mode {
        A_NAPOT => {
            let ones = addr.trailing_ones();
            let size = 1usize << (ones + 3).min(31);
            (((addr as usize) << 2) & !(size - 1), size)
        }
        A_NA4 => ((addr as usize) << 2, 4),
        _ => ((addr as usize) << 2, 0),
    }
}

/// Trap context, before `mret` to U-mode: allow `[base, rx_end)` R+X and
/// `[rx_end, end)` R+W on this hart, nothing else that can be fenced.
pub fn enter_app(base: usize, rx_end: usize, end: usize) {
    let ok = with_plan(|plan| plan.map(|p| p.apply(base, rx_end, end)));
    if let Err(e) = ok {
        // Apps are pinned to a hart `app_hart` found usable, so this
        // only happens if a pinned app resumed somewhere else.
        kprintln!("*** cannot isolate user mode on hart {}: {}", irq::hart_id(), e);
        crate::halt()
    }
}
