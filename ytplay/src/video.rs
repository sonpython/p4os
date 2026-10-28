//! The video decoder thread: H.264 access units in, RGB565 frames out in
//! presentation order, with late frames dropped against the media clock.
//!
//! It runs on its own worker thread (core 1 on the board) and talks to the
//! player only through lock-free queues and atomics.

use crate::mp4::Sample;
use crate::spsc::{Consumer, Producer};
use crate::yuv::YuvToRgb;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use p4os_sdk::{Color, Worker};
use rusty_h264_decoder::Decoder;

/// Decoded pictures held back to restore presentation order. YouTube's
/// streams use up to two consecutive B-frames.
const REORDER: usize = 3;
/// RGB frames in flight between the decoder and the screen.
pub const POOL: usize = REORDER + 3;
/// A picture this late is not converted or shown.
const LATE_MS: i32 = 60;
/// Behind by this much, skip ahead to the next keyframe.
const SKIP_AHEAD_MS: i32 = 1500;
const IDLE_SLEEP_MS: u32 = 3;
/// Give up after this many consecutive undecodable access units.
const MAX_ERRORS: u32 = 25;

pub struct VideoIn {
    pub generation: u32,
    pub sample: Sample,
}

pub struct VideoFrame {
    pub generation: u32,
    pub pts_ms: i64,
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Color>,
}

/// Counters and controls shared with the player.
#[derive(Default)]
pub struct VideoShared {
    pub stop: AtomicBool,
    /// Bumped on seek: older samples and frames are discarded.
    pub generation: AtomicU32,
    /// The generation whose last sample has been queued.
    pub input_done: AtomicU32,
    /// Media clock in ms, updated by the player every tick.
    pub clock_ms: AtomicI32,
    pub decoded: AtomicU32,
    /// Pictures decoded but not shown because they were already late.
    pub dropped_late: AtomicU32,
    /// Access units skipped to catch up with a keyframe.
    pub skipped: AtomicU32,
    pub decode_us: AtomicU32,
    pub decode_max_us: AtomicU32,
    pub convert_us: AtomicU32,
    pub errors: AtomicU32,
    /// Set when the decoder gave up; the player shows the error.
    pub failed: AtomicBool,
    pub running: AtomicBool,
}

pub struct DecoderThread {
    pub input: Consumer<VideoIn>,
    pub output: Producer<VideoFrame>,
    /// Frames handed back by the player for reuse.
    pub free: Consumer<VideoFrame>,
    pub shared: Arc<VideoShared>,
}

struct Pending {
    pts_ms: i64,
    frame: VideoFrame,
}

impl DecoderThread {
    pub fn run(mut self, w: &dyn Worker) {
        let sh = self.shared.clone();
        sh.running.store(true, Ordering::Release);
        let conv = YuvToRgb::new();
        let mut decoder = Decoder::new();
        let mut generation = sh.generation.load(Ordering::Acquire);
        let mut pending: Vec<Pending> = Vec::with_capacity(REORDER + 1);
        let mut allocated = 0usize;
        let mut skipping = false;
        let mut consecutive_errors = 0;
        while !sh.stop.load(Ordering::Acquire) {
            let g = sh.generation.load(Ordering::Acquire);
            if g != generation {
                generation = g;
                decoder = Decoder::new();
                for p in pending.drain(..) {
                    allocated -= 1;
                    drop(p);
                }
                skipping = false;
                consecutive_errors = 0;
            }
            let drained = self.input.is_empty() && sh.input_done.load(Ordering::Acquire) == generation;
            if pending.len() > REORDER || (drained && !pending.is_empty()) {
                // Lowest pts first; the output never overflows since at most
                // POOL frames exist.
                let i = (0..pending.len()).min_by_key(|&i| pending[i].pts_ms).unwrap_or(0);
                let p = pending.swap_remove(i);
                if self.output.push(p.frame).is_err() {
                    allocated -= 1;
                }
                continue;
            }
            let Some(item) = self.input.pop() else {
                w.sleep_ms(IDLE_SLEEP_MS);
                continue;
            };
            if item.generation != generation {
                continue;
            }
            let clock = sh.clock_ms.load(Ordering::Relaxed);
            let lag = clock as i64 - item.sample.pts_ms;
            if !skipping && lag > SKIP_AHEAD_MS as i64 && !item.sample.is_sync {
                skipping = true;
            }
            if skipping {
                if item.sample.is_sync && lag < LATE_MS as i64 {
                    skipping = false;
                } else {
                    sh.skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            let t = w.now_us();
            if let Some(p) = &item.sample.params {
                // SPS/PPS only: never yields a picture.
                let _ = decoder.decode(p);
            }
            let picture = decoder.decode(&item.sample.data);
            let dt = (w.now_us() - t) as u32;
            sh.decode_us.fetch_add(dt, Ordering::Relaxed);
            sh.decode_max_us.fetch_max(dt, Ordering::Relaxed);
            let yuv = match picture {
                Ok(Some(y)) => {
                    consecutive_errors = 0;
                    y
                }
                Ok(None) => continue,
                Err(_) => {
                    sh.errors.fetch_add(1, Ordering::Relaxed);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_ERRORS {
                        sh.failed.store(true, Ordering::Release);
                        break;
                    }
                    continue;
                }
            };
            sh.decoded.fetch_add(1, Ordering::Relaxed);
            if (sh.clock_ms.load(Ordering::Relaxed) as i64 - item.sample.pts_ms) > LATE_MS as i64 {
                sh.dropped_late.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // A buffer to convert into: a returned one, a new one while the
            // pool is not full, else wait for the screen to release one.
            let frame = loop {
                if let Some(f) = self.free.pop() {
                    break Some(f);
                }
                if allocated < POOL {
                    allocated += 1;
                    break Some(VideoFrame { generation, pts_ms: 0, width: 0, height: 0, pixels: Vec::new() });
                }
                if sh.stop.load(Ordering::Acquire) || sh.generation.load(Ordering::Acquire) != generation {
                    break None;
                }
                w.sleep_ms(IDLE_SLEEP_MS);
            };
            let Some(mut f) = frame else { continue };
            let t = w.now_us();
            f.pixels.resize(yuv.width * yuv.height, Color(0));
            conv.convert(&yuv.y, &yuv.u, &yuv.v, yuv.width, yuv.height, &mut f.pixels);
            sh.convert_us.fetch_add((w.now_us() - t) as u32, Ordering::Relaxed);
            f.generation = generation;
            f.pts_ms = item.sample.pts_ms;
            (f.width, f.height) = (yuv.width, yuv.height);
            pending.push(Pending { pts_ms: item.sample.pts_ms, frame: f });
        }
        sh.running.store(false, Ordering::Release);
    }
}

/// A decoded 4:2:0 picture.
pub struct Picture {
    pub pts_ms: i64,
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// Decode a whole sample list on the calling thread (tests, host tools):
/// pictures in presentation order.
pub fn decode_all(samples: &[Sample]) -> Result<Vec<Picture>, String> {
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    for s in samples {
        if let Some(p) = &s.params {
            dec.decode(p).map_err(|e| alloc::format!("{e}"))?;
        }
        if let Some(f) = dec.decode(&s.data).map_err(|e| alloc::format!("{e}"))? {
            out.push(Picture { pts_ms: s.pts_ms, width: f.width, height: f.height, y: f.y, u: f.u, v: f.v });
        }
    }
    out.sort_by_key(|p| p.pts_ms);
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bench::crc32;
    use crate::mp4;

    /// `tests/fixtures/pattern.mp4`: 2 s of ffmpeg's testsrc2 at 128x72,
    /// H.264 Main with CABAC, B-frames and weighted prediction in DASH
    /// fMP4, like YouTube's streams (see make-fixtures.sh).
    pub const PATTERN: &[u8] = include_bytes!("../tests/fixtures/pattern.mp4");
    /// CRC-32 of each frame (Y, U, V planes) as decoded by ffmpeg, in
    /// presentation order.
    const FFMPEG_CRCS: [u32; 50] = [
        0x00613435, 0xb37ef56c, 0xf5d69a74, 0x6083854c, 0x6357e6aa, 0x08be9227, 0xa9066df0, 0x4475c41c, 0xa4282a23, 0x11c3cee0,
        0xb0036deb, 0x2a347905, 0xf8c05a2c, 0xbc999022, 0xe0d346dc, 0x573ef2fd, 0x396f5a49, 0xf6c8d6e6, 0x8b324228, 0x95d0123b,
        0x9c2ae8fe, 0x70bb9a58, 0x87596f8a, 0x1bcd4e36, 0xf6066081, 0xaa90f10c, 0xf9ec996f, 0xb45f60e8, 0x6797f460, 0x3b6a746c,
        0xc22d4cdb, 0x324355c0, 0xcf03b3df, 0x8915fbbb, 0xc02543f1, 0xbc8d0a17, 0xf55b91a4, 0x02443cd2, 0x76bcd6db, 0x78950f36,
        0xedcf2eae, 0xbadab916, 0x42b29020, 0x52b1b70d, 0xe7ba730b, 0x80a3cee0, 0x4cb73fb3, 0x2e368f79, 0xf483fc16, 0x80a8fb52,
    ];

    pub fn pattern_samples() -> Vec<Sample> {
        let init = mp4::parse_init(&PATTERN[..776]).unwrap();
        let segs = mp4::parse_sidx(&PATTERN[776..840], 776).unwrap();
        assert_eq!(segs.len(), 2);
        let mut samples = Vec::new();
        for s in &segs {
            let bytes = &PATTERN[s.offset as usize..(s.offset + s.size) as usize];
            samples.extend(mp4::parse_fragments(bytes.to_vec(), &init).unwrap());
        }
        samples
    }

    #[test]
    fn decodes_the_fixture_bit_exact_with_ffmpeg() {
        let samples = pattern_samples();
        assert_eq!(samples.len(), 50);
        assert!(samples[0].is_sync && samples[25].is_sync && !samples[1].is_sync);
        let pics = decode_all(&samples).unwrap();
        assert_eq!(pics.len(), 50);
        assert_eq!((pics[0].width, pics[0].height), (128, 72));
        let crcs: Vec<u32> = pics.iter().map(|p| crc32(crc32(crc32(0, &p.y), &p.u), &p.v)).collect();
        assert_eq!(crcs, FFMPEG_CRCS);
        // Presentation times step by one frame (40 ms at 25 fps).
        assert!(pics.windows(2).all(|w| w[1].pts_ms - w[0].pts_ms == 40));
    }

    pub struct TestWorker(pub std::time::Instant);

    impl Worker for TestWorker {
        fn sleep_ms(&self, ms: u32) {
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
        }
        fn now_us(&self) -> u64 {
            self.0.elapsed().as_micros() as u64
        }
    }

    fn start(clock_ms: i32) -> (crate::spsc::Consumer<VideoFrame>, crate::spsc::Producer<VideoFrame>, Arc<VideoShared>, std::thread::JoinHandle<()>) {
        let (mut in_tx, in_rx) = crate::spsc::channel(64);
        let (out_tx, out_rx) = crate::spsc::channel(POOL);
        let (free_tx, free_rx) = crate::spsc::channel(POOL);
        let shared = Arc::new(VideoShared::default());
        shared.clock_ms.store(clock_ms, Ordering::Relaxed);
        for sample in pattern_samples() {
            in_tx.push(VideoIn { generation: 0, sample }).ok().unwrap();
        }
        let t = DecoderThread { input: in_rx, output: out_tx, free: free_rx, shared: shared.clone() };
        let handle = std::thread::spawn(move || t.run(&TestWorker(std::time::Instant::now())));
        // Everything is queued up front; input_done marks the end.
        shared.input_done.store(0, Ordering::Release);
        drop(in_tx);
        (out_rx, free_tx, shared, handle)
    }

    #[test]
    fn decoder_thread_emits_frames_in_order_and_recycles_buffers() {
        let (mut out_rx, mut free_tx, shared, handle) = start(0);
        let mut pts = Vec::new();
        while pts.len() < 50 {
            match out_rx.pop() {
                Some(f) => {
                    assert_eq!((f.width, f.height, f.pixels.len()), (128, 72, 128 * 72));
                    pts.push(f.pts_ms);
                    free_tx.push(f).ok().unwrap();
                }
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
        shared.stop.store(true, Ordering::Release);
        handle.join().unwrap();
        assert!(pts.windows(2).all(|w| w[1] > w[0]), "{pts:?}");
        assert_eq!(shared.decoded.load(Ordering::Relaxed), 50);
        assert_eq!(shared.dropped_late.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn late_pictures_are_decoded_but_not_shown() {
        // The clock is at 1.1 s: the first GOP (0.08-1.04 s) is late, so it
        // is decoded (later pictures reference it) but not converted.
        let (mut out_rx, _free_tx, shared, handle) = start(1100);
        let first = loop {
            if let Some(f) = out_rx.pop() {
                break f;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        shared.stop.store(true, Ordering::Release);
        handle.join().unwrap();
        assert!(first.pts_ms >= 1100 - LATE_MS as i64, "{}", first.pts_ms);
        assert!(shared.dropped_late.load(Ordering::Relaxed) >= 20);
    }
}
