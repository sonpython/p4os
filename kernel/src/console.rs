//! Kernel console over the on-chip USB-Serial-JTAG controller.

use crate::spin::SpinLock;
use crate::sync::{Mutex, Semaphore};
use crate::{irq, mmio, soc};
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const EP1: usize = soc::USB_SERIAL_JTAG;
const EP1_CONF: usize = soc::USB_SERIAL_JTAG + 0x4;
const WR_DONE: u32 = 1 << 0;
const IN_EP_DATA_FREE: u32 = 1 << 1;
const OUT_EP_DATA_AVAIL: u32 = 1 << 2;
const INT_ENA: usize = soc::USB_SERIAL_JTAG + 0x10;
const INT_CLR: usize = soc::USB_SERIAL_JTAG + 0x14;
const SERIAL_OUT_RECV_PKT: u32 = 1 << 2;
const LINE: usize = 17;
/// Ctrl-T prints the thread table from interrupt context, even when every
/// thread is stuck.
const CTRL_T: u8 = 0x14;
const LEVEL: u32 = 2;

/// Receive ring: one producer (the interrupt handler) and possibly several
/// consumers (the shell and the GUI, which may run on different cores).
struct RxRing {
    buf: UnsafeCell<[u8; 256]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

unsafe impl Sync for RxRing {}

static RX: RxRing = RxRing {
    buf: UnsafeCell::new([0; 256]),
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
};

fn on_rx() {
    while mmio::read(EP1_CONF) & OUT_EP_DATA_AVAIL != 0 {
        let byte = mmio::read(EP1) as u8;
        if byte == CTRL_T {
            crate::shell::dump_threads();
            continue;
        }
        let head = RX.head.load(Ordering::Relaxed);
        let next = (head + 1) % 256;
        if next != RX.tail.load(Ordering::Acquire) {
            unsafe { (*RX.buf.get())[head] = byte };
            RX.head.store(next, Ordering::Release);
        }
    }
    mmio::write(INT_CLR, SERIAL_OUT_RECV_PKT);
    RX_READY.release();
}

static RX_READY: Semaphore = Semaphore::new(0);

pub fn init_rx() {
    mmio::write(INT_CLR, SERIAL_OUT_RECV_PKT);
    mmio::set_bits(INT_ENA, SERIAL_OUT_RECV_PKT);
    irq::attach(soc::source::USB_SERIAL_JTAG, LINE, LEVEL, on_rx);
}

/// Block the calling thread until a byte arrives.
pub fn read_byte_blocking() -> u8 {
    loop {
        if let Some(b) = read_byte() {
            return b;
        }
        RX_READY.acquire();
    }
}

pub fn read_byte() -> Option<u8> {
    loop {
        let tail = RX.tail.load(Ordering::Acquire);
        if tail == RX.head.load(Ordering::Acquire) {
            return None;
        }
        // The producer does not reuse the slot before `tail` moves past it,
        // so reading first and then claiming it is safe; a consumer that
        // loses the race simply retries.
        let byte = unsafe { (*RX.buf.get())[tail] };
        if RX.tail.compare_exchange(tail, (tail + 1) % 256, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            return Some(byte);
        }
    }
}

/// Spin budget per byte. When no host has the port open the FIFO never
/// drains, and the kernel must keep running instead of hanging on output.
const FIFO_TIMEOUT: u32 = 50_000;

/// Set after a timeout: until the FIFO drains again, output is dropped
/// without spinning, so printing stays cheap when nobody listens (some
/// callers print with interrupts masked).
static HOST_GONE: AtomicBool = AtomicBool::new(false);

fn write_byte(byte: u8) -> bool {
    let budget = if HOST_GONE.load(Ordering::Relaxed) { 1 } else { FIFO_TIMEOUT };
    let mut spins = 0;
    while mmio::read(EP1_CONF) & IN_EP_DATA_FREE == 0 {
        spins += 1;
        if spins >= budget {
            HOST_GONE.store(true, Ordering::Relaxed);
            return false;
        }
    }
    HOST_GONE.store(false, Ordering::Relaxed);
    mmio::write(EP1, byte as u32);
    true
}

pub fn write_bytes(bytes: &[u8]) {
    for &b in bytes {
        if b == b'\n' && !write_byte(b'\r') {
            break;
        }
        if !write_byte(b) {
            break;
        }
    }
    mmio::write(EP1_CONF, WR_DONE);
}

/// Exclusive use of the USB port for a binary transfer, from
/// [`binary_transfer`]. Bytes go out untranslated (no CR before LF).
pub struct BinaryWriter {
    /// Give up when the FIFO has not drained for this long.
    stall_ticks: u64,
}

/// A transfer gave up: the host stopped reading.
#[derive(Debug)]
pub struct Stalled;

impl BinaryWriter {
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), Stalled> {
        let mut last_progress = crate::systimer::now_ticks();
        for &b in bytes {
            while mmio::read(EP1_CONF) & IN_EP_DATA_FREE == 0 {
                if crate::systimer::now_ticks() - last_progress > self.stall_ticks {
                    HOST_GONE.store(true, Ordering::Relaxed);
                    return Err(Stalled);
                }
            }
            mmio::write(EP1, b as u32);
            last_progress = crate::systimer::now_ticks();
        }
        HOST_GONE.store(false, Ordering::Relaxed);
        Ok(())
    }
}

/// Run `f` with the USB port to itself. Other threads' output waits until
/// it returns (it must not print). Interrupt handlers and spinlock holders
/// still print directly, so the stream needs its own framing and checksum.
/// Not available while output is being captured for the Terminal app.
pub fn binary_transfer<R>(stall_ms: u32, f: impl FnOnce(&mut BinaryWriter) -> R) -> R {
    let _out = OUTPUT.lock();
    let mut w = BinaryWriter { stall_ticks: stall_ms as u64 * 1000 * crate::systimer::TICKS_PER_US };
    let r = f(&mut w);
    mmio::write(EP1_CONF, WR_DONE);
    r
}

pub struct Console;

/// When set, kernel output is collected here instead of going to USB (used
/// to show shell command output inside the Terminal app).
static CAPTURE: SpinLock<Option<alloc::string::String>> = SpinLock::new(None);

/// Serialises USB output from threads on both cores, so each `write_str`
/// lands in one piece. A sleeping lock, because waiting for the host to
/// drain the FIFO can take a USB frame (~1 ms), far too long to spend with
/// interrupts masked.
static OUTPUT: Mutex<()> = Mutex::new(());

pub fn capture_start() {
    *CAPTURE.lock() = Some(alloc::string::String::new());
}

pub fn capture_end() -> alloc::string::String {
    CAPTURE.lock().take().unwrap_or_default()
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // An exception raised while this core holds the capture lock must
        // still be able to report itself, and trap handlers (Ctrl-T dump,
        // fatal exceptions) may not grow the buffer: the heap is off limits
        // in interrupt context.
        if !irq::in_trap() && CAPTURE.owner() != Some(irq::hart_id()) {
            if let Some(buf) = &mut *CAPTURE.lock() {
                buf.push_str(s);
                return Ok(());
            }
        }
        // Interrupt handlers, spinlock holders and boot code cannot block:
        // they print directly and may interleave with a thread's output.
        if irq::enabled() && crate::sched::in_thread() {
            let _out = OUTPUT.lock();
            write_bytes(s.as_bytes());
        } else {
            write_bytes(s.as_bytes());
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        let _ = core::fmt::Write::write_fmt(&mut $crate::console::Console, format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{
        $crate::kprint!($($arg)*);
        $crate::kprint!("\n");
    }};
}
