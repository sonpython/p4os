//! Decoder benchmarks for the board (`yt bench`): decode an Annex-B H.264
//! stream or an Opus WebM file and report time per frame, so the default
//! video quality can be chosen from measurements.

use crate::yuv::YuvToRgb;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use p4os_sdk::Color;

/// Name of the H.264 decoder, for reports.
pub const H264_DECODER: &str = "rusty_h264-decoder 0.16.1";

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub frames: u32,
    pub width: usize,
    pub height: usize,
    /// Decode time over all frames, microseconds.
    pub decode_us: u64,
    /// Slowest single frame.
    pub max_frame_us: u64,
    /// YUV to RGB565 conversion of every frame at native size.
    pub convert_us: u64,
    /// CRC-32 over the Y, U and V planes of every frame in decode order,
    /// to compare against a host decode of the same bytes.
    pub crc: u32,
}

impl Report {
    pub fn summary(&self, name: &str) -> String {
        let mbs = (self.width.div_ceil(16) * self.height.div_ceil(16)) as u64;
        let per_frame = self.decode_us / self.frames.max(1) as u64;
        let fps = if self.decode_us > 0 { self.frames as u64 * 1_000_000 / self.decode_us } else { 0 };
        // Cycles per macroblock at 360 MHz.
        let cyc_mb = per_frame * 360 / mbs.max(1);
        format!(
            "{name}: {}x{} {} frames, decode {} us/frame (max {}), {} fps on one core, ~{} cycles/MB; yuv->rgb565 {} us/frame; crc {:08x}",
            self.width,
            self.height,
            self.frames,
            per_frame,
            self.max_frame_us,
            fps,
            cyc_mb,
            self.convert_us / self.frames.max(1) as u64,
            self.crc
        )
    }
}

pub fn crc32(mut crc: u32, data: &[u8]) -> u32 {
    crc = !crc;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

struct Acc<'a> {
    report: Report,
    rgb: Vec<Color>,
    conv: &'a YuvToRgb,
    now_us: &'a dyn Fn() -> u64,
}

impl Acc<'_> {
    fn frame(&mut self, w: usize, h: usize, y: &[u8], u: &[u8], v: &[u8], decode_us: u64) {
        let r = &mut self.report;
        r.frames += 1;
        r.width = w;
        r.height = h;
        r.decode_us += decode_us;
        r.max_frame_us = r.max_frame_us.max(decode_us);
        r.crc = crc32(crc32(crc32(r.crc, y), u), v);
        self.rgb.resize(w * h, Color(0));
        let t = (self.now_us)();
        self.conv.convert(y, u, v, w, h, &mut self.rgb);
        self.report.convert_us += (self.now_us)() - t;
    }
}

/// Decode up to `max_frames` pictures of an Annex-B stream.
pub fn h264(data: &[u8], max_frames: u32, now_us: &dyn Fn() -> u64) -> Result<Report, String> {
    let conv = YuvToRgb::new();
    let mut acc = Acc { report: Report::default(), rgb: vec![], conv: &conv, now_us };
    let mut dec = rusty_h264_decoder::Decoder::new();
    for au in rusty_h264_decoder::split_access_units(data) {
        if acc.report.frames >= max_frames {
            break;
        }
        let t = now_us();
        let frame = dec.decode(au).map_err(|e| format!("frame {}: {e}", acc.report.frames))?;
        let dt = now_us() - t;
        if let Some(f) = frame {
            acc.frame(f.width, f.height, &f.y, &f.u, &f.v, dt);
        }
    }
    if acc.report.frames == 0 {
        return Err("no pictures decoded".into());
    }
    Ok(acc.report)
}

#[derive(Clone, Debug, Default)]
pub struct OpusReport {
    pub packets: u32,
    /// Audio decoded, ms.
    pub media_ms: u64,
    pub decode_us: u64,
    pub max_packet_us: u64,
    /// CRC-32 of the interleaved s16le PCM (after the pre-skip).
    pub crc: u32,
    /// Against a reference decode (ffmpeg, s16le stereo 48 kHz), skipping
    /// the first 0.5 s where decoders settle differently: SNR in dB and
    /// the largest sample difference.
    pub reference: Option<(f64, u32)>,
}

impl OpusReport {
    pub fn summary(&self) -> String {
        let load = self.decode_us as f64 / (self.media_ms.max(1) * 10) as f64;
        format!(
            "opus-rs 0.1.40: {} packets ({} ms of audio) in {} us: {} us/packet (max {}), {:.1}% of one core; pcm crc {:08x}",
            self.packets,
            self.media_ms,
            self.decode_us,
            self.decode_us / self.packets.max(1) as u64,
            self.max_packet_us,
            load,
            self.crc
        ) + &match self.reference {
            Some((snr, max)) => format!("; vs reference: SNR {snr:.1} dB, max diff {max}"),
            None => String::new(),
        }
    }
}

/// SNR (dB) and largest difference of `got` against `want` (s16le bytes),
/// over the samples both have after `skip` samples.
pub fn compare_pcm(got: &[i16], want: &[u8], skip: usize) -> (f64, u32) {
    let (mut sig, mut noise, mut max) = (0f64, 0f64, 0u32);
    for (g, w) in got.iter().zip(want.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]))).skip(skip) {
        let (g, w) = (*g as f64, w as f64);
        sig += w * w;
        noise += (g - w) * (g - w);
        max = max.max((g - w).abs() as u32);
    }
    let snr = if noise == 0.0 { f64::INFINITY } else { 10.0 * libm::log10(sig / noise) };
    (snr, max)
}

/// Decode every Opus packet of a WebM file (whole or truncated).
pub fn opus(webm_file: &[u8], reference: Option<&[u8]>, now_us: &dyn Fn() -> u64) -> Result<OpusReport, String> {
    let (init, packets) = crate::webm::demux_file(webm_file)?;
    if packets.is_empty() {
        return Err("no Opus packets".into());
    }
    let mut r = OpusReport::default();
    let mut dec = crate::audio::PacketDecoder::new(init.channels()).map_err(String::from)?;
    let mut crc = 0;
    let mut skip = init.pre_skip() as usize * 2;
    let mut pcm = Vec::new();
    let mut all = Vec::new();
    for p in &packets {
        pcm.clear();
        let t = now_us();
        let frames = dec.decode(&p.data, &mut pcm).map_err(String::from)?;
        let dt = now_us() - t;
        r.decode_us += dt;
        r.max_packet_us = r.max_packet_us.max(dt);
        r.packets += 1;
        r.media_ms += frames as u64 * 1000 / crate::audio::RATE as u64;
        let drop = skip.min(pcm.len());
        skip -= drop;
        let bytes: Vec<u8> = pcm[drop..].iter().flat_map(|s| s.to_le_bytes()).collect();
        crc = crc32(crc, &bytes);
        if reference.is_some_and(|want| all.len() * 2 < want.len()) {
            all.extend_from_slice(&pcm[drop..]);
        }
    }
    r.crc = crc;
    r.reference = reference.map(|want| compare_pcm(&all, want, crate::audio::RATE as usize));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_pcm() {
        let want: Vec<u8> = [1000i16, -1000, 500, 0].iter().flat_map(|s| s.to_le_bytes()).collect();
        let (snr, max) = compare_pcm(&[1000, -1000, 500, 0], &want, 0);
        assert!(snr > 100.0 && max == 0);
        let (snr, max) = compare_pcm(&[1010, -1000, 500, 0, 7], &want, 0);
        assert!((snr - 43.5).abs() < 0.2 && max == 10, "{snr}");
    }

    #[test]
    fn crc_matches_reference() {
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(crc32(0, b"1234"), b"56789"), 0xCBF4_3926);
    }
}
