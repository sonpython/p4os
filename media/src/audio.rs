//! Streaming audio sources: MP3 (nanomp3) and PCM WAV, read incrementally
//! from an open file so large tracks never need to fit in memory.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use p4os_sdk::{AudioSource, FileReader};

/// Pick a decoder from the file name extension.
pub fn open(name: &str, reader: Box<dyn FileReader>) -> Option<Box<dyn AudioSource>> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".mp3") {
        Some(Box::new(Mp3Source::new(reader)))
    } else if lower.ends_with(".wav") {
        WavSource::new(reader).map(|w| Box::new(w) as Box<dyn AudioSource>)
    } else {
        None
    }
}

pub fn is_audio_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".mp3") || lower.ends_with(".wav")
}

const READ_CHUNK: usize = 16 * 1024;

pub struct Mp3Source {
    reader: Box<dyn FileReader>,
    // The decoder state is several KiB; keep it off the caller's stack.
    decoder: Box<nanomp3::Decoder>,
    input: Vec<u8>,
    pos: usize,
    eof: bool,
    pcm: Vec<i16>,
    pcm_pos: usize,
    sample_rate: u32,
}

impl Mp3Source {
    pub fn new(reader: Box<dyn FileReader>) -> Self {
        let mut s = Self {
            reader,
            decoder: Box::new(nanomp3::Decoder::new()),
            input: Vec::new(),
            pos: 0,
            eof: false,
            pcm: Vec::new(),
            pcm_pos: 0,
            sample_rate: 44_100,
        };
        // Decode the first frame up front so sample_rate() is accurate.
        s.decode_frame();
        s
    }

    fn refill(&mut self) {
        if self.eof {
            return;
        }
        self.input.drain(..self.pos);
        self.pos = 0;
        let start = self.input.len();
        self.input.resize(start + READ_CHUNK, 0);
        match self.reader.read(&mut self.input[start..]) {
            Ok(0) | Err(_) => {
                self.input.truncate(start);
                self.eof = true;
            }
            Ok(n) => self.input.truncate(start + n),
        }
    }

    /// Decode the next frame into `pcm`; false at end of stream.
    fn decode_frame(&mut self) -> bool {
        let mut out = vec![0f32; nanomp3::MAX_SAMPLES_PER_FRAME];
        loop {
            // nanomp3 wants several frames of lookahead to avoid artifacts.
            if self.input.len() - self.pos < READ_CHUNK / 2 && !self.eof {
                self.refill();
            }
            if self.pos >= self.input.len() {
                return false;
            }
            let (consumed, info) = self.decoder.decode(&self.input[self.pos..], &mut out);
            self.pos += consumed;
            if let Some(info) = info {
                self.sample_rate = info.sample_rate;
                let ch = info.channels.num() as usize;
                self.pcm.clear();
                self.pcm_pos = 0;
                for f in out[..info.samples_produced * ch].chunks(ch) {
                    let l = to_i16(f[0]);
                    let r = if ch == 2 { to_i16(f[1]) } else { l };
                    self.pcm.push(l);
                    self.pcm.push(r);
                }
                return true;
            }
            if consumed == 0 {
                if self.eof {
                    return false;
                }
                // Need more data to find the next frame.
                self.refill();
            }
        }
    }
}

fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

impl AudioSource for Mp3Source {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn fill(&mut self, out: &mut [i16]) -> usize {
        let mut n = 0;
        while n < out.len() {
            if self.pcm_pos >= self.pcm.len() && !self.decode_frame() {
                break;
            }
            let take = (out.len() - n).min(self.pcm.len() - self.pcm_pos);
            out[n..n + take].copy_from_slice(&self.pcm[self.pcm_pos..self.pcm_pos + take]);
            n += take;
            self.pcm_pos += take;
        }
        n
    }
}

/// 16-bit PCM WAV, mono or stereo.
pub struct WavSource {
    reader: Box<dyn FileReader>,
    channels: usize,
    sample_rate: u32,
    remaining: u64,
    buf: Vec<u8>,
}

impl WavSource {
    pub fn new(mut reader: Box<dyn FileReader>) -> Option<Self> {
        let mut header = vec![0u8; 12];
        read_exact(&mut *reader, &mut header)?;
        if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
            return None;
        }
        let (mut channels, mut rate, mut bits) = (0usize, 0u32, 0u16);
        loop {
            let mut chunk = [0u8; 8];
            read_exact(&mut *reader, &mut chunk)?;
            let len = u32::from_le_bytes(chunk[4..8].try_into().ok()?) as usize;
            match &chunk[0..4] {
                b"fmt " => {
                    let mut fmt = vec![0u8; len];
                    read_exact(&mut *reader, &mut fmt)?;
                    let format = u16::from_le_bytes([fmt[0], fmt[1]]);
                    channels = u16::from_le_bytes([fmt[2], fmt[3]]) as usize;
                    rate = u32::from_le_bytes(fmt[4..8].try_into().ok()?);
                    bits = u16::from_le_bytes([fmt[14], fmt[15]]);
                    if format != 1 {
                        return None;
                    }
                }
                b"data" => {
                    if bits != 16 || !(1..=2).contains(&channels) {
                        return None;
                    }
                    return Some(Self { reader, channels, sample_rate: rate, remaining: len as u64, buf: Vec::new() });
                }
                _ => {
                    let mut skip = vec![0u8; len + (len & 1)];
                    read_exact(&mut *reader, &mut skip)?;
                }
            }
        }
    }
}

fn read_exact(r: &mut dyn FileReader, buf: &mut [u8]) -> Option<()> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) | Err(_) => return None,
            Ok(k) => n += k,
        }
    }
    Some(())
}

impl AudioSource for WavSource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn fill(&mut self, out: &mut [i16]) -> usize {
        let frames = out.len() / 2;
        let want = ((frames * self.channels * 2) as u64).min(self.remaining) as usize;
        self.buf.resize(want, 0);
        let got = match self.reader.read(&mut self.buf) {
            Ok(n) => n & !(self.channels * 2 - 1),
            Err(_) => 0,
        };
        self.remaining -= got as u64;
        let samples = self.buf[..got].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]));
        let mut n = 0;
        if self.channels == 2 {
            for s in samples {
                out[n] = s;
                n += 1;
            }
        } else {
            for s in samples {
                out[n] = s;
                out[n + 1] = s;
                n += 2;
            }
        }
        n
    }
}
