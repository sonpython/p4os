//! Audio output: I2S1 (master) -> ES8311 codec (slave, I2C 0x18) -> NS4150B
//! speaker amplifier enabled by GPIO11.
//!
//! Pins (Guition JC4880P443C): MCLK 13, BCLK 12, LRCK 10, DOUT 9.
//! The AHB GDMA streams a ring of descriptors into the I2S TX FIFO. An
//! audio thread refills each buffer when the DMA reports it finished, and
//! pulls PCM from the active `AudioSource`.

use crate::sync::{Mutex, Semaphore};
use crate::{gpio, i2c, irq, kprintln, mmio, sched, soc};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use p4os_sdk::{AudioSource, PlaybackState};

const I2S1: usize = soc::HPPERIPH1 + 0x7000;
const AHB_DMA: usize = 0x5008_5000;
const CLK: usize = soc::HP_SYS_CLKRST;

const PIN_MCLK: u32 = 13;
const PIN_BCLK: u32 = 12;
const PIN_WS: u32 = 10;
const PIN_DOUT: u32 = 9;
const PIN_PA: u32 = 11;
const SIG_BCK: u32 = 31;
const SIG_MCLK: u32 = 32;
const SIG_WS: u32 = 33;
const SIG_SD: u32 = 34;

const CODEC: u8 = 0x18;
const IRQ_LINE: usize = 19;
const IRQ_LEVEL: u32 = 5;
const DMA_PERI_I2S1: u32 = 4;
const XTAL_HZ: u32 = 40_000_000;

const DESCRIPTORS: usize = 4;
/// Stereo 16-bit frames per descriptor (~11.6 ms at 44.1 kHz).
const FRAMES_PER_BUF: usize = 512;
const BUF_BYTES: usize = FRAMES_PER_BUF * 4;
const UNCACHED: usize = 0x4000_0000;

#[repr(C, align(64))]
struct Ring {
    desc: [[u32; 3]; DESCRIPTORS],
    _pad: [u32; 4],
    bufs: [[u8; BUF_BYTES]; DESCRIPTORS],
}

static mut RING: Ring = Ring { desc: [[0; 3]; DESCRIPTORS], _pad: [0; 4], bufs: [[0; BUF_BYTES]; DESCRIPTORS] };

static BUFFER_DONE: Semaphore = Semaphore::new(0);
static SOURCE: Mutex<Option<Box<dyn AudioSource>>> = Mutex::new(None);
static PLAYING: AtomicBool = AtomicBool::new(false);
static PAUSED: AtomicBool = AtomicBool::new(false);
/// Frames played; 32 bits last ~27 hours at 44.1 kHz (RV32 has no 64-bit atomics).
static POSITION: AtomicU32 = AtomicU32::new(0);
static VOLUME: AtomicU32 = AtomicU32::new(70);
static RATE: AtomicU32 = AtomicU32::new(0);
static READY: AtomicBool = AtomicBool::new(false);
static EOF_COUNT: AtomicU32 = AtomicU32::new(0);

fn ring_desc(i: usize) -> usize {
    (&raw mut RING as usize) + i * 12
}

fn ring_buf(i: usize) -> usize {
    (&raw mut RING as usize) + 64 + i * BUF_BYTES
}

fn field(addr: usize, hi: u32, lo: u32, value: u32) {
    let mask = (((1u64 << (hi - lo + 1)) - 1) as u32) << lo;
    mmio::write(addr, (mmio::read(addr) & !mask) | ((value << lo) & mask));
}

fn codec_write(reg: u8, val: u8) {
    if let Err(e) = i2c::write(CODEC, &[reg], &[val]) {
        kprintln!("audio: ES8311 write {:#04x} failed: {:?}", reg, e);
    }
}

fn codec_read(reg: u8) -> u8 {
    let mut v = [0u8];
    let _ = i2c::write_read(CODEC, &[reg], &mut v);
    v[0]
}

fn codec_update(reg: u8, clear: u8, set: u8) {
    let v = codec_read(reg);
    codec_write(reg, (v & !clear) | set);
}

/// ES8311 bring-up for 16-bit I2S slave with MCLK = 256 * fs, following
/// esp_codec_dev's open / set_fs / enable sequence.
fn codec_init() {
    if codec_read(0x0D) != 0xFA {
        codec_write(0x0D, 0xFA);
    }
    codec_write(0x44, 0x08);
    codec_write(0x44, 0x08);
    for (r, v) in [(0x01, 0x30), (0x02, 0x00), (0x03, 0x10), (0x16, 0x24), (0x04, 0x10), (0x05, 0x00),
                   (0x0B, 0x00), (0x0C, 0x00), (0x10, 0x1F), (0x11, 0x7F), (0x00, 0x80)] {
        codec_write(r, v);
    }
    codec_write(0x01, 0x3F); // clocks from the MCLK pin
    codec_update(0x06, 0x20, 0); // SCLK not inverted
    for (r, v) in [(0x13, 0x10), (0x1B, 0x0A), (0x1C, 0x6A), (0x44, 0x58)] {
        codec_write(r, v);
    }
    // 16-bit Philips I2S on both serial ports.
    codec_update(0x09, 0x1F, 0x0C);
    codec_update(0x0A, 0x1F, 0x0C);
    for (r, v) in [(0x02, 0x00), (0x05, 0x00), (0x03, 0x10), (0x04, 0x10)] {
        codec_write(r, v);
    }
    codec_update(0x07, 0x3F, 0x00);
    codec_write(0x08, 0xFF);
    codec_update(0x06, 0x1F, 0x03);
    // Enable: DAC unmuted, power up the analog path.
    codec_write(0x00, 0x80);
    codec_write(0x01, 0x3F);
    codec_update(0x09, 0x40, 0);
    for (r, v) in [(0x17, 0xBF), (0x0E, 0x02), (0x12, 0x00), (0x14, 0x1A), (0x0D, 0x01), (0x15, 0x40),
                   (0x37, 0x08), (0x45, 0x00)] {
        codec_write(r, v);
    }
    codec_write(0x32, 0xBF); // DAC volume 0 dB; level is applied in software
    codec_update(0x31, 0x60, 0); // unmute
}

/// Best MCLK divider XTAL / (n + b/a) for `mclk`, a <= 63.
fn mclk_divider(mclk: u32) -> (u32, u32, u32) {
    let n = XTAL_HZ / mclk;
    let mut best = (n, 0, 1, u64::MAX);
    for a in 1..64u64 {
        for b in 0..a {
            let f = XTAL_HZ as u64 * a / (n as u64 * a + b);
            let err = f.abs_diff(mclk as u64);
            if err < best.3 {
                best = (n, b as u32, a as u32, err);
            }
        }
    }
    (best.0, best.1, best.2)
}

fn set_rate(rate: u32) {
    let (n, b, a) = mclk_divider(rate * 256);
    let (yn1, x, y, z) = if b == 0 {
        (0, 0, 0, 0)
    } else if b <= a / 2 {
        (0, a / b - 1, a % b, b)
    } else {
        (1, a / (a - b) - 1, a % (a - b), a - b)
    };
    // Write a safe intermediate divider first (ESP-IDF workaround for a
    // double-division glitch), then the real one.
    field(CLK + 0x58, 7, 0, 2);
    field(CLK + 0x5C, 9, 9, 0);
    field(CLK + 0x58, 25, 17, 1);
    field(CLK + 0x5C, 8, 0, 0);
    field(CLK + 0x58, 16, 8, 0);
    field(CLK + 0x5C, 9, 9, yn1);
    field(CLK + 0x5C, 8, 0, z);
    field(CLK + 0x58, 25, 17, y);
    field(CLK + 0x58, 16, 8, x);
    field(CLK + 0x58, 7, 0, n);
    RATE.store(rate, Ordering::Relaxed);
}

fn route(pin: u32, signal: u32) {
    let pad = soc::IO_MUX + 4 + 4 * pin as usize;
    mmio::write(pad, (mmio::read(pad) & !(0x7 << 12)) | (1 << 12) | (2 << 10));
    mmio::write(soc::GPIO + 0x558 + 4 * pin as usize, signal);
    mmio::write(soc::GPIO + 0x24, 1 << pin);
}

fn i2s_init(rate: u32) {
    mmio::set_bits(CLK + 0x1C, 1 << 15);
    mmio::set_bits(soc::LP_CLKRST + 0x40, 1 << 11);
    mmio::set_bits(CLK + 0xC8, 1 << 5);
    mmio::clear_bits(CLK + 0xC8, 1 << 5);
    field(CLK + 0x54, 30, 29, 0); // TX clock from XTAL
    mmio::set_bits(CLK + 0x54, 1 << 28);
    mmio::set_bits(CLK + 0x5C, 1 << 10); // MCLK pin follows TX
    set_rate(rate);

    for (pin, sig) in [(PIN_MCLK, SIG_MCLK), (PIN_BCLK, SIG_BCK), (PIN_WS, SIG_WS), (PIN_DOUT, SIG_SD)] {
        route(pin, sig);
    }

    let conf = I2S1 + 0x24;
    mmio::set_bits(conf, 0b11);
    mmio::clear_bits(conf, 0b11);
    // Master, standard mode as 2-slot TDM, Philips one-bit delay, BCLK = MCLK / 8.
    let mut v = mmio::read(conf);
    v &= !((1 << 3) | (1 << 6) | (1 << 15) | (1 << 20) | (0x3F << 21));
    v |= (1 << 13) | (1 << 19) | (7 << 21);
    mmio::write(conf, v);
    mmio::write(I2S1 + 0x2C, 15 | (15 << 14) | (15 << 19) | (15 << 27));
    mmio::clear_bits(I2S1 + 0x40, 1 << 25); // no PCM->PDM conversion
    field(I2S1 + 0x54, 19, 16, 1);
    mmio::set_bits(I2S1 + 0x54, 0b11);
    mmio::set_bits(conf, 1 << 8);
    while mmio::read(conf) & (1 << 8) != 0 {}
}

fn dma_init() {
    for i in 0..DESCRIPTORS {
        let d = ring_desc(i) + UNCACHED;
        // owner = DMA, suc_eof so every buffer raises an EOF interrupt.
        mmio::write(d, (1 << 31) | (1 << 30) | ((BUF_BYTES as u32) << 12) | BUF_BYTES as u32);
        mmio::write(d + 4, ring_buf(i) as u32);
        mmio::write(d + 8, ring_desc((i + 1) % DESCRIPTORS) as u32);
        for w in (0..BUF_BYTES).step_by(4) {
            mmio::write(ring_buf(i) + UNCACHED + w, 0);
        }
    }
    let conf0 = AHB_DMA + 0xD0;
    mmio::set_bits(conf0, 1);
    mmio::clear_bits(conf0, 1);
    mmio::set_bits(conf0, (1 << 2) | (1 << 3));
    mmio::write(AHB_DMA + 0x100, DMA_PERI_I2S1);
    mmio::write(AHB_DMA + 0x3C, 0xFFFF_FFFF);
    mmio::write(AHB_DMA + 0x38, 1 << 1); // EOF interrupt
    irq::attach(soc::source::AHB_PDMA_OUT_CH0, IRQ_LINE, IRQ_LEVEL, on_dma_eof);
    mmio::write(AHB_DMA + 0x3B8, ring_desc(0) as u32);
    mmio::set_bits(AHB_DMA + 0xE0, 1 << 1); // outlink start
}

fn on_dma_eof() {
    mmio::write(AHB_DMA + 0x3C, mmio::read(AHB_DMA + 0x34));
    EOF_COUNT.fetch_add(1, Ordering::Relaxed);
    BUFFER_DONE.release();
}

pub fn init() {
    let rate = 44_100;
    i2s_init(rate);
    dma_init();
    mmio::set_bits(I2S1 + 0x24, 1 << 2); // tx_start
    codec_init();
    gpio::output(PIN_PA, true);
    READY.store(true, Ordering::Release);
    sched::spawn("audio", 4, 16 * 1024, audio_thread, 0);
    kprintln!("audio: I2S1 + ES8311 ready at {} Hz", rate);
}

/// Index of the descriptor the DMA just finished.
fn finished_descriptor() -> usize {
    let addr = mmio::read(AHB_DMA + 0xE8) as usize;
    (0..DESCRIPTORS).find(|&i| ring_desc(i) == addr).unwrap_or(0)
}

fn audio_thread(_: usize) {
    let mut pcm = alloc::vec![0i16; FRAMES_PER_BUF * 2];
    loop {
        BUFFER_DONE.acquire();
        let idx = finished_descriptor();
        let mut filled = 0;
        if PLAYING.load(Ordering::Acquire) && !PAUSED.load(Ordering::Relaxed) {
            let mut src = SOURCE.lock();
            if let Some(s) = src.as_mut() {
                if s.sample_rate() != RATE.load(Ordering::Relaxed) {
                    set_rate(s.sample_rate());
                }
                while filled < pcm.len() {
                    let n = s.fill(&mut pcm[filled..]);
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled < pcm.len() {
                    *src = None;
                    PLAYING.store(false, Ordering::Release);
                }
            }
        }
        let vol = VOLUME.load(Ordering::Relaxed) as i32;
        let base = ring_buf(idx) + UNCACHED;
        for f in 0..FRAMES_PER_BUF {
            let (l, r) = if 2 * f + 1 < filled { (pcm[2 * f], pcm[2 * f + 1]) } else { (0, 0) };
            let l = (l as i32 * vol / 100) as i16 as u16 as u32;
            let r = (r as i32 * vol / 100) as i16 as u16 as u32;
            mmio::write(base + f * 4, l | (r << 16));
        }
        POSITION.fetch_add((filled / 2) as u32, Ordering::Relaxed);
    }
}

/// The `Audio` service handed to apps.
#[derive(Default)]
pub struct Speaker;

impl p4os_sdk::Audio for Speaker {
    fn play(&mut self, source: Box<dyn AudioSource>) {
        if !READY.load(Ordering::Acquire) {
            kprintln!("audio: not initialised");
            return;
        }
        *SOURCE.lock() = Some(source);
        POSITION.store(0, Ordering::Relaxed);
        PAUSED.store(false, Ordering::Relaxed);
        PLAYING.store(true, Ordering::Release);
    }

    fn pause(&mut self, paused: bool) {
        PAUSED.store(paused, Ordering::Relaxed);
    }

    fn stop(&mut self) {
        PLAYING.store(false, Ordering::Release);
        *SOURCE.lock() = None;
    }

    fn state(&self) -> PlaybackState {
        match (PLAYING.load(Ordering::Acquire), PAUSED.load(Ordering::Relaxed)) {
            (false, _) => PlaybackState::Stopped,
            (true, true) => PlaybackState::Paused,
            (true, false) => PlaybackState::Playing,
        }
    }

    fn position_frames(&self) -> u64 {
        POSITION.load(Ordering::Relaxed) as u64
    }

    fn set_volume(&mut self, volume: f32) {
        VOLUME.store((volume.clamp(0.0, 1.0) * 100.0) as u32, Ordering::Relaxed);
    }

    fn volume(&self) -> f32 {
        VOLUME.load(Ordering::Relaxed) as f32 / 100.0
    }
}

/// A sine tone, used by the shell `tone` command to test the speaker.
pub struct Tone {
    pub freq: u32,
    pub frames_left: u32,
    phase: f32,
}

impl Tone {
    pub fn new(freq: u32, ms: u32) -> Self {
        Self { freq, frames_left: 44_100 * ms / 1000, phase: 0.0 }
    }
}

impl AudioSource for Tone {
    fn sample_rate(&self) -> u32 {
        44_100
    }

    fn fill(&mut self, out: &mut [i16]) -> usize {
        let step = 2.0 * core::f32::consts::PI * self.freq as f32 / 44_100.0;
        let mut n = 0;
        while n + 1 < out.len() && self.frames_left > 0 {
            let s = (libm::sinf(self.phase) * 12_000.0) as i16;
            out[n] = s;
            out[n + 1] = s;
            self.phase += step;
            if self.phase > 2.0 * core::f32::consts::PI {
                self.phase -= 2.0 * core::f32::consts::PI;
            }
            self.frames_left -= 1;
            n += 2;
        }
        n
    }
}

/// Print the audio pipeline state for debugging.
pub fn diagnose() {
    kprintln!(
        "audio: eof {} position {} playing {} rate {} | dma conf0 {:#x} link {:#x} state {:#x} int_raw {:#x} | i2s tx_conf {:#x} state {:#x} | clk ctrl15 {:#x} 16 {:#x} 17 {:#x}",
        EOF_COUNT.load(Ordering::Relaxed), POSITION.load(Ordering::Relaxed), PLAYING.load(Ordering::Relaxed),
        RATE.load(Ordering::Relaxed), mmio::read(AHB_DMA + 0xD0), mmio::read(AHB_DMA + 0xE0), mmio::read(AHB_DMA + 0xE4),
        mmio::read(AHB_DMA + 0x30), mmio::read(I2S1 + 0x24), mmio::read(I2S1 + 0x74),
        mmio::read(CLK + 0x54), mmio::read(CLK + 0x58), mmio::read(CLK + 0x5C)
    );
    let regs: alloc::vec::Vec<u8> = (0u8..0x46).map(codec_read).collect();
    kprintln!("es8311: {:02x?}", regs);
}
