//! Opus decoding inside the audio thread: an [`AudioSource`] that pulls
//! demuxed packets from a queue filled by the player and keeps the media
//! clock (samples actually played) that video is synchronised to.

use crate::spsc::Consumer;
use crate::webm::Packet;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use opus_rs::OpusDecoder;
use p4os_sdk::AudioSource;

pub const RATE: u32 = 48_000;
/// Largest Opus packet: 120 ms at 48 kHz.
const MAX_FRAME: usize = 5760;

/// State shared between the audio thread and the player.
#[derive(Default)]
pub struct AudioClock {
    /// 48 kHz frames of real audio handed to the speaker since the source
    /// started (silence while starved does not count).
    pub played: AtomicU32,
    /// Times the queue ran dry while more audio was expected.
    pub underruns: AtomicU32,
    pub decode_errors: AtomicU32,
    /// Set by the player when every packet has been queued.
    pub input_done: AtomicBool,
    /// Set by the source once it has played the last sample.
    pub finished: AtomicBool,
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

pub struct OpusSource {
    packets: Consumer<Packet>,
    decoder: PacketDecoder,
    pcm: Vec<i16>,
    pos: usize,
    /// Interleaved stereo samples still to discard (OpusHead pre-skip).
    skip: usize,
    /// Packets before this time are decoded (to settle the decoder after a
    /// seek) but not played.
    start_ms: i64,
    clock: Arc<AudioClock>,
    starved: bool,
}

impl OpusSource {
    /// `pre_skip` applies only when playing from the start of the stream.
    pub fn new(packets: Consumer<Packet>, channels: u8, pre_skip: u16, start_ms: i64, clock: Arc<AudioClock>) -> Result<Self, &'static str> {
        Ok(Self {
            packets,
            decoder: PacketDecoder::new(channels)?,
            pcm: Vec::with_capacity(MAX_FRAME * 2),
            pos: 0,
            skip: if start_ms <= 0 { pre_skip as usize * 2 } else { 0 },
            start_ms,
            clock,
            starved: false,
        })
    }

    /// Decode the next packet into `pcm`. False when no packet is queued.
    fn decode_next(&mut self) -> bool {
        let Some(pkt) = self.packets.pop() else { return false };
        self.pcm.clear();
        self.pos = 0;
        let n = match self.decoder.decode(&pkt.data, &mut self.pcm) {
            Ok(n) => n,
            Err(_) => {
                self.clock.decode_errors.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        };
        if pkt.pts_ms + (n as i64 * 1000 / RATE as i64) <= self.start_ms {
            self.pcm.clear();
            return true;
        }
        let sum = self.pcm.iter().fold(0u32, |a, v| a.wrapping_add(v.unsigned_abs() as u32));
        self.clock.checksum.fetch_add(sum, Ordering::Relaxed);
        let drop = self.skip.min(self.pcm.len());
        self.pos = drop;
        self.skip -= drop;
        true
    }
}

/// Packets of the fixture or a stream, for host tools and tests: decode
/// everything to interleaved stereo PCM, honouring the pre-skip.
pub fn decode_packets(packets: &[Packet], channels: u8, pre_skip: u16) -> Result<Vec<i16>, &'static str> {
    let (mut tx, rx) = crate::spsc::channel(packets.len().max(1));
    for p in packets {
        let _ = tx.push(p.clone());
    }
    let clock = Arc::new(AudioClock::default());
    clock.input_done.store(true, Ordering::Release);
    let mut src = OpusSource::new(rx, channels, pre_skip, 0, clock)?;
    let mut out = Vec::new();
    let mut buf = vec![0i16; 4096];
    loop {
        let n = src.fill(&mut buf);
        out.extend_from_slice(&buf[..n]);
        if n < buf.len() {
            return Ok(out);
        }
    }
}

impl AudioSource for OpusSource {
    fn sample_rate(&self) -> u32 {
        RATE
    }

    fn fill(&mut self, out: &mut [i16]) -> usize {
        let mut n = 0;
        while n + 1 < out.len() {
            if self.pos >= self.pcm.len() {
                if self.decode_next() {
                    continue;
                }
                if self.clock.input_done.load(Ordering::Acquire) {
                    self.clock.finished.store(true, Ordering::Release);
                    break;
                }
                // Starved: play silence without advancing the clock, so
                // video waits for the network too.
                if !self.starved {
                    self.clock.underruns.fetch_add(1, Ordering::Relaxed);
                    self.starved = true;
                }
                out[n..].fill(0);
                return out.len() & !1;
            }
            self.starved = false;
            let take = (self.pcm.len() - self.pos).min((out.len() - n) & !1);
            out[n..n + take].copy_from_slice(&self.pcm[self.pos..self.pos + take]);
            self.pos += take;
            n += take;
            self.clock.played.fetch_add((take / 2) as u32, Ordering::Relaxed);
        }
        n
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::webm;

    /// `tests/fixtures/tone.webm`: 1 s of 440 Hz left / 660 Hz right at
    /// half scale, Opus 64 kbps in WebM (see make-fixtures.sh).
    pub const TONE: &[u8] = include_bytes!("../tests/fixtures/tone.webm");

    fn tone_packets() -> (webm::AudioInit, Vec<Packet>) {
        let (header_end, (cs, ce)) = webm::tests::layout(TONE);
        let init = webm::parse_init(&TONE[..header_end]).unwrap();
        let segs = webm::parse_cues(&TONE[cs..ce], &init, TONE.len() as u64).unwrap();
        let mut packets = Vec::new();
        for s in &segs {
            packets.extend(webm::parse_clusters(&TONE[s.offset as usize..(s.offset + s.size) as usize], &init).unwrap());
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
    fn keeps_the_clock_and_reports_underruns() {
        let (init, packets) = tone_packets();
        let (mut tx, rx) = crate::spsc::channel(64);
        let clock = Arc::new(AudioClock::default());
        let mut src = OpusSource::new(rx, init.channels(), init.pre_skip(), 0, clock.clone()).unwrap();
        let mut buf = vec![0i16; 1024];
        // Nothing queued yet: silence, the clock stands still, one underrun.
        assert_eq!(src.fill(&mut buf), 1024);
        assert_eq!(src.fill(&mut buf), 1024);
        assert_eq!(clock.played.load(Ordering::Relaxed), 0);
        assert_eq!(clock.underruns.load(Ordering::Relaxed), 1);
        for p in packets.iter().take(10) {
            tx.push(p.clone()).unwrap();
        }
        clock.input_done.store(true, Ordering::Release);
        let mut total = 0;
        loop {
            let n = src.fill(&mut buf);
            total += n;
            if n < buf.len() {
                break;
            }
        }
        // 10 packets of 20 ms minus the pre-skip.
        assert_eq!(total / 2, 10 * 960 - init.pre_skip() as usize);
        assert_eq!(clock.played.load(Ordering::Relaxed) as usize, total / 2);
        assert!(clock.finished.load(Ordering::Acquire));
        assert!(clock.checksum.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn skips_audio_before_the_seek_point() {
        let (init, packets) = tone_packets();
        let (mut tx, rx) = crate::spsc::channel(64);
        let clock = Arc::new(AudioClock::default());
        let mut src = OpusSource::new(rx, init.channels(), init.pre_skip(), 500, clock.clone()).unwrap();
        for p in &packets {
            tx.push(p.clone()).unwrap();
        }
        clock.input_done.store(true, Ordering::Release);
        let mut buf = vec![0i16; 1 << 16];
        let mut total = 0;
        loop {
            let n = src.fill(&mut buf);
            total += n;
            if n < buf.len() {
                break;
            }
        }
        // Roughly the second half second remains.
        let ms = total / 2 * 1000 / RATE as usize;
        assert!((480..=540).contains(&ms), "{ms} ms");
    }
}
