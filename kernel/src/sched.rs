//! Preemptive priority scheduler (single core).
//!
//! Every thread owns a stack. On any trap the interrupted context is pushed
//! onto the current thread's stack (see `boot.rs`); switching threads means
//! returning a different thread's saved frame pointer to the trap exit path.
//!
//! Policy: the highest-priority ready thread runs; equal priorities share
//! the core round-robin with a fixed time slice. All scheduler state is only
//! touched with interrupts masked (trap context or `irq::free`).

use crate::boot::TrapFrame;
use crate::{irq, kprintln, time};
use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const MAX_THREADS: usize = 16;
pub const PRIORITY_IDLE: u8 = 0;
const TIME_SLICE_TICKS: u32 = 10;
const STACK_CANARY: u32 = 0xDEAD_C0DE;
const STACK_FILL: u32 = 0xA5A5_A5A5;

const MSTATUS_MPP_M: u32 = 3 << 11;
const MSTATUS_MPIE: u32 = 1 << 7;
const MSTATUS_FS_MASK: u32 = 3 << 13;
const MSTATUS_FS_INITIAL: u32 = 1 << 13;
const MSTATUS_FS_CLEAN: u32 = 2 << 13;
const MSTATUS_FS_DIRTY: u32 = 3 << 13;
/// CLIC mirrors MPP/MPIE into mcause and restores them on `mret`.
const MCAUSE_MPP_M: u32 = 3 << 28;
const MCAUSE_MPIE: u32 = 1 << 27;

/// Kernel call numbers passed in a7 with `ecall`.
pub mod call {
    pub const YIELD: u32 = 0;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ready,
    Sleeping { until: u32 },
    Blocked,
    Exited,
}

pub struct Thread {
    _stack: alloc::boxed::Box<[u32]>,
    pub name: &'static str,
    pub priority: u8,
    pub state: State,
    frame: *mut TrapFrame,
    stack_bottom: *mut u32,
    pub stack_size: usize,
    fp: [u32; 33],
    pub run_ticks: u32,
    slice_used: u32,
}

struct Scheduler {
    threads: [Option<Thread>; MAX_THREADS],
    current: Option<usize>,
}

struct Global(UnsafeCell<Scheduler>);
unsafe impl Sync for Global {}

static SCHED: Global = Global(UnsafeCell::new(Scheduler {
    threads: [const { None }; MAX_THREADS],
    current: None,
}));

/// Only call with interrupts masked.
fn sched() -> &'static mut Scheduler {
    unsafe { &mut *SCHED.0.get() }
}

/// Create a thread that runs `entry(arg)` on a freshly allocated stack.
pub fn spawn(name: &'static str, priority: u8, stack_size: usize, entry: fn(usize), arg: usize) -> usize {
    let stack_size = stack_size & !15;
    let mut stack_mem = alloc::vec![STACK_FILL; stack_size / 4].into_boxed_slice();
    stack_mem[0] = STACK_CANARY;
    let stack = stack_mem.as_mut_ptr();
    let top = stack as usize + stack_size;

    // gp is fixed for the whole image, so the creator's value is correct.
    let gp: u32;
    unsafe { asm!("mv {0}, gp", out(reg) gp) };
    let frame = (top - core::mem::size_of::<TrapFrame>()) as *mut TrapFrame;
    unsafe {
        frame.write_bytes(0, 1);
        let f = &mut *frame;
        f.regs[0] = thread_exit as extern "C" fn() -> ! as usize as u32; // ra
        f.regs[1] = top as u32; // sp
        f.regs[2] = gp;
        f.regs[9] = entry as usize as u32; // a0: entry fn pointer
        f.regs[10] = arg as u32; // a1
        f.mepc = thread_trampoline as extern "C" fn(usize, usize) -> ! as usize as u32;
        f.mstatus = MSTATUS_MPP_M | MSTATUS_MPIE | MSTATUS_FS_INITIAL;
        f.mcause = MCAUSE_MPP_M | MCAUSE_MPIE;
    }

    let reaped = irq::free(|| {
        let s = sched();
        let reaped = reap_exited(s);
        let slot = s.threads.iter().position(Option::is_none).expect("thread table full");
        s.threads[slot] = Some(Thread {
            _stack: stack_mem,
            name,
            priority,
            state: State::Ready,
            frame,
            stack_bottom: stack,
            stack_size,
            fp: [0; 33],
            run_ticks: 0,
            slice_used: 0,
        });
        (slot, reaped)
    });
    // Free the dead stacks outside the critical section.
    drop(reaped.1);
    reaped.0
}

/// Detach exited threads (other than the running one, whose stack is still
/// in use) and return them so the caller can free their stacks.
fn reap_exited(s: &mut Scheduler) -> alloc::vec::Vec<Thread> {
    let mut dead = alloc::vec::Vec::new();
    for (i, slot) in s.threads.iter_mut().enumerate() {
        if s.current != Some(i) && slot.as_ref().is_some_and(|t| t.state == State::Exited) {
            dead.push(slot.take().unwrap());
        }
    }
    dead
}

extern "C" fn thread_trampoline(entry: usize, arg: usize) -> ! {
    let entry: fn(usize) = unsafe { core::mem::transmute(entry) };
    entry(arg);
    thread_exit()
}

extern "C" fn thread_exit() -> ! {
    irq::free(|| {
        let s = sched();
        if let Some(t) = s.current.and_then(|i| s.threads[i].as_mut()) {
            t.state = State::Exited;
        }
    });
    yield_now();
    unreachable!("exited thread was scheduled again")
}

/// Hand the core to the scheduler. Never returns to the boot stack.
pub fn start() -> ! {
    spawn("idle", PRIORITY_IDLE, 2048, idle, 0);
    irq::enable_global();
    yield_now();
    unreachable!("boot context resumed")
}

fn idle(_: usize) {
    loop {
        unsafe { asm!("wfi") };
    }
}

pub fn yield_now() {
    unsafe { asm!("ecall", in("a7") call::YIELD, options(nostack)) };
}

pub fn sleep_ms(ms: u32) {
    irq::free(|| {
        let s = sched();
        let cur = s.current.expect("sleep outside a thread");
        s.threads[cur].as_mut().unwrap().state = State::Sleeping { until: time::ticks().wrapping_add(ms) };
    });
    yield_now();
}

/// Mark the current thread blocked and return its id. Must be called with
/// interrupts masked, in the same critical section that registered the
/// thread as a waiter; the caller then calls `yield_now` to switch away.
pub fn mark_current_blocked() -> usize {
    let s = sched();
    let cur = s.current.expect("block outside a thread");
    s.threads[cur].as_mut().unwrap().state = State::Blocked;
    cur
}

pub fn unblock(id: usize) {
    irq::free(|| {
        if let Some(t) = sched().threads[id].as_mut() {
            if t.state == State::Blocked {
                t.state = State::Ready;
            }
        }
    });
}

static NEED_RESCHED: AtomicBool = AtomicBool::new(false);
static IDLE_TICKS: AtomicU32 = AtomicU32::new(0);
static LOAD_PERMILLE: AtomicU32 = AtomicU32::new(0);

/// Share of the last full second not spent in the idle thread (0..=1).
pub fn cpu_load() -> f32 {
    LOAD_PERMILLE.load(Ordering::Relaxed) as f32 / 1000.0
}

pub fn request_resched() {
    NEED_RESCHED.store(true, Ordering::Relaxed);
}

/// Consume a pending reschedule request raised by an interrupt handler.
pub fn take_resched() -> bool {
    NEED_RESCHED.swap(false, Ordering::Relaxed)
}

/// Called from the tick interrupt: account time, wake sleepers, and report
/// whether the current thread should be preempted.
pub fn on_tick(now: u32) -> bool {
    let s = sched();
    let mut woke_priority = None;
    for t in s.threads.iter_mut().flatten() {
        if let State::Sleeping { until } = t.state {
            if (now.wrapping_sub(until) as i32) >= 0 {
                t.state = State::Ready;
                woke_priority = woke_priority.max(Some(t.priority));
            }
        }
    }
    let Some(cur) = s.current.and_then(|i| s.threads[i].as_mut()) else {
        return false;
    };
    if cur.priority == PRIORITY_IDLE {
        IDLE_TICKS.fetch_add(1, Ordering::Relaxed);
    }
    if now % 1000 == 0 {
        let idle = IDLE_TICKS.swap(0, Ordering::Relaxed);
        LOAD_PERMILLE.store(1000u32.saturating_sub(idle), Ordering::Relaxed);
    }
    cur.run_ticks += 1;
    cur.slice_used += 1;
    woke_priority.is_some_and(|p| p > cur.priority) || cur.slice_used >= TIME_SLICE_TICKS
}

/// Pick the next thread and return the frame to resume. `frame` is the
/// context that trapped, or a throwaway boot frame before the first switch.
pub fn switch(frame: *mut TrapFrame) -> *mut TrapFrame {
    let s = sched();
    if let Some(cur) = s.current {
        let t = s.threads[cur].as_mut().unwrap();
        t.frame = frame;
        if unsafe { t.stack_bottom.read() } != STACK_CANARY {
            kprintln!("*** stack overflow in thread '{}'", t.name);
            crate::halt();
        }
    }

    let next = pick_next(s);
    if s.current != Some(next) {
        if let Some(cur) = s.current {
            let t = s.threads[cur].as_mut().unwrap();
            save_fp(t);
        }
        let t = s.threads[next].as_mut().unwrap();
        restore_fp(t);
    }
    let t = s.threads[next].as_mut().unwrap();
    t.slice_used = 0;
    s.current = Some(next);
    t.frame
}

/// Highest priority ready thread, round-robin among equals starting after
/// the current one.
fn pick_next(s: &Scheduler) -> usize {
    let start = s.current.map_or(0, |c| c + 1);
    let mut best: Option<(usize, u8)> = None;
    for k in 0..MAX_THREADS {
        let i = (start + k) % MAX_THREADS;
        if let Some(t) = &s.threads[i] {
            if t.state == State::Ready && best.is_none_or(|(_, p)| t.priority > p) {
                best = Some((i, t.priority));
            }
        }
    }
    best.expect("no runnable thread (idle missing?)").0
}

/// Save FP registers only when the thread actually dirtied them.
fn save_fp(t: &mut Thread) {
    let f = unsafe { &mut *t.frame };
    if f.mstatus & MSTATUS_FS_MASK != MSTATUS_FS_DIRTY {
        return;
    }
    let p = t.fp.as_mut_ptr();
    unsafe {
        asm!(
            "fsw f0, 0({p})", "fsw f1, 4({p})", "fsw f2, 8({p})", "fsw f3, 12({p})",
            "fsw f4, 16({p})", "fsw f5, 20({p})", "fsw f6, 24({p})", "fsw f7, 28({p})",
            "fsw f8, 32({p})", "fsw f9, 36({p})", "fsw f10, 40({p})", "fsw f11, 44({p})",
            "fsw f12, 48({p})", "fsw f13, 52({p})", "fsw f14, 56({p})", "fsw f15, 60({p})",
            "fsw f16, 64({p})", "fsw f17, 68({p})", "fsw f18, 72({p})", "fsw f19, 76({p})",
            "fsw f20, 80({p})", "fsw f21, 84({p})", "fsw f22, 88({p})", "fsw f23, 92({p})",
            "fsw f24, 96({p})", "fsw f25, 100({p})", "fsw f26, 104({p})", "fsw f27, 108({p})",
            "fsw f28, 112({p})", "fsw f29, 116({p})", "fsw f30, 120({p})", "fsw f31, 124({p})",
            "frcsr {t}", "sw {t}, 128({p})",
            p = in(reg) p, t = out(reg) _,
        );
    }
    f.mstatus = (f.mstatus & !MSTATUS_FS_MASK) | MSTATUS_FS_CLEAN;
}

fn restore_fp(t: &Thread) {
    let f = unsafe { &*t.frame };
    if f.mstatus & MSTATUS_FS_MASK == MSTATUS_FS_INITIAL {
        return;
    }
    let p = t.fp.as_ptr();
    unsafe {
        asm!(
            "flw f0, 0({p})", "flw f1, 4({p})", "flw f2, 8({p})", "flw f3, 12({p})",
            "flw f4, 16({p})", "flw f5, 20({p})", "flw f6, 24({p})", "flw f7, 28({p})",
            "flw f8, 32({p})", "flw f9, 36({p})", "flw f10, 40({p})", "flw f11, 44({p})",
            "flw f12, 48({p})", "flw f13, 52({p})", "flw f14, 56({p})", "flw f15, 60({p})",
            "flw f16, 64({p})", "flw f17, 68({p})", "flw f18, 72({p})", "flw f19, 76({p})",
            "flw f20, 80({p})", "flw f21, 84({p})", "flw f22, 88({p})", "flw f23, 92({p})",
            "flw f24, 96({p})", "flw f25, 100({p})", "flw f26, 104({p})", "flw f27, 108({p})",
            "flw f28, 112({p})", "flw f29, 116({p})", "flw f30, 120({p})", "flw f31, 124({p})",
            "lw {t}, 128({p})", "fscsr {t}",
            p = in(reg) p, t = out(reg) _,
        );
    }
}

#[derive(Clone, Copy)]
pub struct ThreadInfo {
    pub id: usize,
    pub name: &'static str,
    pub priority: u8,
    pub state: State,
    pub run_ticks: u32,
    pub stack_size: usize,
    pub stack_used: usize,
    pub current: bool,
}

/// Copy out thread info so callers can print without masking interrupts.
pub fn snapshot() -> [Option<ThreadInfo>; MAX_THREADS] {
    irq::free(|| {
        let s = sched();
        let mut out = [None; MAX_THREADS];
        for (i, t) in s.threads.iter().enumerate() {
            if let Some(t) = t {
                out[i] = Some(ThreadInfo {
                    id: i,
                    name: t.name,
                    priority: t.priority,
                    state: t.state,
                    run_ticks: t.run_ticks,
                    stack_size: t.stack_size,
                    stack_used: stack_high_water(t),
                    current: s.current == Some(i),
                });
            }
        }
        out
    })
}

/// Bytes of stack ever touched, found by scanning for the fill pattern.
fn stack_high_water(t: &Thread) -> usize {
    let words = t.stack_size / 4;
    let base = t.stack_bottom;
    let untouched = (1..words).take_while(|&i| unsafe { base.add(i).read() } == STACK_FILL).count();
    t.stack_size - (untouched + 1) * 4
}
