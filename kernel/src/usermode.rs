//! Isolated user-mode (U-mode) threads.
//!
//! A user app runs in an ordinary scheduler thread. The thread starts in
//! M-mode in [`thread_main`], which registers its [`UserApp`] and asks the
//! trap handler (kernel call `ENTER_USER`) to turn its own trap frame into
//! a U-mode frame. From then on:
//!
//! * The thread's kernel stack top is kept in `mscratch` while it runs in
//!   U-mode, so a trap from U-mode saves the user frame there and never
//!   trusts the user `sp` (see `boot.rs`). The user frame therefore always
//!   sits at `ktop - size_of::<TrapFrame>()`.
//! * Before every `mret` into U-mode, [`before_resume`] programs PMP so that
//!   the app reaches only its own memory: R+X on its code, R+W on data and
//!   stack, and any other U-mode access faults (`pmp.rs`, which also lists
//!   the bootloader-locked ranges that stay reachable). M-mode is not
//!   restricted by the unlocked entries the kernel uses.
//! * Syscalls (`ecall` from U-mode) and faults are not handled in trap
//!   context. The trap handler builds an M-mode frame just below the user
//!   frame that runs [`syscall_entry`] or [`kill_entry`] in thread context on
//!   the thread's kernel stack, with interrupts enabled. A syscall may block
//!   or sleep like any kernel code; it resumes the app with the kernel call
//!   `RETURN_TO_USER`.
//!
//! Integration points with the scheduler (kept small on purpose):
//! `sched::current_id`, `sched::exit_current`, and the per-thread
//! `mstatus`/`mcause` MPP bits in the saved frame. PMP and `mscratch` are
//! per hart, so this also holds when threads run on the second core as long
//! as `before_resume` runs on the hart that executes the `mret`.

use crate::boot::TrapFrame;
use crate::{kprint, kprintln, pmp, sched, trap};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::alloc::Layout;
use core::arch::asm;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};
use p4os_abi::nr;

const FRAME_SIZE: usize = core::mem::size_of::<TrapFrame>();

/// Kernel-internal calls (M-mode `ecall`, number in a7). Kept apart from
/// `sched::call` so the two sets can grow independently.
pub mod call {
    pub const ENTER_USER: u32 = 0x100;
    pub const RETURN_TO_USER: u32 = 0x101;
}

/// Priority and kernel stack of user threads. The kernel stack holds the
/// saved user frame plus whatever a syscall needs (formatting included).
const PRIORITY: u8 = 1;
const KERNEL_STACK: usize = 8 * 1024;

const ECALL_FROM_U: u32 = 8;
const MSTATUS_MPP_M: u32 = 3 << 11;
const MSTATUS_MPIE: u32 = 1 << 7;
const MSTATUS_FS_MASK: u32 = 3 << 13;
const MSTATUS_FS_CLEAN: u32 = 2 << 13;
/// CLIC mirrors MPP/MPIE into mcause; `mret` uses the mcause copy.
const MCAUSE_MPP_MASK: u32 = 3 << 28;
const MCAUSE_MPIE: u32 = 1 << 27;

/// PMP granule of the P4 HP core (ESP-IDF `SOC_CPU_PMP_REGION_GRANULARITY`).
pub const REGION_ALIGN: usize = p4os_abi::REGION_ALIGN as usize;

/// How an app ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Exited(i32),
    Killed,
}

/// Completion state shared between an app and whoever started it.
#[derive(Default)]
pub struct AppStatus {
    done: AtomicBool,
    killed: AtomicBool,
    code: AtomicI32,
}

impl AppStatus {
    pub fn outcome(&self) -> Option<Outcome> {
        if !self.done.load(Ordering::Acquire) {
            None
        } else if self.killed.load(Ordering::Relaxed) {
            Some(Outcome::Killed)
        } else {
            Some(Outcome::Exited(self.code.load(Ordering::Relaxed)))
        }
    }

    fn set(&self, outcome: Outcome) {
        match outcome {
            Outcome::Exited(code) => self.code.store(code, Ordering::Relaxed),
            Outcome::Killed => self.killed.store(true, Ordering::Relaxed),
        }
        self.done.store(true, Ordering::Release);
    }
}

/// Zeroed, PMP-aligned RAM owned by one app.
pub struct Region {
    ptr: NonNull<u8>,
    layout: Layout,
}

// The region is plain memory handed from the loader to the app thread.
unsafe impl Send for Region {}

impl Region {
    pub fn alloc(size: usize) -> Option<Region> {
        let size = size.checked_next_multiple_of(REGION_ALIGN)?;
        let layout = Layout::from_size_align(size, REGION_ALIGN).ok()?;
        if size == 0 {
            return None;
        }
        let ptr = NonNull::new(unsafe { alloc::alloc::alloc_zeroed(layout) })?;
        Some(Region { ptr, layout })
    }

    pub fn base(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    pub fn len(&self) -> usize {
        self.layout.size()
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len()) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// A loaded app: its memory and how to start it.
pub struct UserApp {
    pub name: String,
    region: Region,
    rx_size: usize,
    entry: usize,
    arg: u32,
    /// Top of the kernel stack; the user frame sits just below it.
    ktop: usize,
    status: Arc<AppStatus>,
}

impl UserApp {
    /// `region` holds the relocated image; `[0, rx_size)` becomes R+X and
    /// the rest R+W, with the stack at the top. `entry` is an offset.
    pub fn new(name: String, region: Region, rx_size: usize, entry: usize, arg: u32) -> UserApp {
        assert!(rx_size % REGION_ALIGN == 0 && rx_size < region.len() && entry < rx_size);
        UserApp { name, region, rx_size, entry, arg, ktop: 0, status: Arc::default() }
    }

    pub fn base(&self) -> usize {
        self.region.base()
    }

    /// The user bytes `[ptr, ptr+len)` if the app may read all of them.
    pub fn user_bytes(&self, ptr: u32, len: u32) -> Option<&[u8]> {
        let (start, len) = (ptr as usize, len as usize);
        let end = start.checked_add(len)?;
        let base = self.region.base();
        if len == 0 {
            return Some(&[]);
        }
        if start < base || end > base + self.region.len() {
            return None;
        }
        Some(unsafe { core::slice::from_raw_parts(start as *const u8, len) })
    }

    fn user_frame(&self) -> *mut TrapFrame {
        (self.ktop - FRAME_SIZE) as *mut TrapFrame
    }
}

/// Apps by scheduler thread id. Each entry is written by its own thread
/// (start and exit) and read by that thread or the trap handler running it.
static APPS: [AtomicPtr<UserApp>; sched::MAX_THREADS] = [const { AtomicPtr::new(ptr::null_mut()) }; sched::MAX_THREADS];

fn current_app() -> Option<&'static mut UserApp> {
    let id = sched::current_id()?;
    let app = APPS[id].load(Ordering::Acquire);
    // Only the thread itself (or the trap handler running it) gets here.
    unsafe { app.as_mut() }
}

/// Start `app` in a new thread named `thread_name`. Returns the thread id.
pub fn spawn(thread_name: &'static str, app: UserApp) -> (usize, Arc<AppStatus>) {
    let status = app.status.clone();
    let raw = Box::into_raw(Box::new(app));
    let id = sched::spawn(thread_name, PRIORITY, KERNEL_STACK, thread_main, raw as usize);
    (id, status)
}

fn thread_main(raw: usize) {
    let id = sched::current_id().expect("user thread outside the scheduler");
    APPS[id].store(raw as *mut UserApp, Ordering::Release);
    unsafe { asm!("ecall", in("a7") call::ENTER_USER, options(noreturn)) };
}

/// True for the kernel-internal calls this module serves.
pub fn is_kernel_call(n: u32) -> bool {
    n == call::ENTER_USER || n == call::RETURN_TO_USER
}

/// Trap context: serve `ENTER_USER` / `RETURN_TO_USER` for the current
/// thread and return the U-mode frame to resume.
pub fn kernel_call(frame: *mut TrapFrame) -> *mut TrapFrame {
    let Some(app) = current_app() else {
        kprintln!("*** user-mode kernel call from a thread without an app");
        crate::halt()
    };
    let (number, kernel_mstatus) = unsafe { ((*frame).regs[16], (*frame).mstatus) };
    if number == call::ENTER_USER {
        // Reuse the caller's frame slot: everything above it on the kernel
        // stack belongs to thread_main, which never runs again.
        app.ktop = frame as usize + FRAME_SIZE;
        let base = app.base() as u32;
        unsafe { frame.write_bytes(0, 1) };
        let f = unsafe { &mut *frame };
        f.regs[1] = base + app.region.len() as u32; // sp: top of the app stack
        f.regs[9] = app.arg; // a0
        f.mepc = base + app.entry as u32;
        // MPP = U. FP state starts zeroed and Clean, matching the zeroed
        // per-thread save area, so no other thread's FP values leak in.
        f.mstatus = MSTATUS_MPIE | MSTATUS_FS_CLEAN;
        f.mcause = MCAUSE_MPIE;
        zero_fp_registers();
        frame
    } else {
        let user = app.user_frame();
        let u = unsafe { &mut *user };
        // The FP registers now hold this thread's state as left by the
        // syscall path; carry its dirty tracking over to the user frame.
        u.mstatus = (u.mstatus & !MSTATUS_FS_MASK) | (kernel_mstatus & MSTATUS_FS_MASK);
        user
    }
}

fn zero_fp_registers() {
    unsafe {
        asm!(
            "fmv.w.x f0, zero", "fmv.w.x f1, zero", "fmv.w.x f2, zero", "fmv.w.x f3, zero",
            "fmv.w.x f4, zero", "fmv.w.x f5, zero", "fmv.w.x f6, zero", "fmv.w.x f7, zero",
            "fmv.w.x f8, zero", "fmv.w.x f9, zero", "fmv.w.x f10, zero", "fmv.w.x f11, zero",
            "fmv.w.x f12, zero", "fmv.w.x f13, zero", "fmv.w.x f14, zero", "fmv.w.x f15, zero",
            "fmv.w.x f16, zero", "fmv.w.x f17, zero", "fmv.w.x f18, zero", "fmv.w.x f19, zero",
            "fmv.w.x f20, zero", "fmv.w.x f21, zero", "fmv.w.x f22, zero", "fmv.w.x f23, zero",
            "fmv.w.x f24, zero", "fmv.w.x f25, zero", "fmv.w.x f26, zero", "fmv.w.x f27, zero",
            "fmv.w.x f28, zero", "fmv.w.x f29, zero", "fmv.w.x f30, zero", "fmv.w.x f31, zero",
            "fscsr zero",
            options(nostack),
        );
    }
}

/// Whether a saved frame resumes in U-mode.
pub fn is_user_frame(f: &TrapFrame) -> bool {
    f.mcause & MCAUSE_MPP_MASK == 0
}

/// Trap context: an exception taken from U-mode. Syscalls continue in
/// thread context; anything else kills the app (and only the app).
pub fn user_exception(frame: *mut TrapFrame) -> *mut TrapFrame {
    let f = unsafe { &mut *frame };
    let code = f.mcause & 0xFFF;
    if code == ECALL_FROM_U {
        f.mepc += 4;
        return upcall(frame, syscall_entry);
    }
    let id = sched::current_id().unwrap_or(usize::MAX);
    match current_app() {
        Some(app) => {
            kprint!("*** app '{}' (thread {}) killed: {} at pc={:#010x}", app.name, id, trap::exception_name(code), f.mepc);
            // The offset helps with `llvm-objdump` on the app ELF.
            if let Some(off) = (f.mepc as usize).checked_sub(app.base()).filter(|&o| o < app.region.len()) {
                kprint!(" (app+{:#x})", off);
            }
            kprintln!(", mtval={:#010x}", f.mtval);
        }
        None => {
            kprintln!("*** U-mode exception {} in thread {} without an app", code, id);
            crate::halt()
        }
    }
    upcall(frame, kill_entry)
}

/// Build an M-mode frame just below the user frame that runs `func` on the
/// thread's kernel stack, and return it for the trap exit to resume.
fn upcall(user: *mut TrapFrame, func: extern "C" fn() -> !) -> *mut TrapFrame {
    let gp: u32;
    unsafe { asm!("mv {0}, gp", out(reg) gp) };
    let user_mstatus = unsafe { (*user).mstatus };
    let k = unsafe { user.sub(1) };
    unsafe {
        k.write_bytes(0, 1);
        let f = &mut *k;
        f.regs[1] = user as u32; // sp: grows down from the saved user frame
        f.regs[2] = gp; // the trap entry already loaded the kernel gp
        f.mepc = func as usize as u32;
        f.mstatus = MSTATUS_MPP_M | MSTATUS_MPIE | (user_mstatus & MSTATUS_FS_MASK);
        f.mcause = MCAUSE_MPP_MASK | MCAUSE_MPIE;
    }
    k
}

/// Trap context, just before `mret`: when `resume` returns to U-mode,
/// restrict this hart's PMP to the current app.
pub fn before_resume(resume: &TrapFrame) {
    if !is_user_frame(resume) {
        return;
    }
    let Some(app) = current_app() else {
        kprintln!("*** resuming U-mode without an app");
        crate::halt()
    };
    let base = app.base();
    pmp::enter_app(base, base + app.rx_size, base + app.region.len());
}

/// Thread context (M-mode, kernel stack): serve the syscall saved in the
/// user frame, then resume the app.
extern "C" fn syscall_entry() -> ! {
    let app = current_app().expect("syscall from a thread without an app");
    let u = unsafe { &mut *app.user_frame() };
    let number = u.regs[16]; // a7
    let args = [u.regs[9], u.regs[10], u.regs[11], u.regs[12], u.regs[13], u.regs[14]]; // a0..a5
    if number == nr::EXIT {
        finish(Outcome::Exited(args[0] as i32));
    }
    let ret = crate::syscall::dispatch(app, number, args);
    u.regs[9] = ret as u32;
    unsafe { asm!("ecall", in("a7") call::RETURN_TO_USER, options(noreturn)) };
}

extern "C" fn kill_entry() -> ! {
    finish(Outcome::Killed)
}

/// Release the app's memory, publish the outcome and end the thread.
fn finish(outcome: Outcome) -> ! {
    let id = sched::current_id().expect("app exit outside the scheduler");
    let raw = APPS[id].swap(ptr::null_mut(), Ordering::AcqRel);
    assert!(!raw.is_null(), "app exit without an app");
    let app = unsafe { Box::from_raw(raw) };
    if let Outcome::Exited(code) = outcome {
        kprintln!("app '{}' (thread {}) exited with code {}", app.name, id, code);
    }
    let status = app.status.clone();
    drop(app);
    status.set(outcome);
    drop(status);
    sched::exit_current()
}
