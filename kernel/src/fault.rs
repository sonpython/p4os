//! Fault recovery: every fatal error ends in a crash report that survives
//! the reset, then a clean software reset of the chip.
//!
//! Fatal errors are kernel panics, CPU exceptions, stack overflows (from
//! the hardware stack guard, `stack_guard.rs`, or the canary checked on
//! every switch), and what the watchdog supervisor (`wdt.rs`) detects: a
//! critical thread that stopped checking in, a core that stopped taking
//! interrupts, or the MWDT expiring because the supervisor got no CPU.
//!
//! The crash path runs with interrupts masked and touches no lock and no
//! heap, so it works whatever state the kernel is in:
//! 1. The first core to get here owns the crash; a fault inside the crash
//!    path resets at once.
//! 2. The other core is stopped through an IPI: its next trap records the
//!    context it interrupted and spins (`enter_trap`). A core with
//!    interrupts masked for good does not answer within 20 ms, which the
//!    report says.
//! 3. The report (cause, pc/ra/sp, mcause/mtval, thread, a backtrace
//!    found by scanning the stack for return addresses, the other core's
//!    context) is written to LP SRAM with a CRC and printed raw on the
//!    console, bypassing Terminal capture.
//! 4. `LP_SYSTEM SYS_SW_RST` resets the HP system (both cores and every
//!    HP peripheral); LP SRAM keeps its contents.
//!
//! On the next boot `boot_report` prints the record and saves a one-line
//! summary in the flash settings (`sys.last_crash`), which also survives
//! power loss. A reset by a hardware watchdog leaves no record; the boot
//! then reports the ROM's reset reason instead. Shell: `crash`.

use crate::boot::TrapFrame;
use crate::irq::{self, HART_COUNT};
use crate::spin::SpinLock;
use crate::{config, console, kprintln, mmio, sched, smp, soc, stack_guard, systimer, wdt};
use alloc::string::String;
use core::arch::asm;
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// Crash record in LP SRAM (0x50108000..0x50110000). The LP ROM keeps its
/// data and stack from 0x5010FA80 up and ESP-IDF reserves the bottom for
/// its retained data; this kernel uses nothing else of LP SRAM.
const RECORD_ADDR: usize = 0x5010_E000;
const MAGIC: u32 = u32::from_le_bytes(*b"P4CR");
const VERSION: u16 = 1;
/// Flash setting holding the summary of the last crash.
pub const CONFIG_KEY: &str = "sys.last_crash";

/// LP_SYSTEM_REG_SYS_CTRL_REG and its HP system software reset bit.
const LP_SYS_CTRL: usize = soc::LPAON + 0x8;
const SYS_SW_RST: u32 = 1 << 1;
/// `rtc_get_reset_reason(cpu)` (esp_rom/esp32p4/ld/esp32p4.rom.ld).
const ROM_GET_RESET_REASON: usize = 0x4FC0_0018;

/// Memory a stack can live in: HP L2 SRAM and PSRAM.
const STACK_MEMORY: [(usize, usize); 2] = [(0x4FF0_0000, 0x4FF8_0000), (0x4800_0000, 0x4C00_0000)];
/// Bytes of stack scanned for return addresses at most.
const SCAN_BYTES: usize = 8 * 1024;
/// How long the crashing core waits for the other one to stop: longer
/// than a flash sector erase (~45 ms with interrupts masked).
const FREEZE_WAIT_US: u64 = 60_000;

const NAME_LEN: usize = 16;
/// Longest thread name read back from `RUNNING`.
const MAX_NAME: usize = 32;
const MESSAGE_LEN: usize = 120;
const BACKTRACE_LEN: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    Panic = 1,
    Exception = 2,
    StackOverflow = 3,
    HungThread = 4,
    CoreStuck = 5,
    Watchdog = 6,
    HardwareReset = 7,
}

impl Kind {
    fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => Kind::Panic,
            2 => Kind::Exception,
            3 => Kind::StackOverflow,
            4 => Kind::HungThread,
            5 => Kind::CoreStuck,
            6 => Kind::Watchdog,
            7 => Kind::HardwareReset,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Panic => "kernel panic",
            Kind::Exception => "fatal exception",
            Kind::StackOverflow => "stack overflow",
            Kind::HungThread => "hung thread",
            Kind::CoreStuck => "core hang",
            Kind::Watchdog => "watchdog timeout",
            Kind::HardwareReset => "hardware reset",
        }
    }
}

/// What the other core was doing when the crash was recorded.
const PEER_OFFLINE: u32 = 0;
const PEER_CAPTURED: u32 = 1;
const PEER_SILENT: u32 = 2;

/// The crash happened inside a trap handler.
const FLAG_IN_TRAP: u32 = 1 << 0;

/// The stored crash report. Plain words and bytes only, so it can be
/// copied to and from LP SRAM word by word.
#[repr(C)]
#[derive(Clone, Copy)]
struct Record {
    magic: u32,
    version: u16,
    size: u16,
    kind: u32,
    flags: u32,
    hart: u32,
    /// Set once the report has been shown at boot.
    reported: u32,
    /// ROM reset reason of the boot that found the record.
    reset_reason: u32,
    uptime_ms: u32,
    pc: u32,
    ra: u32,
    sp: u32,
    mcause: u32,
    mtval: u32,
    thread: [u8; NAME_LEN],
    message: [u8; MESSAGE_LEN],
    backtrace: [u32; BACKTRACE_LEN],
    peer_state: u32,
    peer_hart: u32,
    peer_pc: u32,
    peer_ra: u32,
    peer_sp: u32,
    peer_thread: [u8; NAME_LEN],
    crc: u32,
}

const RECORD_WORDS: usize = core::mem::size_of::<Record>() / 4;
const _: () = assert!(core::mem::size_of::<Record>() % 4 == 0 && core::mem::size_of::<Record>() <= 1024);

impl Record {
    fn empty() -> Self {
        // All fields are integers or integer arrays.
        unsafe { core::mem::zeroed() }
    }

    fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>()) }
    }

    fn compute_crc(&self) -> u32 {
        let b = self.bytes();
        p4os_sdk::config::crc32(&[&b[..b.len() - 4]])
    }

    fn seal(&mut self) {
        self.magic = MAGIC;
        self.version = VERSION;
        self.size = core::mem::size_of::<Self>() as u16;
        self.crc = self.compute_crc();
    }

    fn kind(&self) -> Option<Kind> {
        Kind::from_u32(self.kind)
    }
}

fn store(r: &Record) {
    let src = r as *const Record as *const u32;
    for i in 0..RECORD_WORDS {
        unsafe { core::ptr::write_volatile((RECORD_ADDR as *mut u32).add(i), src.add(i).read()) };
    }
}

fn load() -> Option<Record> {
    let mut r = Record::empty();
    let dst = &mut r as *mut Record as *mut u32;
    for i in 0..RECORD_WORDS {
        unsafe { dst.add(i).write(core::ptr::read_volatile((RECORD_ADDR as *const u32).add(i))) };
    }
    let valid = r.magic == MAGIC
        && r.version == VERSION
        && r.size as usize == core::mem::size_of::<Record>()
        && r.crc == r.compute_crc()
        && r.kind().is_some();
    valid.then_some(r)
}

fn erase() {
    unsafe { core::ptr::write_volatile(RECORD_ADDR as *mut u32, 0) };
}

/// Zero-terminated string in a fixed buffer.
fn text(buf: &[u8]) -> &str {
    let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&buf[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// Formats into a fixed buffer, silently truncating, keeping a final 0.
struct BufWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len().saturating_sub(1 + self.len);
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

fn copy_str(buf: &mut [u8], s: &str) {
    let _ = BufWriter { buf, len: 0 }.write_str(s);
}

/// Console output straight to USB, past Terminal capture and the output
/// mutex: the crash path must not block or allocate.
struct RawConsole;

impl Write for RawConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        console::write_bytes(s.as_bytes());
        Ok(())
    }
}

/// What each core runs, readable without the scheduler lock.
struct Running {
    name: AtomicUsize,
    name_len: AtomicUsize,
    stack_lo: AtomicUsize,
    stack_hi: AtomicUsize,
}

static RUNNING: [Running; HART_COUNT] = [const {
    Running {
        name: AtomicUsize::new(0),
        name_len: AtomicUsize::new(0),
        stack_lo: AtomicUsize::new(0),
        stack_hi: AtomicUsize::new(0),
    }
}; HART_COUNT];

/// Called by the scheduler, in a trap, for the thread about to run.
pub fn on_switch(hart: usize, name: &'static str, stack_bottom: usize, stack_size: usize) {
    let r = &RUNNING[hart];
    r.name.store(name.as_ptr() as usize, Ordering::Relaxed);
    r.name_len.store(name.len(), Ordering::Relaxed);
    r.stack_lo.store(stack_bottom, Ordering::Relaxed);
    r.stack_hi.store(stack_bottom + stack_size, Ordering::Relaxed);
    stack_guard::set_thread(hart, stack_bottom);
}

/// Name and stack of the thread `hart` runs ("-" and an empty range
/// before its first switch).
fn running(hart: usize) -> (&'static str, (usize, usize)) {
    let r = &RUNNING[hart];
    let ptr = r.name.load(Ordering::Relaxed);
    if ptr == 0 {
        return ("-", (0, 0));
    }
    // Pointer and length come from a `&'static str` stored by `on_switch`.
    // Read from the other core they may belong to two different names, so
    // the length is capped and the bytes checked.
    let len = r.name_len.load(Ordering::Relaxed).min(MAX_NAME);
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    let name = core::str::from_utf8(bytes).unwrap_or("?");
    (name, (r.stack_lo.load(Ordering::Relaxed), r.stack_hi.load(Ordering::Relaxed)))
}

/// Name of the calling thread, if called from a thread.
pub fn current_thread() -> Option<&'static str> {
    irq::free(|| {
        let (name, _) = running(irq::hart_id());
        (!irq::in_trap() && name != "-").then_some(name)
    })
}

/// Saved frame of the trap each core is handling, and traps taken so far.
static TRAP_FRAME: [AtomicUsize; HART_COUNT] = [const { AtomicUsize::new(0) }; HART_COUNT];
static TRAPS: [AtomicU32; HART_COUNT] = [const { AtomicU32::new(0) }; HART_COUNT];

/// Hart + 1 of the core reporting a crash, 0 while there is none.
static CRASH_OWNER: AtomicUsize = AtomicUsize::new(0);
/// Raised by the crash owner: the other core stops at its next trap.
static FREEZE: AtomicBool = AtomicBool::new(false);
static FROZEN: AtomicBool = AtomicBool::new(false);
static PEER_HART: AtomicU32 = AtomicU32::new(0);
static PEER_REGS: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
static PEER_NAME: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

/// Number of traps `hart` has taken (supervisor liveness check).
pub fn trap_count(hart: usize) -> u32 {
    TRAPS[hart].load(Ordering::Relaxed)
}

/// First thing every trap does: note the frame, stop here if the other
/// core is crashing, and report a stack overflow of the interrupted thread.
#[inline(always)]
pub fn enter_trap(frame: *mut TrapFrame) {
    let hart = irq::hart_id();
    TRAP_FRAME[hart].store(frame as usize, Ordering::Relaxed);
    // Only this core writes its own counter.
    TRAPS[hart].store(TRAPS[hart].load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    if FREEZE.load(Ordering::Acquire) {
        if CRASH_OWNER.load(Ordering::Acquire) == hart + 1 {
            // A fault inside the crash path itself.
            reset_now();
        }
        freeze_here(Some(unsafe { &*frame }));
    }
    if stack_guard::fired_here(frame as usize, unsafe { (*frame).mcause }) {
        stack_overflow(unsafe { &*frame });
    }
}

#[derive(Clone, Copy, Default)]
struct Regs {
    pc: u32,
    ra: u32,
    sp: u32,
    mcause: u32,
    mtval: u32,
}

impl Regs {
    fn of(f: &TrapFrame) -> Self {
        Regs { pc: f.mepc, ra: f.regs[0], sp: f.regs[1], mcause: f.mcause, mtval: f.mtval }
    }
}

unsafe extern "C" {
    static _irq_stack_top: u8;
}

/// Interrupt stack of `hart` (see `boot.rs`).
fn irq_stack(hart: usize) -> (usize, usize) {
    const IRQ_STACK_SIZE: usize = 1 << 13;
    let top = &raw const _irq_stack_top as usize - hart * IRQ_STACK_SIZE;
    (top - IRQ_STACK_SIZE, top)
}

/// The stack `sp` is on for `hart`, and whether that is the trap stack.
fn stack_of(hart: usize, sp: u32) -> ((usize, usize), bool) {
    let irq = irq_stack(hart);
    if (irq.0..irq.1).contains(&(sp as usize)) { (irq, true) } else { (running(hart).1, false) }
}

fn read_sp() -> u32 {
    let sp: u32;
    unsafe { asm!("mv {0}, sp", out(reg) sp) };
    sp
}

struct Report<'a> {
    kind: Kind,
    hart: usize,
    thread: &'a str,
    regs: Regs,
    /// Stack range scanned for the backtrace, from `regs.sp` up.
    stack: (usize, usize),
    flags: u32,
    /// Full register state to print (not stored).
    frame: Option<&'a TrapFrame>,
    message: fmt::Arguments<'a>,
}

pub fn panic(info: &PanicInfo) -> ! {
    let hart = irq::hart_id();
    let sp = read_sp();
    let (stack, in_trap) = stack_of(hart, sp);
    let location = info.location();
    crash(Report {
        kind: Kind::Panic,
        hart,
        thread: running(hart).0,
        regs: Regs { sp, ..Regs::default() },
        stack,
        flags: if in_trap { FLAG_IN_TRAP } else { 0 },
        frame: None,
        message: format_args!(
            "{}:{}: {}",
            location.map_or("?", |l| l.file()),
            location.map_or(0, |l| l.line()),
            info.message()
        ),
    })
}

/// A CPU exception the kernel cannot handle.
pub fn exception(f: &TrapFrame, name: &str) -> ! {
    let hart = irq::hart_id();
    let (stack, in_trap) = stack_of(hart, f.regs[1]);
    crash(Report {
        kind: Kind::Exception,
        hart,
        thread: running(hart).0,
        regs: Regs::of(f),
        stack,
        flags: if in_trap { FLAG_IN_TRAP } else { 0 },
        frame: Some(f),
        message: format_args!("{} (cause {})", name, f.mcause & 0xFFF),
    })
}

fn stack_overflow(f: &TrapFrame) -> ! {
    let hart = irq::hart_id();
    let (thread, stack) = running(hart);
    crash(Report {
        kind: Kind::StackOverflow,
        hart,
        thread,
        regs: Regs::of(f),
        stack,
        flags: 0,
        frame: Some(f),
        message: format_args!(
            "sp {:#010x} below guard {:#010x} (stack {:#010x}..{:#010x}), guard pc {:#010x}",
            f.regs[1],
            stack_guard::limit(hart),
            stack.0,
            stack.1,
            stack_guard::fired_pc(hart)
        ),
    })
}

/// The canary at the bottom of a thread stack was overwritten (checked on
/// every switch; catches overflows the guard cannot see).
pub fn stack_canary(f: &TrapFrame, thread: &str, stack_bottom: usize, stack_size: usize) -> ! {
    crash(Report {
        kind: Kind::StackOverflow,
        hart: irq::hart_id(),
        thread,
        regs: Regs::of(f),
        stack: (stack_bottom, stack_bottom + stack_size),
        flags: 0,
        frame: Some(f),
        message: format_args!("canary at {:#010x} overwritten", stack_bottom),
    })
}

/// Supervisor: watched thread `name` has not checked in for `silent` ms.
pub fn thread_hung(name: &'static str, silent: u32, limit: u32) -> ! {
    let ctx = sched::context_of(name);
    let hart = ctx.as_ref().and_then(|c| c.last_cpu).unwrap_or(irq::hart_id());
    let running_on = ctx.as_ref().and_then(|c| c.on_cpu);
    // The saved frame is only meaningful while the thread is switched out;
    // a running thread's live context comes from freezing its core.
    let regs = match &ctx {
        Some(c) if c.on_cpu.is_none() => Regs { pc: c.pc, ra: c.ra, sp: c.sp, ..Regs::default() },
        _ => Regs::default(),
    };
    crash(Report {
        kind: Kind::HungThread,
        hart,
        thread: name,
        regs,
        stack: ctx.as_ref().map_or((0, 0), |c| c.stack),
        flags: 0,
        frame: None,
        message: format_args!(
            "no check-in for {} ms (limit {} ms), {}",
            silent,
            limit,
            match running_on {
                Some(0) => "running on core 0 (its context is the core 0 line)",
                Some(_) => "running on core 1 (its context is the core 1 line)",
                None => "not running; pc/ra/sp where it last stopped",
            }
        ),
    })
}

/// Supervisor: `hart` took no trap for `quiet` ms, IPIs included.
pub fn core_stuck(hart: usize, quiet: u32) -> ! {
    // The core cannot be asked where it is; only its thread is known.
    crash(Report {
        kind: Kind::CoreStuck,
        hart,
        thread: running(hart).0,
        regs: Regs::default(),
        stack: (0, 0),
        flags: 0,
        frame: None,
        message: format_args!(
            "core {} took no interrupt for {} ms (stalled, or interrupts masked)",
            hart, quiet
        ),
    })
}

/// MWDT stage 0 interrupt: the supervisor did not feed it in time.
pub fn watchdog_expired() -> ! {
    let hart = irq::hart_id();
    let frame = TRAP_FRAME[hart].load(Ordering::Relaxed) as *const TrapFrame;
    let f = unsafe { &*frame };
    let (thread, stack) = running(hart);
    crash(Report {
        kind: Kind::Watchdog,
        hart,
        thread,
        regs: Regs::of(f),
        stack,
        flags: 0,
        frame: Some(f),
        message: format_args!(
            "supervisor did not feed the MWDT for {} ms (CPU starved or tick stopped)",
            wdt::MWDT_TIMEOUT_MS
        ),
    })
}

fn crash(r: Report) -> ! {
    irq::disable_global();
    let me = irq::hart_id();
    if let Err(owner) = CRASH_OWNER.compare_exchange(0, me + 1, Ordering::AcqRel, Ordering::Acquire) {
        if owner == me + 1 {
            reset_now();
        }
        // The other core is already reporting: stop like a frozen peer.
        let frame = irq::in_trap().then(|| unsafe { &*(TRAP_FRAME[me].load(Ordering::Relaxed) as *const TrapFrame) });
        freeze_here(frame);
    }
    let peer_state = freeze_peer(me);

    let mut rec = Record::empty();
    rec.kind = r.kind as u32;
    rec.flags = r.flags;
    rec.hart = r.hart as u32;
    // From the systimer: the kernel tick stops when core 0 hangs.
    rec.uptime_ms = (systimer::now_ticks() / (systimer::TICKS_PER_US * 1000)) as u32;
    rec.pc = r.regs.pc;
    rec.ra = r.regs.ra;
    rec.sp = r.regs.sp;
    rec.mcause = r.regs.mcause;
    rec.mtval = r.regs.mtval;
    copy_str(&mut rec.thread, r.thread);
    let _ = BufWriter { buf: &mut rec.message, len: 0 }.write_fmt(r.message);
    rec.peer_state = peer_state;
    if peer_state == PEER_CAPTURED {
        rec.peer_hart = PEER_HART.load(Ordering::Relaxed);
        rec.peer_pc = PEER_REGS[0].load(Ordering::Relaxed);
        rec.peer_ra = PEER_REGS[1].load(Ordering::Relaxed);
        rec.peer_sp = PEER_REGS[2].load(Ordering::Relaxed);
        let (ptr, len) = (PEER_NAME[0].load(Ordering::Relaxed), PEER_NAME[1].load(Ordering::Relaxed));
        if ptr != 0 {
            let name = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
            copy_str(&mut rec.peer_thread, core::str::from_utf8(name).unwrap_or("?"));
        }
    } else {
        rec.peer_hart = (1 - me) as u32;
    }
    rec.seal();
    store(&rec);
    // Scanned last: should the scan fault, the report above is kept.
    rec.backtrace = backtrace(&r.regs, r.stack);
    rec.seal();
    store(&rec);

    let mut out = RawConsole;
    let _ = writeln!(out, "\n*** FATAL ERROR, resetting");
    print(&rec, &mut out);
    if let Some(f) = r.frame {
        print_registers(f, &mut out);
    }
    let _ = writeln!(out, "*** crash report saved, resetting the chip");
    // Give the host a moment to drain the USB FIFO.
    systimer::delay_us(20_000);
    reset_now()
}

/// Ask the other core to stop and wait until it has.
fn freeze_peer(me: usize) -> u32 {
    let other = 1 - me;
    if !sched::is_online(other) {
        return PEER_OFFLINE;
    }
    FREEZE.store(true, Ordering::SeqCst);
    smp::send_ipi(other);
    let deadline = systimer::now_ticks() + FREEZE_WAIT_US * systimer::TICKS_PER_US;
    while !FROZEN.load(Ordering::Acquire) {
        if systimer::now_ticks() > deadline {
            return PEER_SILENT;
        }
    }
    PEER_CAPTURED
}

/// Record what this core was doing for the crashing core, then wait for
/// the reset.
fn freeze_here(frame: Option<&TrapFrame>) -> ! {
    irq::disable_global();
    let hart = irq::hart_id();
    let regs = frame.map_or(Regs::default(), Regs::of);
    PEER_REGS[0].store(regs.pc, Ordering::Relaxed);
    PEER_REGS[1].store(regs.ra, Ordering::Relaxed);
    PEER_REGS[2].store(regs.sp, Ordering::Relaxed);
    let (name, _) = running(hart);
    PEER_NAME[0].store(name.as_ptr() as usize, Ordering::Relaxed);
    PEER_NAME[1].store(name.len(), Ordering::Relaxed);
    PEER_HART.store(hart as u32, Ordering::Relaxed);
    FROZEN.store(true, Ordering::Release);
    loop {
        core::hint::spin_loop();
    }
}

/// Reset the HP system: both cores and all HP peripherals. LP SRAM and the
/// RWDT keep running; the RWDT is fed so the next boot has its full time.
fn reset_now() -> ! {
    wdt::feed();
    mmio::write(LP_SYS_CTRL, mmio::read(LP_SYS_CTRL) | SYS_SW_RST);
    loop {
        core::hint::spin_loop();
    }
}

/// Reset on request (`crash reboot`), with no report.
pub fn reboot() -> ! {
    irq::disable_global();
    let _ = writeln!(RawConsole, "rebooting");
    systimer::delay_us(20_000);
    reset_now()
}

unsafe extern "C" {
    static _text_start: u8;
    static _text_end: u8;
}

/// Whether `a` lies in the kernel code (only that part of the flash window
/// is mapped, so nothing else may be read as an instruction).
fn is_code(a: u32) -> bool {
    let (start, end) = (&raw const _text_start as u32, &raw const _text_end as u32);
    (start..end).contains(&a)
}

fn read16(a: u32) -> u16 {
    unsafe { core::ptr::read_volatile(a as *const u16) }
}

/// Whether `a` is a return address: in the code and right after a call
/// (`jal`/`jalr` linking `ra`, or the compressed `c.jal`/`c.jalr`).
fn is_return_address(a: u32) -> bool {
    if a & 1 != 0 || !is_code(a.wrapping_sub(4)) || !is_code(a) {
        return false;
    }
    let hi = read16(a - 2);
    let ins = read16(a - 4) as u32 | (hi as u32) << 16;
    let links_ra = (ins >> 7) & 0x1F == 1;
    let jal = ins & 0x7F == 0x6F && links_ra;
    let jalr = ins & 0x707F == 0x67 && links_ra;
    let c_jalr = hi & 0xF07F == 0x9002 && (hi >> 7) & 0x1F != 0;
    let c_jal = hi & 0xE003 == 0x2001;
    jal || jalr || c_jalr || c_jal
}

fn in_stack_memory(lo: usize, hi: usize) -> bool {
    STACK_MEMORY.iter().any(|&(s, e)| lo >= s && hi <= e)
}

/// pc, ra, then return addresses found on the stack above `sp`. A stack
/// scan rather than a frame-pointer walk (the kernel is built without
/// frame pointers), so stale addresses from earlier calls can show up.
fn backtrace(regs: &Regs, (lo, hi): (usize, usize)) -> [u32; BACKTRACE_LEN] {
    let mut out = [0u32; BACKTRACE_LEN];
    let mut n = 0;
    let mut push = |a: u32| {
        if n < BACKTRACE_LEN && (n == 0 || out[n - 1] != a) {
            out[n] = a;
            n += 1;
        }
    };
    if is_code(regs.pc) {
        push(regs.pc);
    }
    if is_return_address(regs.ra) {
        push(regs.ra);
    }
    let sp = regs.sp as usize & !3;
    // An overflowing thread's sp may sit a little below its stack.
    if lo < hi && sp >= lo.saturating_sub(4096) && sp < hi && in_stack_memory(sp, hi) {
        let end = hi.min(sp + SCAN_BYTES);
        let mut a = sp;
        while a < end {
            let w = unsafe { core::ptr::read_volatile(a as *const u32) };
            if is_return_address(w) {
                push(w);
            }
            a += 4;
        }
    }
    out
}

fn reset_reason() -> u32 {
    let f: extern "C" fn(i32) -> u32 = unsafe { core::mem::transmute(ROM_GET_RESET_REASON) };
    f(0)
}

fn reset_reason_name(r: u32) -> &'static str {
    match r {
        0x01 => "power-on",
        0x03 => "software system reset",
        0x05 => "PMU power-down / deep sleep",
        0x07 => "MWDT system reset",
        0x09 => "RWDT core reset",
        0x0B => "MWDT CPU reset",
        0x0C => "software CPU reset",
        0x0D => "RWDT CPU reset",
        0x0F => "brown-out",
        0x10 => "RWDT system reset",
        0x12 => "super watchdog reset",
        0x13 => "power glitch",
        0x14 => "eFuse CRC error",
        0x16 => "USB JTAG reset",
        0x17 => "USB UART reset (host)",
        0x18 => "JTAG CPU reset",
        0x1A => "CPU lockup",
        _ => "unknown",
    }
}

/// Resets nobody asked for: a watchdog, a brown-out, a lockup.
fn is_unexpected_reset(r: u32) -> bool {
    matches!(r, 0x07 | 0x09 | 0x0B | 0x0D | 0x0F | 0x10 | 0x12 | 0x13 | 0x1A)
}

fn print(r: &Record, w: &mut dyn Write) {
    let kind = r.kind().map_or("?", Kind::name);
    if r.kind() == Some(Kind::HardwareReset) {
        let _ = writeln!(w, "{}: {}", kind, text(&r.message));
        return;
    }
    let where_ = if r.flags & FLAG_IN_TRAP != 0 { " (in a trap handler)" } else { "" };
    let _ = writeln!(
        w,
        "{} on core {}, thread '{}'{}, {} ms after boot",
        kind,
        r.hart,
        text(&r.thread),
        where_,
        r.uptime_ms
    );
    let _ = writeln!(w, "  {}", text(&r.message));
    if r.pc != 0 || r.sp != 0 {
        let _ = writeln!(
            w,
            "  pc {:#010x} ra {:#010x} sp {:#010x} mcause {:#010x} mtval {:#010x}",
            r.pc, r.ra, r.sp, r.mcause, r.mtval
        );
    }
    if r.backtrace[0] != 0 {
        let _ = write!(w, "  backtrace:");
        for a in r.backtrace.iter().take_while(|&&a| a != 0) {
            let _ = write!(w, " {:#010x}", a);
        }
        let _ = writeln!(w);
    }
    match r.peer_state {
        PEER_CAPTURED => {
            let _ = writeln!(
                w,
                "  core {}: thread '{}' pc {:#010x} ra {:#010x} sp {:#010x}",
                r.peer_hart,
                text(&r.peer_thread),
                r.peer_pc,
                r.peer_ra,
                r.peer_sp
            );
        }
        PEER_SILENT => {
            let _ = writeln!(w, "  core {}: did not stop within 60 ms (interrupts masked)", r.peer_hart);
        }
        _ => {}
    }
}

const REG_NAMES: [&str; 31] = [
    "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "s2",
    "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
];

fn print_registers(f: &TrapFrame, w: &mut dyn Write) {
    for (i, chunk) in f.regs.chunks(4).enumerate() {
        let _ = write!(w, " ");
        for (j, value) in chunk.iter().enumerate() {
            let _ = write!(w, " {:>3}={:#010x}", REG_NAMES[i * 4 + j], value);
        }
        let _ = writeln!(w);
    }
}

fn summary(r: &Record) -> String {
    let kind = r.kind().map_or("?", Kind::name);
    let mut s = if r.kind() == Some(Kind::HardwareReset) {
        alloc::format!("{}: {}", kind, text(&r.message))
    } else {
        alloc::format!("{} in '{}' (core {}): {}", kind, text(&r.thread), r.hart, text(&r.message))
    };
    // Room for the flash setting (at most 255 bytes).
    let mut cut = s.len().min(200);
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

/// Summary of the last crash, kept for the About page and `crash`.
static LAST: SpinLock<Option<String>> = SpinLock::new(None);

/// What and where of the last recorded crash, short enough for a row on
/// the About page ("kernel panic in 'shell'"); empty if none.
pub fn last_crash() -> String {
    let last = LAST.lock();
    let s = last.as_deref().unwrap_or("");
    let head = s.split_once(": ").map_or(s, |(head, _)| head);
    head.split(" (core").next().unwrap_or(head).into()
}

/// At boot, after the settings are loaded: show the crash that caused this
/// reset, or the reset reason if a hardware watchdog (or brown-out...)
/// reset the chip without one, and remember it in the flash settings.
pub fn boot_report() {
    let reason = reset_reason();
    kprintln!("reset reason: {:#04x} ({})", reason, reset_reason_name(reason));
    let fresh = match load() {
        Some(mut r) if r.reported == 0 => {
            r.reported = 1;
            r.reset_reason = reason;
            r.seal();
            store(&r);
            Some(r)
        }
        _ if is_unexpected_reset(reason) => {
            let mut r = Record::empty();
            r.kind = Kind::HardwareReset as u32;
            r.reported = 1;
            r.reset_reason = reason;
            copy_str(&mut r.thread, "-");
            let _ = write!(
                BufWriter { buf: &mut r.message, len: 0 },
                "reset reason {:#04x} ({}), no report saved",
                reason,
                reset_reason_name(reason)
            );
            r.seal();
            store(&r);
            Some(r)
        }
        _ => None,
    };
    let summary = match fresh {
        Some(r) => {
            kprintln!("*** the previous run ended in a crash:");
            print(&r, &mut console::Console);
            let s = summary(&r);
            if let Err(e) = config::set(CONFIG_KEY, &s) {
                kprintln!("crash: could not save the summary in flash: {:?}", e);
            }
            Some(s)
        }
        None => load().map(|r| summary(&r)).or_else(|| config::get(CONFIG_KEY)),
    };
    *LAST.lock() = summary;
}

/// Shell `crash ...`.
pub fn command(args: &str) {
    let mut words = args.split_whitespace();
    match (words.next(), words.next()) {
        (None | Some("show"), _) => show(),
        (Some("clear"), _) => {
            erase();
            *LAST.lock() = None;
            match config::update(|e| {
                e.remove(CONFIG_KEY);
            }) {
                Ok(()) => kprintln!("crash report cleared"),
                Err(e) => kprintln!("crash report cleared from LP RAM; flash: {:?}", e),
            }
        }
        (Some("reboot"), _) => reboot(),
        (Some("panic"), _) => panic!("deliberate panic (crash panic)"),
        (Some("null"), _) => {
            kprintln!("reading address 0");
            let v: u32;
            unsafe { asm!("lw {0}, 0(zero)", out(reg) v) };
            kprintln!("read {:#x} from address 0?!", v);
        }
        (Some("stack"), _) => {
            kprintln!("starting a thread that recurses until its 4 KiB stack overflows");
            sched::spawn("overflow", 3, 4096, overflow, 0);
        }
        (Some("hang"), name) => {
            let name = name.unwrap_or("gui");
            match wdt::request_hang(name) {
                Ok(limit) => kprintln!("'{}' stops checking in; expect a reset within ~{} ms", name, limit + 500),
                Err(()) => kprintln!("'{}' is not a watched thread (watched: {:?})", name, wdt::watched()),
            }
        }
        (Some("irqoff"), which) => {
            let cores: &[usize] = match which {
                Some("0") => &[0],
                Some("1") => &[1],
                Some("both") => &[0, 1],
                _ => {
                    kprintln!("usage: crash irqoff 0|1|both");
                    return;
                }
            };
            for &c in cores {
                sched::spawn_on(Some(c), ["irqoff0", "irqoff1"][c], 10, 2048, spin_masked, 0);
            }
        }
        (Some("starve"), _) => {
            kprintln!("two threads at priority 10 take both cores; the MWDT should fire in {} ms", wdt::MWDT_TIMEOUT_MS);
            sched::spawn("hog0", 10, 2048, hog, 0);
            sched::spawn("hog1", 10, 2048, hog, 0);
        }
        _ => kprintln!(
            "usage: crash [show|clear|reboot] | crash panic|null|stack|hang [THREAD]|irqoff 0|1|both|starve"
        ),
    }
}

fn show() {
    match load() {
        Some(r) => {
            print(&r, &mut console::Console);
            if r.reset_reason != 0 {
                kprintln!("  reset reason at the next boot: {:#04x} ({})", r.reset_reason, reset_reason_name(r.reset_reason));
            }
        }
        None => match config::get(CONFIG_KEY) {
            Some(s) => kprintln!("last crash (from flash, details lost with power): {}", s),
            None => kprintln!("no crash recorded"),
        },
    }
    kprintln!("watched threads: {:?}", wdt::watched());
}

fn overflow(_: usize) {
    #[inline(never)]
    fn recurse(n: u32) -> u32 {
        let buf = core::hint::black_box([n; 32]);
        if core::hint::black_box(n) == u32::MAX {
            return buf[0];
        }
        recurse(n + 1).wrapping_add(buf[n as usize % 32])
    }
    kprintln!("overflow: {}", recurse(0));
}

fn spin_masked(_: usize) {
    irq::disable_global();
    loop {
        core::hint::spin_loop();
    }
}

fn hog(_: usize) {
    loop {
        core::hint::spin_loop();
    }
}
