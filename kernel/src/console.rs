//! Kernel console over the on-chip USB-Serial-JTAG controller.

use crate::sync::Semaphore;
use crate::{irq, mmio, soc};
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

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

/// Single-producer (interrupt) single-consumer (kernel) receive ring.
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
    let tail = RX.tail.load(Ordering::Relaxed);
    if tail == RX.head.load(Ordering::Acquire) {
        return None;
    }
    let byte = unsafe { (*RX.buf.get())[tail] };
    RX.tail.store((tail + 1) % 256, Ordering::Release);
    Some(byte)
}

/// Spin budget per byte. When no host has the port open the FIFO never
/// drains, and the kernel must keep running instead of hanging on output.
const FIFO_TIMEOUT: u32 = 50_000;

fn write_byte(byte: u8) -> bool {
    let mut spins = 0;
    while mmio::read(EP1_CONF) & IN_EP_DATA_FREE == 0 {
        spins += 1;
        if spins == FIFO_TIMEOUT {
            return false;
        }
    }
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

pub struct Console;

/// When set, kernel output is collected here instead of going to USB (used
/// to show shell command output inside the Terminal app).
struct Capture(UnsafeCell<Option<alloc::string::String>>);
unsafe impl Sync for Capture {}
static CAPTURE: Capture = Capture(UnsafeCell::new(None));

pub fn capture_start() {
    irq::free(|| unsafe { *CAPTURE.0.get() = Some(alloc::string::String::new()) });
}

pub fn capture_end() -> alloc::string::String {
    irq::free(|| unsafe { (*CAPTURE.0.get()).take().unwrap_or_default() })
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let captured = irq::free(|| unsafe {
            match &mut *CAPTURE.0.get() {
                Some(buf) => {
                    buf.push_str(s);
                    true
                }
                None => false,
            }
        });
        if !captured {
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
