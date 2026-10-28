//! Preemptive priority scheduler for both HP cores.
//!
//! Every thread owns a stack. On any trap the interrupted context is pushed
//! onto the current thread's stack (see `boot.rs`); switching threads means
//! returning a different thread's saved frame pointer to the trap exit path.
//!
//! Both cores pick from one thread table guarded by one spinlock. Each core
//! has its own current thread and its own idle thread, and a thread may be
//! pinned to one core (`spawn_on`). Policy: each core runs the
//! highest-priority ready thread allowed on it that the other core is not
//! running; equal priorities share the cores round-robin with a fixed time
//! slice. When a thread becomes ready, the core running the lowest-priority
//! thread it outranks is told to reschedule, through an inter-processor
//! interrupt if that is the other core. The tick runs on core 0 only and
//! accounts time for both cores.

use crate::boot::TrapFrame;
use crate::irq::{self, HART_COUNT};
use crate::spin::SpinLock;
use crate::{kprintln, smp, time};
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const MAX_THREADS: usize = 16;
pub const PRIORITY_IDLE: u8 = 0;
const TIME_SLICE_TICKS: u32 = 10;
const STACK_CANARY: u32 = 0xDEAD_C0DE;
const STACK_FILL: u32 = 0xA5A5_A5A5;
const IDLE_NAMES: [&str; HART_COUNT] = ["idle0", "idle1"];

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
    /// Core the thread is pinned to, or `None` to run anywhere.
    affinity: Option<usize>,
    /// Core executing the thread right now. Its stack and saved frame belong
    /// to that core until it switches away.
    on_cpu: Option<usize>,
    /// Core that ran the thread most recently.
    last_cpu: Option<usize>,
}

impl Thread {
    fn allowed_on(&self, hart: usize) -> bool {
        self.affinity.is_none_or(|a| a == hart)
    }
}

struct Scheduler {
    threads: [Option<Thread>; MAX_THREADS],
    current: [Option<usize>; HART_COUNT],
}

// Thread stacks and frames are only reached through the scheduler lock.
unsafe impl Send for Scheduler {}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    threads: [const { None }; MAX_THREADS],
    current: [None; HART_COUNT],
});

static ONLINE: [AtomicBool; HART_COUNT] = [const { AtomicBool::new(false) }; HART_COUNT];
static NEED_RESCHED: [AtomicBool; HART_COUNT] = [const { AtomicBool::new(false) }; HART_COUNT];
static IDLE_TICKS: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];
static LOAD_PERMILLE: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];

/// Create a thread that runs `entry(arg)` on any core.
pub fn spawn(name: &'static str, priority: u8, stack_size: usize, entry: fn(usize), arg: usize) -> usize {
    spawn_on(None, name, priority, stack_size, entry, arg)
}

/// Create a thread that runs `entry(arg)` on a freshly allocated stack,
/// pinned to core `affinity` if given.
pub fn spawn_on(
    affinity: Option<usize>,
    name: &'static str,
    priority: u8,
    stack_size: usize,
    entry: fn(usize),
    arg: usize,
) -> usize {
    assert!(affinity.is_none_or(|a| a < HART_COUNT), "no such core");
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

    // Allocated up front so reaping does not allocate under the lock.
    let mut dead = alloc::vec::Vec::with_capacity(MAX_THREADS);
    let slot = {
        let mut s = SCHED.lock();
        reap_exited(&mut s, &mut dead);
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
            affinity,
            on_cpu: None,
            last_cpu: None,
        });
        kick(&s, priority, affinity);
        slot
    };
    // Free the dead stacks outside the critical section.
    drop(dead);
    slot
}

/// Detach exited threads that no core is still running on and move them
/// into `dead` so the caller can free their stacks.
fn reap_exited(s: &mut Scheduler, dead: &mut alloc::vec::Vec<Thread>) {
    for slot in s.threads.iter_mut() {
        if slot.as_ref().is_some_and(|t| t.state == State::Exited && t.on_cpu.is_none()) {
            dead.push(slot.take().unwrap());
        }
    }
}

extern "C" fn thread_trampoline(entry: usize, arg: usize) -> ! {
    let entry: fn(usize) = unsafe { core::mem::transmute(entry) };
    entry(arg);
    thread_exit()
}

/// Id of the thread running on the calling core, if the scheduler has
/// started there.
pub fn current_id() -> Option<usize> {
    SCHED.lock().current[irq::hart_id()]
}

/// End the calling thread (as if its entry function had returned).
pub fn exit_current() -> ! {
    thread_exit()
}

extern "C" fn thread_exit() -> ! {
    if let Some(t) = current_mut(&mut SCHED.lock()) {
        t.state = State::Exited;
    }
    yield_now();
    unreachable!("exited thread was scheduled again")
}

/// The thread running on the calling core. Interrupts are masked while the
/// lock is held, so the core cannot change under the caller.
fn current_mut(s: &mut Scheduler) -> Option<&mut Thread> {
    let cur = s.current[irq::hart_id()]?;
    s.threads[cur].as_mut()
}

/// Hand the calling core to the scheduler. Never returns to the boot stack.
pub fn start() -> ! {
    let hart = irq::hart_id();
    spawn_on(Some(hart), IDLE_NAMES[hart], PRIORITY_IDLE, 2048, idle, 0);
    ONLINE[hart].store(true, Ordering::Release);
    irq::enable_global();
    yield_now();
    unreachable!("boot context resumed")
}

/// Whether the caller is a scheduled thread (not boot code or a handler
/// running before the core's first switch).
pub fn in_thread() -> bool {
    !irq::in_trap() && SCHED.lock().current[irq::hart_id()].is_some()
}

/// Whether `hart` has entered the scheduler.
pub fn is_online(hart: usize) -> bool {
    ONLINE[hart].load(Ordering::Acquire)
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
    {
        let mut s = SCHED.lock();
        let t = current_mut(&mut s).expect("sleep outside a thread");
        t.state = State::Sleeping { until: time::ticks().wrapping_add(ms) };
    }
    yield_now();
}

/// Blocking primitives build on `block_unless` and `wake_one`. Their own state
/// is protected by the scheduler lock, which both run the closure under.
///
/// `block_unless` calls `try_take`. If that fails, `add_waiter(current
/// thread id)` runs and the thread is marked blocked in the same critical
/// section, so a concurrent `wake` cannot be lost; the caller then calls
/// `yield_now` and retries. Boot code may use it before the scheduler runs
/// as long as it never has to wait.
pub fn block_unless(try_take: impl FnOnce() -> bool, add_waiter: impl FnOnce(usize)) -> bool {
    let mut s = SCHED.lock();
    if try_take() {
        return true;
    }
    let cur = s.current[irq::hart_id()].expect("block outside a thread");
    add_waiter(cur);
    s.threads[cur].as_mut().unwrap().state = State::Blocked;
    false
}

/// Run `f` under the scheduler lock; it updates the primitive and returns
/// its waiter mask (one bit per thread id). The most important thread in
/// the mask that is still blocked is made ready and its bit cleared; bits
/// of threads no longer blocked are dropped. Costs nothing beyond the lock
/// when there are no waiters. Safe from interrupt handlers.
pub fn wake_one<'a>(f: impl FnOnce() -> &'a mut u32) {
    let mut guard = SCHED.lock();
    let s = &mut *guard;
    let mask = f();
    let mut best: Option<(usize, u8)> = None;
    let mut bits = *mask;
    while bits != 0 {
        let id = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        match s.threads.get(id).and_then(Option::as_ref) {
            Some(t) if t.state == State::Blocked => {
                if best.is_none_or(|(_, p)| t.priority > p) {
                    best = Some((id, t.priority));
                }
            }
            _ => *mask &= !(1 << id),
        }
    }
    let Some((id, _)) = best else { return };
    *mask &= !(1 << id);
    let t = s.threads[id].as_mut().unwrap();
    t.state = State::Ready;
    let (priority, affinity) = (t.priority, t.affinity);
    kick(s, priority, affinity);
}

/// Priority of what `hart` runs now, or -1 if it is idle, between threads,
/// or about to block, so that any ready thread should take it.
fn running_priority(s: &Scheduler, hart: usize) -> i16 {
    match s.current[hart].and_then(|i| s.threads[i].as_ref()) {
        Some(t) if t.state == State::Ready && t.priority != PRIORITY_IDLE => t.priority as i16,
        _ => -1,
    }
}

/// A thread of `priority` became ready: ask the online core running the
/// least important thread it outranks to reschedule. Ties go to the
/// calling core, which saves an interrupt.
fn kick(s: &Scheduler, priority: u8, affinity: Option<usize>) {
    let me = irq::hart_id();
    let mut best: Option<(usize, i16)> = None;
    for hart in 0..HART_COUNT {
        if !is_online(hart) || affinity.is_some_and(|a| a != hart) {
            continue;
        }
        let running = running_priority(s, hart);
        if running >= priority as i16 {
            continue;
        }
        if best.is_none_or(|(_, p)| running < p || (running == p && hart == me)) {
            best = Some((hart, running));
        }
    }
    if let Some((hart, _)) = best {
        resched(hart);
    }
}

/// Make `hart` reschedule soon. Trap handlers check the flag on their way
/// out; anything else needs an interrupt (to the calling core too, since
/// core 1 has no tick of its own to notice the flag).
fn resched(hart: usize) {
    NEED_RESCHED[hart].store(true, Ordering::SeqCst);
    if hart != irq::hart_id() || !irq::in_trap() {
        smp::send_ipi(hart);
    }
}

/// Consume this core's pending reschedule request.
pub fn take_resched() -> bool {
    // A plain load first keeps the common "nothing pending" path free of
    // an atomic read-modify-write. A request landing between the load and
    // the store is served by the switch the caller is about to do.
    let flag = &NEED_RESCHED[irq::hart_id()];
    if !flag.load(Ordering::SeqCst) {
        return false;
    }
    flag.store(false, Ordering::SeqCst);
    true
}

/// Share of the last full second core `hart` spent outside its idle thread.
pub fn cpu_load(hart: usize) -> f32 {
    LOAD_PERMILLE[hart].load(Ordering::Relaxed) as f32 / 1000.0
}

/// Whether a ready thread other than the one `hart` runs could take over
/// from a thread of `priority` there.
fn has_contender(s: &Scheduler, hart: usize, priority: u8) -> bool {
    s.threads.iter().enumerate().any(|(i, t)| {
        t.as_ref().is_some_and(|t| {
            s.current[hart] != Some(i)
                && t.state == State::Ready
                && t.on_cpu.is_none()
                && t.allowed_on(hart)
                && t.priority >= priority
        })
    })
}

/// Called from the tick interrupt on core 0: wake sleepers, account time on
/// every core, and end time slices that another thread is waiting for.
pub fn on_tick(now: u32) {
    let mut guard = SCHED.lock();
    let s = &mut *guard;
    for i in 0..MAX_THREADS {
        let Some(t) = s.threads[i].as_mut() else { continue };
        if let State::Sleeping { until } = t.state {
            if (now.wrapping_sub(until) as i32) >= 0 {
                t.state = State::Ready;
                let (priority, affinity) = (t.priority, t.affinity);
                kick(s, priority, affinity);
            }
        }
    }
    let end_of_second = now % 1000 == 0;
    for hart in 0..HART_COUNT {
        if end_of_second {
            let idle = IDLE_TICKS[hart].swap(0, Ordering::Relaxed);
            let load = if is_online(hart) { 1000u32.saturating_sub(idle) } else { 0 };
            LOAD_PERMILLE[hart].store(load, Ordering::Relaxed);
        }
        let Some(t) = s.current[hart].and_then(|i| s.threads[i].as_mut()) else { continue };
        t.run_ticks += 1;
        if t.priority == PRIORITY_IDLE {
            IDLE_TICKS[hart].fetch_add(1, Ordering::Relaxed);
            continue;
        }
        t.slice_used += 1;
        let priority = t.priority;
        if t.slice_used >= TIME_SLICE_TICKS && has_contender(s, hart, priority) {
            resched(hart);
        }
    }
}

/// Pick the next thread for the calling core and return the frame to
/// resume. `frame` is the context that trapped, or a throwaway boot frame
/// before the core's first switch.
pub fn switch(frame: *mut TrapFrame) -> *mut TrapFrame {
    let mut guard = SCHED.lock();
    let s = &mut *guard;
    let hart = irq::hart_id();
    let prev = s.current[hart];
    if let Some(cur) = prev {
        let t = s.threads[cur].as_mut().unwrap();
        t.frame = frame;
        if unsafe { t.stack_bottom.read() } != STACK_CANARY {
            kprintln!("*** stack overflow in thread '{}' on hart {}", t.name, hart);
            crate::halt();
        }
    }

    let next = pick_next(s, hart);
    s.current[hart] = Some(next);
    if prev != Some(next) {
        if let Some(cur) = prev {
            let t = s.threads[cur].as_mut().unwrap();
            save_fp(t);
            t.on_cpu = None;
            // Preempted but still runnable: another core may be free for it.
            if t.state == State::Ready {
                let (priority, affinity) = (t.priority, t.affinity);
                kick(s, priority, affinity);
            }
        }
        restore_fp(s.threads[next].as_ref().unwrap());
    }
    let t = s.threads[next].as_mut().unwrap();
    t.on_cpu = Some(hart);
    t.last_cpu = Some(hart);
    t.slice_used = 0;
    t.frame
}

/// Highest-priority ready thread that may run on `hart` and is not running
/// elsewhere, round-robin among equals starting after the current one.
fn pick_next(s: &Scheduler, hart: usize) -> usize {
    let start = s.current[hart].map_or(0, |c| c + 1);
    let mut best: Option<(usize, u8)> = None;
    for k in 0..MAX_THREADS {
        let i = (start + k) % MAX_THREADS;
        if let Some(t) = &s.threads[i] {
            let runnable =
                t.state == State::Ready && t.on_cpu.is_none_or(|c| c == hart) && t.allowed_on(hart);
            if runnable && best.is_none_or(|(_, p)| t.priority > p) {
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
    /// Core running the thread now.
    pub cpu: Option<usize>,
    /// Core that ran it most recently.
    pub last_cpu: Option<usize>,
    pub affinity: Option<usize>,
}

/// Copy out thread info so callers can print without holding the lock.
/// Gives up with the holder's hart id if the scheduler lock stays taken
/// (a core stuck inside the scheduler), so a debug dump cannot hang.
pub fn snapshot() -> Result<[Option<ThreadInfo>; MAX_THREADS], Option<usize>> {
    let mut out = [None; MAX_THREADS];
    let mut stacks = [(0usize, 0usize); MAX_THREADS];
    {
        let Some(s) = SCHED.try_lock_for(100_000) else {
            return Err(SCHED.owner());
        };
        for (i, t) in s.threads.iter().enumerate() {
            if let Some(t) = t {
                stacks[i] = (t.stack_bottom as usize, t.stack_size);
                out[i] = Some(ThreadInfo {
                    id: i,
                    name: t.name,
                    priority: t.priority,
                    state: t.state,
                    run_ticks: t.run_ticks,
                    stack_size: t.stack_size,
                    stack_used: 0,
                    cpu: t.on_cpu,
                    last_cpu: t.last_cpu,
                    affinity: t.affinity,
                });
            }
        }
    }
    // Scanning tens of KiB of stack takes too long to do with interrupts
    // masked, so it runs unlocked. A thread reaped meanwhile only makes
    // the scan read freed (still mapped) heap RAM; it is reported as it
    // was when the lock was held.
    for (info, &(bottom, size)) in out.iter_mut().zip(stacks.iter()) {
        if let Some(info) = info {
            info.stack_used = stack_high_water(bottom as *const u32, size);
        }
    }
    Ok(out)
}

/// Bytes of stack ever touched, found by scanning for the fill pattern.
fn stack_high_water(bottom: *const u32, size: usize) -> usize {
    let words = size / 4;
    let untouched =
        (1..words).take_while(|&i| unsafe { bottom.add(i).read_volatile() } == STACK_FILL).count();
    size - (untouched + 1) * 4
}
