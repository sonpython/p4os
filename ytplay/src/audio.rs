//! Opus audio: a decoder worker thread turns demuxed packets into PCM, and
//! a light [`AudioSource`] hands that PCM to the OS audio thread while
//! counting the samples played, which is the media clock video follows.
//!
//! Decoding does not happen in the audio thread itself: opus-rs (`no_std`
//! build) needs about 72 KB of stack to set up its CELT mode and ~50 KB per
//! decode, far more than the OS audio thread has.

use crate::spsc::{Consumer, Producer};
use crate::webm::Packet;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use opus_rs::OpusDecoder;
use p4os_sdk::{AudioSource, Worker};

pub const RATE: u32 = 48_000;
/// Largest Opus packet: 120 ms at 48 kHz.
const MAX_FRAME: usize = 5760;
/// Stack for the decoder thread (see the module docs).
pub const DECODER_STACK: usize = 128 * 1024;
/// Decoded samples buffered ahead of the speaker: 2 s of stereo, one
/// 375 KiB ring (in PSRAM) rather than many small chunks (in SRAM).
pub const PCM_QUEUE: usize = 2 * 2 * RATE as usize;
const IDLE_SLEEP_MS: u32 = 5;

/// State shared between the player, the decoder thread and the audio thread.
#[derive(Default)]
pub struct AudioClock {
    /// 48 kHz frames of real audio handed to the speaker since the source
    /// started (silence while starved does not count).
    pub played: AtomicU32,
    /// Times the PCM queue ran dry while more audio was expected.
    pub underruns: AtomicU32,
    pub decode_errors: AtomicU32,
    /// Microseconds spent decoding, and packets decoded.
    pub decode_us: AtomicU32,
    pub decoded: AtomicU32,
    /// Set by the player when every packet has been queued.
    pub input_done: AtomicBool,
    /// Set by the decoder thread once it decoded the last packet.
    pub decoded_all: AtomicBool,
    /// Set by the source once it has played the last sample.
    pub finished: AtomicBool,
    /// Asks the decoder thread to exit.
    pub stop: AtomicBool,
    /// The decoder could not be created.
    pub failed: AtomicBool,
    /// Sum of the absolute sample values decoded, a cheap fingerprint that
    /// shows audio is really decoded (no speaker is fitted).
    pub checksum: AtomicU32,
}

/// An Opus decoder producing interleaved stereo 16-bit PCM.
pub struct PacketDecoder {
    decoder: Box<OpusDecoder>,
    channels: usize,
    scratch: Vec<f32>,
}

impl PacketDecoder {
    pub fn new(channels: u8) -> Result<Self, &'static str> {
        let channels = if channels == 1 { 1 } else { 2 };
        // The no_std decoder is ~175 KB: build it in place on the heap, as
        // `OpusDecoder::new` would pass it through the thread's stack.
        let mut slot = Box::<OpusDecoder>::new_uninit();
        OpusDecoder::init(&mut slot, RATE as i32, channels)?;
        // SAFETY: `init` returned Ok, so it initialised every field.
        let decoder = unsafe { slot.assume_init() };
        Ok(Self { decoder, channels, scratch: vec![0.0; MAX_FRAME * channels] })
    }

    /// Append the packet's samples to `out`; returns frames decoded.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<usize, &'static str> {
        let n = self.decoder.decode(packet, MAX_FRAME, &mut self.scratch)?.min(MAX_FRAME);
        out.reserve(2 * n);
        for i in 0..n {
            let (l, r) = if self.channels == 2 { (self.scratch[2 * i], self.scratch[2 * i + 1]) } else { (self.scratch[i], self.scratch[i]) };
            out.push((l * 32767.0).clamp(-32768.0, 32767.0) as i16);
            out.push((r * 32767.0).clamp(-32768.0, 32767.0) as i16);
        }
        Ok(n)
    }
}

/// Decodes packets and drops what must not be heard: the OpusHead
/// pre-skip at the start of the stream, and everything before `start_ms`
/// after a seek (still decoded, so the decoder state settles).
pub struct Trimmer {
    decoder: PacketDecoder,
    skip: usize,
    start_ms: i64,
}

impl Trimmer {
    /// `pre_skip` applies only when playing from the start of the stream.
    pub fn new(channels: u8, pre_skip: u16, start_ms: i64) -> Result<Self, &'static str> {
        Ok(Self { decoder: PacketDecoder::new(channels)?, skip: if start_ms <= 0 { pre_skip as usize * 2 } else { 0 }, start_ms })
    }

    /// The audible PCM of one packet (may be empty).
    pub fn decode(&mut self, pkt: &Packet) -> Result<Vec<i16>, &'static str> {
        let mut pcm = Vec::new();
        let n = self.decoder.decode(&pkt.data, &mut pcm)?;
        if pkt.pts_ms + (n as i64 * 1000 / RATE as i64) <= self.start_ms {
            return Ok(Vec::new());
        }
        let drop = self.skip.min(pcm.len());
        self.skip -= drop;
        pcm.drain(..drop);
        Ok(pcm)
    }
}

/// Decode a packet list on the calling thread (tests, host tools).
pub fn decode_packets(packets: &[Packet], channels: u8, pre_skip: u16) -> Result<Vec<i16>, &'static str> {
    let mut t = Trimmer::new(channels, pre_skip, 0)?;
    let mut out = Vec::new();
    for p in packets {
        out.extend(t.decode(p)?);
    }
    Ok(out)
}

/// The decoder worker: packets in, PCM chunks out. The decoder is built
/// inside the thread, whose stack is large enough for opus-rs.
pub struct DecoderThread {
    pub packets: Consumer<Packet>,
    pub pcm: Producer<i16>,
    pub channels: u8,
    pub pre_skip: u16,
    pub start_ms: i64,
    pub clock: Arc<AudioClock>,
}

impl DecoderThread {
    pub fn run(mut self, w: &dyn Worker) {
        let c = &*self.clock;
        let Ok(mut trimmer) = Trimmer::new(self.channels, self.pre_skip, self.start_ms) else {
            c.decode_errors.fetch_add(1, Ordering::Relaxed);
            c.failed.store(true, Ordering::Release);
            return;
        };
        // Decoded samples not yet in the ring, and how many were moved.
        let (mut pending, mut done) = (Vec::new(), 0);
        while !c.stop.load(Ordering::Acquire) {
            if done < pending.len() {
                done += self.pcm.push_slice(&pending[done..]);
                if done < pending.len() {
                    w.sleep_ms(IDLE_SLEEP_MS);
                }
                continue;
            }
            let Some(pkt) = self.packets.pop() else {
                if c.input_done.load(Ordering::Acquire) && self.packets.is_empty() {
                    c.decoded_all.store(true, Ordering::Release);
                }
                w.sleep_ms(IDLE_SLEEP_MS);
                continue;
            };
            let t = w.now_us();
            match trimmer.decode(&pkt) {
                Ok(pcm) if pcm.is_empty() => {}
                Ok(pcm) => {
                    let sum = pcm.iter().fold(0u32, |a, v| a.wrapping_add(v.unsigned_abs() as u32));
                    c.checksum.fetch_add(sum, Ordering::Relaxed);
                    pending = pcm;
                    done = 0;
                }
                Err(_) => {
                    c.decode_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            c.decode_us.fetch_add((w.now_us() - t) as u32, Ordering::Relaxed);
            c.decoded.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The [`AudioSource`] given to the OS: copies decoded PCM, keeps the clock.
pub struct PcmSource {
    pcm: Consumer<i16>,
    clock: Arc<AudioClock>,
    starved: bool,
}

impl PcmSource {
    pub fn new(pcm: Consumer<i16>, clock: Arc<AudioClock>) -> Self {
        Self { pcm, clock, starved: false }
    }
}

impl AudioSource for PcmSource {
    fn sample_rate(&self) -> u32 {
        RATE
    }

    fn fill(&mut self, out: &mut [i16]) -> usize {
        let want = out.len() & !1;
        // Whole stereo frames only: the decoder always pushes pairs, but a
        // pop may race with a push that is half done.
        let avail = self.pcm.len() & !1;
        let n = self.pcm.pop_slice(&mut out[..want.min(avail)]);
        self.clock.played.fetch_add((n / 2) as u32, Ordering::Relaxed);
        if n == want {
            self.starved = false;
            return n;
        }
        if self.clock.decoded_all.load(Ordering::Acquire) && self.pcm.is_empty() {
            self.clock.finished.store(true, Ordering::Release);
            return n;
        }
        // Starved: play silence without advancing the clock, so video
        // waits for the network too.
        if !self.starved {
            self.clock.underruns.fetch_add(1, Ordering::Relaxed);
            self.starved = true;
        }
        out[n..want].fill(0);
        want
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::video::tests::TestWorker;
    use crate::webm;

    /// `tests/fixtures/tone.webm`: 1 s of 440 Hz left / 660 Hz right,
    /// Opus 64 kbps in WebM (see make-fixtures.sh).
    pub const TONE: &[u8] = include_bytes!("../tests/fixtures/tone.webm");

    fn tone_packets() -> (webm::AudioInit, Vec<Packet>) {
        let (header_end, (cs, ce)) = webm::tests::layout(TONE);
        let init = webm::parse_init(&TONE[..header_end]).unwrap();
        let segs = webm::parse_cues(&TONE[cs..ce], &init, TONE.len() as u64).unwrap();
        let mut packets = Vec::new();
        for s in &segs {
            let chunk = Arc::new(TONE[s.offset as usize..(s.offset + s.size) as usize].to_vec());
            packets.extend(webm::parse_clusters(&chunk, &init).unwrap());
        }
        (init, packets)
    }

    /// Signal-to-noise ratio of one channel against the source sine (ffmpeg
    /// generates it at 1/8 scale, halved again by the fixture script),
    /// in dB, over 40 000 samples after the first 20 ms (codec start-up).
    fn snr(pcm: &[i16], ch: usize, freq: f64) -> f64 {
        let (mut sig, mut noise) = (0.0, 0.0);
        for i in 960..960 + 40_000 {
            let want = 32767.0 / 16.0 * (2.0 * core::f64::consts::PI * freq * i as f64 / RATE as f64).sin();
            let got = pcm[2 * i + ch] as f64;
            sig += want * want;
            noise += (got - want) * (got - want);
        }
        10.0 * (sig / noise).log10()
    }

    #[test]
    fn decodes_the_fixture_to_the_original_tones() {
        let (init, packets) = tone_packets();
        let pcm = decode_packets(&packets, init.channels(), init.pre_skip()).unwrap();
        // 1 s of stereo, give or take the last packet's padding.
        assert!((pcm.len() as i64 - 96_000).abs() <= 2 * 960, "{}", pcm.len());
        let (l, r) = (snr(&pcm, 0, 440.0), snr(&pcm, 1, 660.0));
        assert!(l > 25.0 && r > 25.0, "SNR left {l:.1} dB, right {r:.1} dB");
    }

    #[test]
    fn skips_audio_before_the_seek_point() {
        let (init, packets) = tone_packets();
        let mut t = Trimmer::new(init.channels(), init.pre_skip(), 500).unwrap();
        let total: usize = packets.iter().map(|p| t.decode(p).unwrap().len()).sum();
        // Roughly the second half second remains.
        let ms = total / 2 * 1000 / RATE as usize;
        assert!((480..=540).contains(&ms), "{ms} ms");
    }

    fn fill_all(src: &mut PcmSource, clock: &AudioClock) -> usize {
        let mut buf = vec![0i16; 1024];
        let mut total = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !clock.finished.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            let before = clock.played.load(Ordering::Relaxed);
            src.fill(&mut buf);
            total += (clock.played.load(Ordering::Relaxed) - before) as usize;
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        total
    }

    #[test]
    fn pipeline_keeps_the_clock_and_reports_underruns() {
        let (init, packets) = tone_packets();
        let (mut tx, rx) = crate::spsc::channel(64);
        let (pcm_tx, pcm_rx) = crate::spsc::channel(PCM_QUEUE);
        let clock = Arc::new(AudioClock::default());
        let mut src = PcmSource::new(pcm_rx, clock.clone());
        let mut buf = vec![0i16; 1024];
        // Nothing decoded yet: silence, the clock stands still, one underrun.
        assert_eq!(src.fill(&mut buf), 1024);
        assert_eq!(src.fill(&mut buf), 1024);
        assert_eq!(clock.played.load(Ordering::Relaxed), 0);
        assert_eq!(clock.underruns.load(Ordering::Relaxed), 1);
        let thread = DecoderThread { packets: rx, pcm: pcm_tx, channels: init.channels(), pre_skip: init.pre_skip(), start_ms: 0, clock: clock.clone() };
        let h = std::thread::spawn(move || thread.run(&TestWorker(std::time::Instant::now())));
        for p in packets.iter().take(10) {
            tx.push(p.clone()).unwrap();
        }
        clock.input_done.store(true, Ordering::Release);
        let played = fill_all(&mut src, &clock);
        clock.stop.store(true, Ordering::Release);
        h.join().unwrap();
        // 10 packets of 20 ms minus the pre-skip.
        assert_eq!(played, 10 * 960 - init.pre_skip() as usize);
        assert!(clock.finished.load(Ordering::Acquire));
        assert!(clock.checksum.load(Ordering::Relaxed) > 0);
        assert_eq!(clock.decoded.load(Ordering::Relaxed), 10);
    }
}
