//! Guition JC4880P443C 4.3" 480x800 portrait MIPI-DSI panel (ST7701S
//! controller) in DSI video mode.
//!
//! Pipeline: PSRAM framebuffer -> DW-GDMA -> DSI bridge FIFO -> DSI host ->
//! D-PHY (2 lanes x 1 Gbps) -> panel. The DMA descriptor covers one frame
//! and is re-armed from the transfer-done interrupt, which is also where a
//! newly presented buffer becomes visible (tear-free page flip).
//!
//! The register sequence follows ESP-IDF v5.5 (esp_lcd dsi + HAL). Panel
//! parameters were cross-checked three ways: registers read over JTAG from
//! the running factory firmware, the init table extracted from the factory
//! image, and the vendor's JC4880P443 board config and schematic (LCD reset
//! on GPIO5 -> panel RESX, backlight boost enable on GPIO23).

use crate::sched::{self, notify_bits};
use crate::{gpio, irq, kprintln, mmio, psram, soc, systimer};
use alloc::alloc::{alloc_zeroed, Layout};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub const WIDTH: usize = 480;
pub const HEIGHT: usize = 800;
pub const FB_BYTES: usize = WIDTH * HEIGHT * 2;

const BACKLIGHT_PINS: [u32; 1] = [23];
/// LCD reset, active low (vendor schematic: GPIO5 -> panel RESX).
const RESET_PIN: u32 = 5;

/// 2 data lanes at 500 Mbps: lane byte clock 62.5 MHz.
const LANE_MBPS: u32 = 500;
/// DPI pixel clock = PLL_F240M / (DPI_DIV + 1) = 30 MHz.
const DPI_DIV: u32 = 7;
const HSYNC: u32 = 12;
const HBP: u32 = 42;
const HFP: u32 = 42;
const VSYNC: u32 = 2;
const VBP: u32 = 8;
const VFP: u32 = 166;

/// ST7701S init table for this panel (vendor esp_lcd_st7701_mipi.c default
/// table, identical to the one in the factory image). Each entry is
/// (command, parameters, delay in ms after sending).
const ST7701_INIT: &[(u8, &[u8], u32)] = &[
    (0xFF, &[0x77, 0x01, 0x00, 0x00, 0x13], 0),
    (0xEF, &[0x08], 0),
    (0xFF, &[0x77, 0x01, 0x00, 0x00, 0x10], 0),
    (0xC0, &[0x63, 0x00], 0),
    (0xC1, &[0x0D, 0x02], 0),
    (0xC2, &[0x10, 0x08], 0),
    (0xCC, &[0x10], 0),
    (0xB0, &[0x80, 0x09, 0x53, 0x0C, 0xD0, 0x07, 0x0C, 0x09, 0x09, 0x28, 0x06, 0xD4, 0x13, 0x69, 0x2B, 0x71], 0),
    (0xB1, &[0x80, 0x94, 0x5A, 0x10, 0xD3, 0x06, 0x0A, 0x08, 0x08, 0x25, 0x03, 0xD3, 0x12, 0x66, 0x6A, 0x0D], 0),
    (0xFF, &[0x77, 0x01, 0x00, 0x00, 0x11], 0),
    (0xB0, &[0x5D], 0),
    (0xB1, &[0x58], 0),
    (0xB2, &[0x87], 0),
    (0xB3, &[0x80], 0),
    (0xB5, &[0x4E], 0),
    (0xB7, &[0x85], 0),
    (0xB8, &[0x21], 0),
    (0xB9, &[0x10, 0x1F], 0),
    (0xBB, &[0x03], 0),
    (0xBC, &[0x00], 0),
    (0xC1, &[0x78], 0),
    (0xC2, &[0x78], 0),
    (0xD0, &[0x88], 0),
    (0xE0, &[0x00, 0x3A, 0x02], 0),
    (0xE1, &[0x04, 0xA0, 0x00, 0xA0, 0x05, 0xA0, 0x00, 0xA0, 0x00, 0x40, 0x40], 0),
    (0xE2, &[0x30, 0x00, 0x40, 0x40, 0x32, 0xA0, 0x00, 0xA0, 0x00, 0xA0, 0x00, 0xA0, 0x00], 0),
    (0xE3, &[0x00, 0x00, 0x33, 0x33], 0),
    (0xE4, &[0x44, 0x44], 0),
    (0xE5, &[0x09, 0x2E, 0xA0, 0xA0, 0x0B, 0x30, 0xA0, 0xA0, 0x05, 0x2A, 0xA0, 0xA0, 0x07, 0x2C, 0xA0, 0xA0], 0),
    (0xE6, &[0x00, 0x00, 0x33, 0x33], 0),
    (0xE7, &[0x44, 0x44], 0),
    (0xE8, &[0x08, 0x2D, 0xA0, 0xA0, 0x0A, 0x2F, 0xA0, 0xA0, 0x04, 0x29, 0xA0, 0xA0, 0x06, 0x2B, 0xA0, 0xA0], 0),
    (0xEB, &[0x00, 0x00, 0x4E, 0x4E, 0x00, 0x00, 0x00], 0),
    (0xEC, &[0x08, 0x01], 0),
    (0xED, &[0xB0, 0x2B, 0x98, 0xA4, 0x56, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xF7, 0x65, 0x4A, 0x89, 0xB2, 0x0B], 0),
    (0xEF, &[0x08, 0x08, 0x08, 0x45, 0x3F, 0x54], 0),
    (0xFF, &[0x77, 0x01, 0x00, 0x00, 0x00], 0),
    (0x11, &[0x00], 120),
    (0x3A, &[0x55], 0), // COLMOD: 16-bit RGB565
    (0x36, &[0x00], 0), // MADCTL: default scan direction
    (0x29, &[0x00], 20),
];

const CLKRST: usize = soc::HP_SYS_CLKRST;
const HOST: usize = soc::DSI_HOST;
const BRG: usize = soc::DSI_BRG;
const DMAC: usize = soc::DW_GDMA;
const CH: usize = DMAC + 0x100; // channel index 0

const IRQ_LINE: usize = 18;
/// Fast interrupt (see `irq`): the next frame's DMA is started from the
/// handler, so thread-side critical sections must not delay it.
const IRQ_LEVEL: u32 = 6;
const _: () = assert!(IRQ_LEVEL > irq::KERNEL_LEVEL);

/// Non-cacheable alias of internal SRAM, used for the DMA descriptor.
const SRAM_UNCACHED_OFFSET: usize = 0x4000_0000;

#[repr(C, align(64))]
struct Lli([u32; 16]);

static mut LLI: Lli = Lli([0; 16]);

const LLI_CTL_LO: u32 = 0x001E_1B41;
const LLI_CTL_HI: u32 = 0xC010_8840;
const BLOCK_TS: u32 = (FB_BYTES * 8 / 64 - 1) as u32;

static FRONT: AtomicUsize = AtomicUsize::new(0);
static PENDING: AtomicUsize = AtomicUsize::new(0);
static FRAMES: AtomicU32 = AtomicU32::new(0);
static UNDERRUNS: AtomicU32 = AtomicU32::new(0);
/// Thread blocked in `present`, notified when its buffer goes on screen.
/// Written before `PENDING`, so the handler that takes `PENDING` sees it.
static FLIP_WAITER: AtomicUsize = AtomicUsize::new(0);

fn field(addr: usize, hi: u32, lo: u32, value: u32) {
    let mask = (((1u64 << (hi - lo + 1)) - 1) as u32) << lo;
    mmio::modify(addr, mask, value << lo);
}

fn lli_word(i: usize, v: u32) {
    let base = &raw mut LLI as usize + SRAM_UNCACHED_OFFSET;
    mmio::write(base + i * 4, v);
}

/// LDO channel 3 at 2.5 V powers the D-PHY. The dref/mul pair is the
/// eFuse-trimmed value the factory firmware programmed on this chip.
fn phy_power_on() {
    mmio::write(soc::PMU + 0x1C4, 0xE580_0000);
    mmio::write(soc::PMU + 0x1C0, 0x4020_0180);
    systimer::delay_us(100);
}

fn phy_write(addr: u32, val: u32) {
    mmio::write(HOST + 0xB4, 0);
    mmio::write(HOST + 0xB8, 0x10000 | addr);
    mmio::write(HOST + 0xB4, 2);
    mmio::write(HOST + 0xB4, 0);
    mmio::write(HOST + 0xB8, val);
    mmio::write(HOST + 0xB4, 2);
    mmio::write(HOST + 0xB4, 0);
}

fn wait(what: &str, mut ok: impl FnMut() -> bool) -> bool {
    let end = systimer::now_ticks() + 100_000 * systimer::TICKS_PER_US;
    while !ok() {
        if systimer::now_ticks() > end {
            kprintln!("display: timeout waiting for {}", what);
            return false;
        }
    }
    true
}

fn host_and_phy() -> bool {
    mmio::set_bits(CLKRST + 0x18, 1 << 12);
    mmio::set_bits(CLKRST + 0xC0, 1 << 26);
    mmio::clear_bits(CLKRST + 0xC0, 1 << 26);
    mmio::set_bits(CLKRST + 0x2C, 1 << 8);
    field(CLKRST + 0x38, 31, 30, 0);
    mmio::set_bits(CLKRST + 0x3C, 0b11);

    field(HOST + 0xA4, 1, 0, 1); // 2 data lanes
    mmio::write(HOST + 0x04, 1);
    mmio::write(HOST + 0xA0, 0x1);
    mmio::write(HOST + 0xA0, 0x1);
    mmio::write(HOST + 0xA0, 0x3);
    mmio::write(HOST + 0xA0, 0x7);
    mmio::write(HOST + 0xA0, 0xF);

    // D-PHY PLL: 20 MHz ref x M/N = 20 x 50 / 2 = 500 Mbps per lane,
    // hsfreqrange 0x07 for [500, 550) Mbps.
    let (n, m) = (2u32, 50u32);
    for (a, v) in [(0x44, 0x07 << 1), (0x19, 0x30), (0x17, n - 1), (0x18, (m - 1) & 0x1F), (0x18, 0x80 | ((m - 1) >> 5))] {
        phy_write(a, v);
    }
    if !wait("PHY PLL lock", || mmio::read(HOST + 0xB0) & 1 != 0) {
        return false;
    }
    if !wait("lanes stop state", || mmio::read(HOST + 0xB0) & 0x94 == 0x94) {
        return false;
    }

    mmio::write(HOST + 0x34, 1); // command mode
    mmio::write(HOST + 0x94, 0);
    mmio::write(HOST + 0x9C, 0x0032_0068);
    mmio::write(HOST + 0x98, 0x002E_0080);
    mmio::write(HOST + 0x2C, 0x1D); // EoTp tx, BTA, ECC and CRC rx
    // Timeout clock ~10 MHz and escape clock ~18 MHz from the lane byte clock.
    let to_div = LANE_MBPS / 8 / 10;
    let esc_div = LANE_MBPS / 8 / 18;
    mmio::write(HOST + 0x08, (to_div << 8) | esc_div);
    mmio::write(HOST + 0xF4, 6000);
    field(HOST + 0xA4, 15, 8, 0x3F);
    // All commands in LP mode with an acknowledge request after each, as
    // the factory firmware does. Timeouts are armed so a silent panel shows
    // up as an error instead of hanging the host.
    mmio::write(HOST + 0x68, 0x010F_7F02);
    mmio::write(HOST + 0x78, 0x0FFF_0FFF); // HS TX / LP RX timeouts
    mmio::write(HOST + 0x88, 0x0FFF); // LP write timeout
    mmio::write(HOST + 0x8C, 0x0FFF); // BTA timeout
    true
}

/// Send one DCS write: short packet for up to one parameter, otherwise a
/// long packet through the payload FIFO.
fn dcs_write(cmd: u8, params: &[u8]) {
    wait("command FIFO", || mmio::read(HOST + 0x74) & (1 << 1) == 0);
    match params.len() {
        0 => mmio::write(HOST + 0x6C, ((cmd as u32) << 8) | 0x05),
        1 => mmio::write(HOST + 0x6C, ((params[0] as u32) << 16) | ((cmd as u32) << 8) | 0x15),
        n => {
            let mut bytes = alloc::vec::Vec::with_capacity(n + 1);
            bytes.push(cmd);
            bytes.extend_from_slice(params);
            for chunk in bytes.chunks(4) {
                let mut w = [0u8; 4];
                w[..chunk.len()].copy_from_slice(chunk);
                wait("payload FIFO", || mmio::read(HOST + 0x74) & (1 << 3) == 0);
                mmio::write(HOST + 0x70, u32::from_le_bytes(w));
            }
            mmio::write(HOST + 0x6C, (((n + 1) as u32) << 8) | 0x39);
        }
    }
    wait("command sent", || mmio::read(HOST + 0x74) & 0b101 == 0b101);
    let (e0, e1) = (mmio::read(HOST + 0xBC), mmio::read(HOST + 0xC0));
    if e0 | e1 != 0 {
        kprintln!("display: cmd {:#04x} errors int_st0={:#x} int_st1={:#x}", cmd, e0, e1);
    }
}

fn panel_init() {
    // High 5 ms, low 10 ms, high, then 120 ms for the controller to wake.
    gpio::output(RESET_PIN, true);
    systimer::delay_us(5_000);
    gpio::set(RESET_PIN, false);
    systimer::delay_us(10_000);
    gpio::set(RESET_PIN, true);
    systimer::delay_us(120_000);
    for &(cmd, params, delay_ms) in ST7701_INIT {
        dcs_write(cmd, params);
        if delay_ms > 0 {
            systimer::delay_us(delay_ms as u64 * 1000);
        }
    }
}

fn video_timing() {
    mmio::set_bits(CLKRST + 0x28, 1 << 30);
    field(CLKRST + 0x3C, 6, 5, 1); // PLL_F240M
    field(CLKRST + 0x3C, 15, 8, DPI_DIV);
    mmio::set_bits(CLKRST + 0x3C, 1 << 7);

    // Horizontal host timings are in lane-byte clocks, truncated like
    // ESP-IDF: px * (lane byte clock / pixel clock).
    let dpi_khz = 240_000 / (DPI_DIV + 1);
    let lbc_khz = LANE_MBPS * 1000 / 8;
    let to_lbc = |px: u32| (px as u64 * lbc_khz as u64 / dpi_khz as u64) as u32;
    let htotal = WIDTH as u32 + HSYNC + HBP + HFP;
    let vtotal = HEIGHT as u32 + VSYNC + VBP + VFP;

    mmio::write(HOST + 0x0C, 0);
    mmio::write(HOST + 0x10, 0); // RGB565
    mmio::write(HOST + 0x14, 0);
    mmio::write(HOST + 0x38, 0x0000_FF02);
    mmio::write(HOST + 0x3C, WIDTH as u32);
    mmio::write(HOST + 0x40, 0);
    mmio::write(HOST + 0x44, 0);
    mmio::write(HOST + 0x48, to_lbc(HSYNC));
    mmio::write(HOST + 0x4C, to_lbc(HBP));
    mmio::write(HOST + 0x50, to_lbc(htotal));
    mmio::write(HOST + 0x54, VSYNC);
    mmio::write(HOST + 0x58, VBP);
    mmio::write(HOST + 0x60, HEIGHT as u32);
    mmio::write(HOST + 0x5C, VFP);

    mmio::write(BRG + 0x18, 0x2);
    mmio::write(BRG + 0x38, ((WIDTH as u32) << 16) | htotal);
    mmio::write(BRG + 0x3C, (HSYNC << 16) | HBP);
    mmio::write(BRG + 0x30, ((HEIGHT as u32) << 16) | vtotal);
    mmio::write(BRG + 0x34, (VSYNC << 16) | VBP);
    mmio::write(BRG + 0x0C, 0x8000_0000 | (FB_BYTES * 8 / 64) as u32);
    mmio::write(BRG + 0x40, (WIDTH as u32) << 4);
    mmio::write(BRG + 0x88, 0x10);
    mmio::clear_bits(BRG + 0x6C, 1 << 28);
    mmio::write(BRG + 0x08, 256);
    mmio::write(BRG + 0x8C, 768);
    mmio::write(BRG + 0x04, 1);
    mmio::write(BRG + 0x44, 1);
}

fn dma_init() {
    mmio::set_bits(CLKRST + 0x14, 1 << 13);
    mmio::set_bits(CLKRST + 0x18, 1 << 5);
    mmio::set_bits(CLKRST + 0xC0, 1 << 21);
    mmio::clear_bits(CLKRST + 0xC0, 1 << 21);
    mmio::write(DMAC + 0x58, 1);
    wait("DMA reset", || mmio::read(DMAC + 0x58) & 1 == 0);
    mmio::write(DMAC + 0x10, 0x3);
    field(CH + 0x20, 3, 0, 0xF); // linked-list multi-block on both ends
    mmio::write(CH + 0x24, 0x0A02_0001);
    mmio::write(CH + 0x80, 0xFFFF_FFFF);
    mmio::write(CH + 0x90, 1 << 1);
}

/// Point the descriptor at `fb` and start one frame's transfer.
fn dma_start(fb: usize) {
    lli_word(0, fb as u32);
    lli_word(1, 0);
    lli_word(2, soc::DSI_BRG_FIFO as u32);
    lli_word(3, 0);
    lli_word(4, BLOCK_TS);
    lli_word(6, 1);
    lli_word(7, 0);
    lli_word(8, LLI_CTL_LO);
    lli_word(9, LLI_CTL_HI);
    mmio::write(CH + 0x28, (&raw const LLI as usize as u32 & !0x3F) | 1);
    mmio::write(CH + 0x2C, 0);
    mmio::write(DMAC + 0x18, 0x101);
}

fn on_frame_done() {
    let st = mmio::read(CH + 0x88);
    mmio::write(CH + 0x98, st);
    let brg = mmio::read(BRG + 0x5C);
    mmio::write(BRG + 0x54, brg);
    if brg & 1 != 0 {
        UNDERRUNS.fetch_add(1, Ordering::Relaxed);
    }
    let pending = PENDING.swap(0, Ordering::AcqRel);
    if pending != 0 {
        FRONT.store(pending, Ordering::Release);
        sched::notify(FLIP_WAITER.load(Ordering::Relaxed), notify_bits::DISPLAY_FLIP);
    }
    FRAMES.fetch_add(1, Ordering::Relaxed);
    dma_start(FRONT.load(Ordering::Acquire));
}

pub struct Framebuffer {
    pub ptr: *mut u16,
}

impl Framebuffer {
    pub fn pixels(&mut self) -> &mut [u16] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr, WIDTH * HEIGHT) }
    }
}

fn alloc_fb() -> *mut u16 {
    let layout = Layout::from_size_align(FB_BYTES, 128).unwrap();
    let p = unsafe { alloc_zeroed(layout) } as *mut u16;
    assert!(!p.is_null(), "no memory for framebuffer");
    p
}

/// Full bring-up with two PSRAM framebuffers. Returns them as (front, back).
pub fn init() -> Option<(Framebuffer, Framebuffer)> {
    for pin in BACKLIGHT_PINS {
        gpio::output(pin, false);
    }
    phy_power_on();
    if !host_and_phy() {
        return None;
    }
    panel_init();
    video_timing();
    dma_init();
    let front = alloc_fb();
    let back = alloc_fb();
    psram::cache_writeback(front as usize, FB_BYTES);
    FRONT.store(front as usize, Ordering::Release);
    irq::attach(soc::source::DW_GDMA, IRQ_LINE, IRQ_LEVEL, on_frame_done);
    dma_start(front as usize);
    mmio::write(HOST + 0x34, 0); // video mode
    mmio::write(HOST + 0x94, 0x3);
    mmio::set_bits(BRG + 0x40, 1);
    mmio::write(BRG + 0x44, 1);
    mmio::set_bits(BRG + 0x50, 1);
    crate::backlight::init();
    Some((Framebuffer { ptr: front }, Framebuffer { ptr: back }))
}

/// Make `fb` visible at the next frame boundary and block until it is.
/// After this returns the previous front buffer is free to draw into.
pub fn present(fb: &Framebuffer) {
    psram::cache_writeback(fb.ptr as usize, FB_BYTES);
    FLIP_WAITER.store(sched::my_id(), Ordering::Relaxed);
    PENDING.store(fb.ptr as usize, Ordering::Release);
    sched::wait_notify(notify_bits::DISPLAY_FLIP);
}

/// Print pipeline health: frame/underrun counters and raw status registers.
pub fn diagnose() {
    kprintln!(
        "usb_wrap otg_conf {:#x}, gpio out {:#x}, enable {:#x}",
        mmio::read(0x5008_0000), mmio::read(soc::GPIO + 0x04), mmio::read(soc::GPIO + 0x20)
    );
    kprintln!("frames {} underruns {}", FRAMES.load(Ordering::Relaxed), UNDERRUNS.load(Ordering::Relaxed));
    kprintln!(
        "host: pwr {:#x} mode {:#x} phy_status {:#x} int_st0 {:#x} int_st1 {:#x} cmd_status {:#x} lpclk {:#x}",
        mmio::read(HOST + 0x04), mmio::read(HOST + 0x34), mmio::read(HOST + 0xB0),
        mmio::read(HOST + 0xBC), mmio::read(HOST + 0xC0), mmio::read(HOST + 0x74), mmio::read(HOST + 0x94)
    );
    kprintln!(
        "bridge: en {:#x} misc {:#x} int_raw {:#x} status {:#x} | dma ch_en {:#x} ch_status {:#x} intstat {:#x}",
        mmio::read(BRG + 0x04), mmio::read(BRG + 0x40), mmio::read(BRG + 0x58), mmio::read(BRG + 0x5C),
        mmio::read(DMAC + 0x18), mmio::read(CH + 0x30), mmio::read(CH + 0x88)
    );
    kprintln!(
        "clk: soc0 {:#x} soc1 {:#x} peri02 {:#x} peri03 {:#x} | ldo3 {:#x}/{:#x}",
        mmio::read(CLKRST + 0x14), mmio::read(CLKRST + 0x18), mmio::read(CLKRST + 0x38), mmio::read(CLKRST + 0x3C),
        mmio::read(soc::PMU + 0x1C0), mmio::read(soc::PMU + 0x1C4)
    );
}

/// Switch the DSI host's built-in pattern generator on (vertical colour
/// bars) or off. The bridge stops feeding framebuffer pixels meanwhile, so
/// this isolates the panel link from the DMA/PSRAM path.
pub fn test_pattern(on: bool) {
    if on {
        mmio::clear_bits(BRG + 0x40, 1);
        mmio::write(BRG + 0x44, 1);
        let v = mmio::read(HOST + 0x38) & !(0b111 << 16);
        mmio::write(HOST + 0x38, v | (1 << 16)); // vpg_en, colour bars, vertical
    } else {
        mmio::clear_bits(HOST + 0x38, 1 << 16);
        mmio::set_bits(BRG + 0x40, 1);
        mmio::write(BRG + 0x44, 1);
    }
}

/// Solid colour that replaces the desktop while set (0 = off, otherwise
/// RGB565 | 0x1_0000). Used by the camera-based display self-test.
pub static OVERRIDE: AtomicU32 = AtomicU32::new(0);
