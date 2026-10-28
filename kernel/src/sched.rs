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
//!
//! Each thread also has a 32-bit notification word (`notify`,
//! `wait_notify`), the cheapest way for an interrupt handler to wake one
//! known thread, and the only one open to fast handlers (see `irq`).

use crate::boot::TrapFrame;
use crate::irq::{self, HART_COUNT};
use crate::spin::SpinLock;
use crate::{smp, time};
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
    _stack: crate::heap::Stack,
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
    /// Ids of threads in `State::Ready` and `State::Sleeping`, so picking
    /// the next thread and the tick only visit the threads concerned. Kept
    /// in step with `Thread::state` by `set_state`.
    ready: u32,
    sleeping: u32,
}

impl Scheduler {
    fn set_state(&mut self, id: usize, state: State) {
        let bit = 1 << id;
        self.ready &= !bit;
        self.sleeping &= !bit;
        match state {
            State::Ready => self.ready |= bit,
            State::Sleeping { .. } => self.sleeping |= bit,
            State::Blocked | State::Exited => {}
        }
        self.threads[id].as_mut().expect("no such thread").state = state;
    }
}

// Thread stacks and frames are only reached through the scheduler lock.
unsafe impl Send for Scheduler {}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    threads: [const { None }; MAX_THREADS],
    current: [None; HART_COUNT],
    ready: 0,
    sleeping: 0,
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
    let mut stack_mem = crate::heap::Stack::new(stack_size, STACK_FILL);
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
        NOTIFY_PENDING[slot].store(0, Ordering::Relaxed);
        NOTIFY_WAITING[slot].store(0, Ordering::Relaxed);
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
        s.set_state(slot, State::Ready);
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
    {
        let mut s = SCHED.lock();
        if let Some(cur) = s.current[irq::hart_id()] {
            s.set_state(cur, State::Exited);
        }
    }
    yield_now();
    unreachable!("exited thread was scheduled again")
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
        // Kernel-level interrupts, the only ones that may switch threads,
        // are masked while the lock is held, so the core cannot change.
        let mut s = SCHED.lock();
        let cur = s.current[irq::hart_id()].expect("sleep outside a thread");
        s.set_state(cur, State::Sleeping { until: time::ticks().wrapping_add(ms) });
    }
    yield_now();
}

/// Blocking primitives build on `block_unless` and `wake_one`.
///
/// `block_unless` runs `try_take(current thread id)` under the scheduler
/// lock. If it fails, the thread is marked blocked in the same critical
/// section, so a `wake_one` that follows cannot be lost; the caller then
/// calls `yield_now` and retries. `try_take` must register the thread as
/// a waiter before its last check of the primitive's state (see
/// `sync::Semaphore`). Boot code may use it before the scheduler runs as
/// long as it never has to wait.
pub fn block_unless(try_take: impl FnOnce(usize) -> bool) -> bool {
    let mut s = SCHED.lock();
    let cur = s.current[irq::hart_id()].expect("block outside a thread");
    if try_take(cur) {
        return true;
    }
    s.set_state(cur, State::Blocked);
    false
}

/// Under the scheduler lock, make the most important thread in `waiters`
/// (one bit per thread id) that is still blocked ready and clear its bit;
/// bits of threads no longer blocked are dropped. Safe from kernel-level
/// interrupt handlers.
pub fn wake_one(waiters: &AtomicU32) {
    let mut guard = SCHED.lock();
    let s = &mut *guard;
    let mut best: Option<(usize, u8)> = None;
    let mut stale = 0;
    let mut bits = waiters.load(Ordering::Relaxed);
    while bits != 0 {
        let id = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        match s.threads.get(id).and_then(Option::as_ref) {
            Some(t) if t.state == State::Blocked => {
                if best.is_none_or(|(_, p)| t.priority > p) {
                    best = Some((id, t.priority));
                }
            }
            _ => stale |= 1 << id,
        }
    }
    let Some((id, _)) = best else {
        waiters.fetch_and(!stale, Ordering::Relaxed);
        return;
    };
    waiters.fetch_and(!(stale | 1 << id), Ordering::Relaxed);
    make_ready(s, id);
}

/// Make thread `id` ready and find a core for it.
fn make_ready(s: &mut Scheduler, id: usize) {
    s.set_state(id, State::Ready);
    let t = s.threads[id].as_ref().unwrap();
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
        // Relaxed is enough under the lock: a core sets its flag before its
        // first switch takes the lock, and that switch schedules anyway.
        if !ONLINE[hart].load(Ordering::Relaxed) || affinity.is_some_and(|a| a != hart) {
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
    // `send_ipi` fences before raising the line, and without an IPI this
    // core reads the flag itself, so no ordering is needed here.
    NEED_RESCHED[hart].store(true, Ordering::Relaxed);
    if hart != irq::hart_id() || !irq::in_trap() {
        smp::send_ipi(hart);
    }
}

/// Consume this core's pending reschedule request.
pub fn take_resched() -> bool {
    // A plain load first keeps the common "nothing pending" path free of
    // an atomic read-modify-write. A request landing between the load and
    // the store is served by the switch the caller is about to do.
    // A flag set by the other core arrives with an IPI whose handler fences
    // before this runs; one missed here is served by that pending IPI.
    let flag = &NEED_RESCHED[irq::hart_id()];
    if !flag.load(Ordering::Relaxed) {
        return false;
    }
    // The interrupted code is in a kernel critical section, possibly
    // holding the scheduler lock: a fast interrupt, or a kernel-level one
    // taken just before a threshold raise took effect. Switching now would
    // also hand the raised threshold to the next thread. Leave the request
    // pending and raise the (kernel-level) IPI, which is taken, and
    // switches, as soon as the critical section ends.
    if irq::kernel_masked() {
        smp::send_ipi(irq::hart_id());
        return false;
    }
    flag.store(false, Ordering::Relaxed);
    true
}

// --- task notifications ---------------------------------------------------

/// Notification bits, one per signal source, so that one thread can wait
/// for several sources at once.
pub mod notify_bits {
    pub const DISPLAY_FLIP: u32 = 1 << 0;
    pub const AUDIO_DMA: u32 = 1 << 1;
    pub const CONSOLE_RX: u32 = 1 << 2;
    pub const BENCH: u32 = 1 << 31;
}

/// Pending notification bits per thread id. Kept outside the scheduler
/// lock so `notify` can post without it.
static NOTIFY_PENDING: [AtomicU32; MAX_THREADS] = [const { AtomicU32::new(0) }; MAX_THREADS];
/// Bits a thread blocked in `wait_notify` waits for, 0 when it is not
/// waiting. Set by the thread and cleared by its waker, both under the
/// scheduler lock.
static NOTIFY_WAITING: [AtomicU32; MAX_THREADS] = [const { AtomicU32::new(0) }; MAX_THREADS];
/// Threads whose wake-up `notify` handed to the IPI handler because the
/// scheduler lock could be held by the code it interrupted.
static DEFERRED_WAKE: AtomicU32 = AtomicU32::new(0);

const _: () = assert!(MAX_THREADS <= 32);

/// Id of the calling thread, to hand to whoever will `notify` it.
pub fn my_id() -> usize {
    current_id().expect("not called from a thread")
}

/// Post `bits` to thread `id` and wake it if it waits for any of them.
///
/// Safe from any context, fast interrupt handlers included. Costs no
/// scheduler lock when the thread is not waiting for these bits and one
/// acquisition when it is. When kernel-level interrupts are masked (the
/// caller, or the code a fast handler interrupted, may hold the scheduler
/// lock) the wake-up is deferred to a self-IPI, which runs as soon as that
/// critical section ends.
pub fn notify(id: usize, bits: u32) {
    NOTIFY_PENDING[id].fetch_or(bits, Ordering::SeqCst);
    // Pairs with `wait_notify`, which publishes its mask before checking
    // the pending bits: either it sees these bits or this sees it waiting.
    if NOTIFY_WAITING[id].load(Ordering::SeqCst) & bits == 0 {
        return;
    }
    if irq::kernel_masked() {
        DEFERRED_WAKE.fetch_or(1 << id, Ordering::SeqCst);
        smp::send_ipi(irq::hart_id());
        return;
    }
    wake_notified(&mut SCHED.lock(), id);
}

/// Make thread `id` ready if it is blocked in `wait_notify` and one of the
/// bits it waits for is pending.
fn wake_notified(s: &mut Scheduler, id: usize) {
    let waiting = NOTIFY_WAITING[id].load(Ordering::Relaxed);
    if waiting == 0 || NOTIFY_PENDING[id].load(Ordering::Relaxed) & waiting == 0 {
        return;
    }
    if s.threads[id].as_ref().is_none_or(|t| t.state != State::Blocked) {
        return;
    }
    NOTIFY_WAITING[id].store(0, Ordering::Relaxed);
    make_ready(s, id);
}

/// Perform the wake-ups `notify` deferred. Called from the (kernel-level)
/// IPI handler.
pub fn wake_deferred() {
    let mut ids = DEFERRED_WAKE.swap(0, Ordering::SeqCst);
    if ids == 0 {
        return;
    }
    let mut s = SCHED.lock();
    while ids != 0 {
        let id = ids.trailing_zeros() as usize;
        ids &= ids - 1;
        wake_notified(&mut s, id);
    }
}

/// Block until one of the bits in `mask` is notified, then clear and return
/// the pending bits of `mask`. Bits outside `mask` stay pending. Bits that
/// arrived before the call count, so a notification is never lost between
/// a thread arming its source and waiting.
pub fn wait_notify(mask: u32) -> u32 {
    assert!(mask != 0, "wait_notify with an empty mask");
    // Known after the first pass: a woken thread consumes its bits without
    // taking the lock again (only the thread itself clears them).
    let mut me: Option<usize> = None;
    loop {
        if let Some(id) = me {
            let got = NOTIFY_PENDING[id].fetch_and(!mask, Ordering::SeqCst) & mask;
            if got != 0 {
                return got;
            }
        }
        {
            let mut s = SCHED.lock();
            let id = s.current[irq::hart_id()].expect("wait outside a thread");
            me = Some(id);
            NOTIFY_WAITING[id].store(mask, Ordering::SeqCst);
            let got = NOTIFY_PENDING[id].fetch_and(!mask, Ordering::SeqCst) & mask;
            if got != 0 {
                NOTIFY_WAITING[id].store(0, Ordering::Relaxed);
                return got;
            }
            s.set_state(id, State::Blocked);
        }
        yield_now();
    }
}

/// Share of the last full second core `hart` spent outside its idle thread.
pub fn cpu_load(hart: usize) -> f32 {
    LOAD_PERMILLE[hart].load(Ordering::Relaxed) as f32 / 1000.0
}

/// Whether a ready thread other than the one `hart` runs could take over
/// from a thread of `priority` there.
fn has_contender(s: &Scheduler, hart: usize, priority: u8) -> bool {
    let running = s.current[hart].map_or(0, |c| 1 << c);
    ids(s.ready & !running).any(|i| {
        let t = s.threads[i].as_ref().unwrap();
        t.on_cpu.is_none() && t.allowed_on(hart) && t.priority >= priority
    })
}

/// Thread ids set in `mask`, lowest first.
fn ids(mut mask: u32) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        let id = mask.trailing_zeros() as usize;
        mask &= mask.wrapping_sub(1);
        (id < 32).then_some(id)
    })
}

/// Called from the tick interrupt on core 0: wake sleepers, account time on
/// every core, and end time slices that another thread is waiting for.
pub fn on_tick(now: u32) {
    let mut guard = SCHED.lock();
    let s = &mut *guard;
    for i in ids(s.sleeping) {
        if let State::Sleeping { until } = s.threads[i].as_ref().unwrap().state {
            if (now.wrapping_sub(until) as i32) >= 0 {
                make_ready(s, i);
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
            crate::fault::stack_canary(unsafe { &*frame }, t.name, t.stack_bottom as usize, t.stack_size);
        }
    }

    let next = pick_next(s, hart);
    s.current[hart] = Some(next);
    if prev != Some(next) {
        if let Some(cur) = prev {
            let t = s.threads[cur].as_mut().unwrap();
            save_fp(t);
            t.on_cpu = None;
            // Preempted but still runnable: another core may be free for it
            // (not for an idle thread, which is pinned to this core).
            if t.state == State::Ready && t.priority != PRIORITY_IDLE {
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
    crate::fault::on_switch(hart, t.name, t.stack_bottom as usize, t.stack_size);
    t.frame
}

/// Highest-priority ready thread that may run on `hart` and is not running
/// elsewhere, round-robin among equals starting after the current one.
fn pick_next(s: &Scheduler, hart: usize) -> usize {
    let start = s.current[hart].map_or(0, |c| c + 1) % MAX_THREADS;
    let before = (1u32 << start) - 1;
    let mut best: Option<(usize, u8)> = None;
    // Ids from `start` up first, then the rest, so the first of several
    // equals found is the next one round-robin.
    for i in ids(s.ready & !before).chain(ids(s.ready & before)) {
        let t = s.threads[i].as_ref().unwrap();
        let runnable = t.on_cpu.is_none_or(|c| c == hart) && t.allowed_on(hart);
        if runnable && best.is_none_or(|(_, p)| t.priority > p) {
            best = Some((i, t.priority));
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

/// Where a thread stands, for crash reports.
pub struct Context {
    /// pc, ra and sp saved when it last left a core (stale while running).
    pub pc: u32,
    pub ra: u32,
    pub sp: u32,
    pub stack: (usize, usize),
    pub on_cpu: Option<usize>,
    pub last_cpu: Option<usize>,
}

/// Context of the thread called `name`. Gives up if the scheduler lock
/// stays taken.
pub fn context_of(name: &str) -> Option<Context> {
    let s = SCHED.try_lock_for(100_000)?;
    let t = s.threads.iter().flatten().find(|t| t.name == name)?;
    let f = unsafe { &*t.frame };
    Some(Context {
        pc: f.mepc,
        ra: f.regs[0],
        sp: f.regs[1],
        stack: (t.stack_bottom as usize, t.stack_bottom as usize + t.stack_size),
        on_cpu: t.on_cpu,
        last_cpu: t.last_cpu,
    })
}
