//! Screenshot encoding: the framed serial protocol, a lossless RGB565
//! codec and a store-only PNG writer.
//!
//! Serial frame, all integers little endian:
//!
//! ```text
//! START (8) | header (16) | payload (payload_len) | CRC-32 (4) | END (8)
//! header: width u16, height u16, format u8, encoding u8, reserved u16,
//!         payload_len u32, pixels_len u32 (decoded bytes)
//! ```
//!
//! The CRC (zlib's CRC-32) covers the header and the payload. The markers
//! let the host skip log lines printed before or after the frame.
//!
//! The `Rle` encoding is a stream of tokens over RGB565 pixels. Each token
//! starts with a byte `op << 6 | n`: `n < 63` means a length of `n + 1`,
//! `n == 63` means the length follows as a u16. Ops: 0 = literal (that
//! many pixels follow), 1 = run (one pixel follows, repeated), 2 = up (copy
//! the pixels one row above; no payload). Op 3 is invalid.

use alloc::vec::Vec;

pub const START: [u8; 8] = *b"\x1b[P4SHOT";
pub const END: [u8; 8] = *b"\x1b[P4END]";
pub const HEADER_LEN: usize = 16;
/// Pixel format: RGB565, one little-endian u16 per pixel, rows top down.
pub const FORMAT_RGB565: u8 = 1;
/// Largest token length.
const MAX_TOKEN: usize = u16::MAX as usize;
/// Lengths above this are written as `n == 63` plus a u16.
const SHORT_MAX: usize = 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Encoding {
    Raw = 0,
    Rle = 1,
}

impl Encoding {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Raw),
            1 => Some(Self::Rle),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: u16,
    pub height: u16,
    pub format: u8,
    pub encoding: Encoding,
    pub payload_len: u32,
    pub pixels_len: u32,
}

impl Header {
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..2].copy_from_slice(&self.width.to_le_bytes());
        b[2..4].copy_from_slice(&self.height.to_le_bytes());
        b[4] = self.format;
        b[5] = self.encoding as u8;
        b[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        b[12..16].copy_from_slice(&self.pixels_len.to_le_bytes());
        b
    }

    /// Parse and sanity-check a header (known format and encoding,
    /// consistent sizes).
    pub fn parse(b: &[u8]) -> Option<Self> {
        let b: &[u8; HEADER_LEN] = b.get(..HEADER_LEN)?.try_into().ok()?;
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let h = Self {
            width: u16_at(0),
            height: u16_at(2),
            format: b[4],
            encoding: Encoding::from_u8(b[5])?,
            payload_len: u32_at(8),
            pixels_len: u32_at(12),
        };
        let ok = h.format == FORMAT_RGB565
            && h.width > 0
            && h.height > 0
            && h.pixels_len as u64 == h.width as u64 * h.height as u64 * 2
            && (h.encoding != Encoding::Raw || h.payload_len == h.pixels_len);
        ok.then_some(h)
    }
}

/// CRC-32 (IEEE, reflected, as zlib and PNG use it), table driven.
pub struct Crc32(u32);

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    pub const fn new() -> Self {
        Self(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

/// The bytes that wrap a payload: START + header before it, CRC + END after.
pub fn frame_parts(header: &Header, payload: &[u8]) -> ([u8; 8 + HEADER_LEN], [u8; 12]) {
    let hb = header.to_bytes();
    let mut crc = Crc32::new();
    crc.update(&hb);
    crc.update(payload);
    let mut head = [0u8; 8 + HEADER_LEN];
    head[..8].copy_from_slice(&START);
    head[8..].copy_from_slice(&hb);
    let mut tail = [0u8; 12];
    tail[..4].copy_from_slice(&crc.finish().to_le_bytes());
    tail[4..].copy_from_slice(&END);
    (head, tail)
}

/// Pixels as little-endian bytes (the `Raw` payload).
pub fn raw_bytes(pixels: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 2);
    for p in pixels {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out
}

const OP_LITERAL: u8 = 0;
const OP_RUN: u8 = 1;
const OP_UP: u8 = 2;

fn token(out: &mut Vec<u8>, op: u8, len: usize) {
    debug_assert!((1..=MAX_TOKEN).contains(&len));
    if len <= SHORT_MAX {
        out.push((op << 6) | (len - 1) as u8);
    } else {
        out.push((op << 6) | 63);
        out.extend_from_slice(&(len as u16).to_le_bytes());
    }
}

fn flush_literal(out: &mut Vec<u8>, pixels: &[u16]) {
    if !pixels.is_empty() {
        token(out, OP_LITERAL, pixels.len());
        for p in pixels {
            out.extend_from_slice(&p.to_le_bytes());
        }
    }
}

/// Encode `pixels` (rows of `width`) with the `Rle` encoding.
pub fn rle_encode(pixels: &[u16], width: usize, out: &mut Vec<u8>) {
    let n = pixels.len();
    let mut i = 0;
    let mut lit = 0; // start of the pending literal
    while i < n {
        let limit = (n - i).min(MAX_TOKEN);
        let v = pixels[i];
        let run = pixels[i..i + limit].iter().take_while(|&&p| p == v).count();
        let up = if i >= width {
            pixels[i..i + limit].iter().zip(&pixels[i - width..]).take_while(|(a, b)| a == b).count()
        } else {
            0
        };
        if up >= 2 && up >= run {
            flush_literal(out, &pixels[lit..i]);
            token(out, OP_UP, up);
            i += up;
            lit = i;
        } else if run >= 2 {
            flush_literal(out, &pixels[lit..i]);
            token(out, OP_RUN, run);
            out.extend_from_slice(&v.to_le_bytes());
            i += run;
            lit = i;
        } else {
            i += 1;
            if i - lit == MAX_TOKEN {
                flush_literal(out, &pixels[lit..i]);
                lit = i;
            }
        }
    }
    flush_literal(out, &pixels[lit..n]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadToken,
    /// An `up` token in the first row, or tokens past the expected size.
    OutOfRange,
    /// The stream ended before `count` pixels.
    Short,
}

/// Decode an `Rle` stream into exactly `count` pixels of rows of `width`.
pub fn rle_decode(data: &[u8], width: usize, count: usize) -> Result<Vec<u16>, DecodeError> {
    let mut out: Vec<u16> = Vec::with_capacity(count);
    let mut at = 0;
    let byte = |at: usize| data.get(at).copied().ok_or(DecodeError::Truncated);
    let pixel = |at: usize| Ok::<u16, DecodeError>(u16::from_le_bytes([byte(at)?, byte(at + 1)?]));
    while at < data.len() {
        let t = data[at];
        at += 1;
        let mut len = (t & 63) as usize + 1;
        if len == 64 {
            len = u16::from_le_bytes([byte(at)?, byte(at + 1)?]) as usize;
            at += 2;
        }
        if out.len() + len > count {
            return Err(DecodeError::OutOfRange);
        }
        match t >> 6 {
            OP_LITERAL => {
                for _ in 0..len {
                    out.push(pixel(at)?);
                    at += 2;
                }
            }
            OP_RUN => {
                let v = pixel(at)?;
                at += 2;
                out.resize(out.len() + len, v);
            }
            OP_UP => {
                if out.len() < width {
                    return Err(DecodeError::OutOfRange);
                }
                for _ in 0..len {
                    out.push(out[out.len() - width]);
                }
            }
            _ => return Err(DecodeError::BadToken),
        }
    }
    if out.len() != count {
        return Err(DecodeError::Short);
    }
    Ok(out)
}

/// RGB565 to RGB888 with bit replication, so 0xFFFF becomes pure white.
pub fn rgb565_to_rgb888(p: u16) -> [u8; 3] {
    let (r, g, b) = ((p >> 11) & 0x1F, (p >> 5) & 0x3F, p & 0x1F);
    [((r << 3) | (r >> 2)) as u8, ((g << 2) | (g >> 4)) as u8, ((b << 3) | (b >> 2)) as u8]
}

/// zlib's Adler-32, reducing modulo 65521 only every 5552 bytes.
struct Adler32 {
    a: u32,
    b: u32,
}

impl Adler32 {
    fn new() -> Self {
        Self { a: 1, b: 0 }
    }

    fn update(&mut self, data: &[u8]) {
        for chunk in data.chunks(5552) {
            for &byte in chunk {
                self.a += byte as u32;
                self.b += self.a;
            }
            self.a %= 65_521;
            self.b %= 65_521;
        }
    }

    fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A PNG (8-bit RGB) of the pixels, compressed with zlib's "stored"
/// method: larger than a deflated PNG but trivial and fast to produce.
pub fn png_rgb565(pixels: &[u16], width: usize, height: usize) -> Vec<u8> {
    assert_eq!(pixels.len(), width * height, "pixel count does not match the size");
    let row = 1 + width * 3;
    let raw_len = row * height;
    const BLOCK: usize = 65_535;
    let blocks = raw_len.div_ceil(BLOCK).max(1);
    let zlib_len = 2 + raw_len + blocks * 5 + 4;
    let mut out = Vec::with_capacity(8 + 25 + 12 + zlib_len + 12);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&(width as u32).to_be_bytes());
    ihdr[4..8].copy_from_slice(&(height as u32).to_be_bytes());
    ihdr[8] = 8; // bit depth
    ihdr[9] = 2; // truecolour
    png_chunk(&mut out, b"IHDR", &ihdr);

    // IDAT written in place: length, type, zlib stream, then the CRC.
    out.extend_from_slice(&(zlib_len as u32).to_be_bytes());
    let chunk_start = out.len();
    out.extend_from_slice(b"IDAT");
    out.extend_from_slice(&[0x78, 0x01]);
    let mut adler = Adler32::new();
    let mut left_in_block = 0;
    let mut remaining = raw_len;
    let mut line = Vec::with_capacity(row);
    for y in 0..height {
        line.clear();
        line.push(0); // filter: none
        for &p in &pixels[y * width..(y + 1) * width] {
            line.extend_from_slice(&rgb565_to_rgb888(p));
        }
        adler.update(&line);
        let mut rest = &line[..];
        while !rest.is_empty() {
            if left_in_block == 0 {
                let len = remaining.min(BLOCK);
                out.push((len == remaining) as u8); // BFINAL, stored
                out.extend_from_slice(&(len as u16).to_le_bytes());
                out.extend_from_slice(&(!(len as u16)).to_le_bytes());
                left_in_block = len;
            }
            let n = rest.len().min(left_in_block);
            out.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            left_in_block -= n;
            remaining -= n;
        }
    }
    out.extend_from_slice(&adler.finish().to_be_bytes());
    let crc = crc32(&out[chunk_start..]);
    out.extend_from_slice(&crc.to_be_bytes());
    png_chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x3 image shared with scripts/test_screenshot.py, which decodes
    /// the same bytes: the host and the device must agree on the format.
    pub const SAMPLE: [u16; 12] = [
        0xF800, 0xF800, 0xF800, 0x07E0, //
        0xF800, 0xF800, 0xF800, 0x001F, //
        0x1234, 0x5678, 0x5678, 0x5678,
    ];
    pub const SAMPLE_RLE: [u8; 15] = [
        0x42, 0x00, 0xF8, // run 3 of 0xF800
        0x00, 0xE0, 0x07, // literal 1: 0x07E0
        0x82, // up 3
        0x01, 0x1F, 0x00, 0x34, 0x12, // literal 2: 0x001F 0x1234
        0x42, 0x78, 0x56, // run 3 of 0x5678
    ];

    fn encode(pixels: &[u16], width: usize) -> Vec<u8> {
        let mut out = Vec::new();
        rle_encode(pixels, width, &mut out);
        out
    }

    #[test]
    fn crc_matches_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let mut c = Crc32::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.finish(), 0xCBF4_3926);
    }

    #[test]
    fn sample_encodes_to_the_shared_vector() {
        assert_eq!(encode(&SAMPLE, 4), SAMPLE_RLE);
        assert_eq!(rle_decode(&SAMPLE_RLE, 4, 12).unwrap(), SAMPLE);
    }

    #[test]
    fn round_trips_flat_noisy_and_long_runs() {
        let mut seed = 0x1234_5678u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let (w, h) = (480, 800);
        let flat = alloc::vec![0x4208u16; w * h];
        let noisy: Vec<u16> = (0..w * h).map(|_| rnd() as u16).collect();
        // UI-like: bands with a few coloured boxes and some noise.
        let mut ui = alloc::vec![0x0000u16; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                ui[i] = if (x / 60 + y / 100) % 3 == 0 { 0x39E7 } else { (y / 40) as u16 * 0x0841 };
                if rnd() % 97 == 0 {
                    ui[i] = rnd() as u16;
                }
            }
        }
        for (name, img) in [("flat", &flat), ("noisy", &noisy), ("ui", &ui)] {
            let enc = encode(img, w);
            assert_eq!(&rle_decode(&enc, w, w * h).unwrap(), img, "{name}");
        }
        assert!(encode(&flat, w).len() < 64, "flat frame compresses to a few tokens");
        // Worst case: one header byte per 63-pixel literal (+ u16 for long ones).
        assert!(encode(&noisy, w).len() <= w * h * 2 + w * h / 63 * 3 + 3);
    }

    #[test]
    fn literal_longer_than_a_token_is_split() {
        let img: Vec<u16> = (0..70_000u32).map(|i| i as u16).collect();
        // Every pixel differs from its neighbour, so all of it is literal.
        assert!(img.windows(2).all(|p| p[0] != p[1]));
        let enc = encode(&img, 70_000);
        assert_eq!(rle_decode(&enc, 70_000, 70_000).unwrap(), img);
    }

    #[test]
    fn decoder_rejects_corrupt_streams() {
        assert_eq!(rle_decode(&[0x80], 4, 4), Err(DecodeError::OutOfRange), "up in the first row");
        assert_eq!(rle_decode(&[0xC0], 4, 4), Err(DecodeError::BadToken));
        assert_eq!(rle_decode(&[0x41, 0x00], 4, 4), Err(DecodeError::Truncated));
        assert_eq!(rle_decode(&[0x44, 0, 0], 4, 4), Err(DecodeError::OutOfRange), "5 pixels into 4");
        assert_eq!(rle_decode(&[0x42, 0, 0], 4, 4), Err(DecodeError::Short));
        assert_eq!(rle_decode(&[0x7F], 4, 4), Err(DecodeError::Truncated), "long length cut off");
    }

    #[test]
    fn header_round_trip_and_validation() {
        let h = Header { width: 800, height: 480, format: FORMAT_RGB565, encoding: Encoding::Rle, payload_len: 1234, pixels_len: 800 * 480 * 2 };
        assert_eq!(Header::parse(&h.to_bytes()), Some(h));
        let bad_size = Header { pixels_len: 7, ..h };
        assert_eq!(Header::parse(&bad_size.to_bytes()), None);
        let raw_mismatch = Header { encoding: Encoding::Raw, ..h };
        assert_eq!(Header::parse(&raw_mismatch.to_bytes()), None);
        let mut b = h.to_bytes();
        b[5] = 9;
        assert_eq!(Header::parse(&b), None, "unknown encoding");
        assert_eq!(Header::parse(&b[..10]), None, "short");
    }

    #[test]
    fn frame_parts_wrap_with_markers_and_crc() {
        let payload = encode(&SAMPLE, 4);
        let h = Header { width: 4, height: 3, format: FORMAT_RGB565, encoding: Encoding::Rle, payload_len: payload.len() as u32, pixels_len: 24 };
        let (head, tail) = frame_parts(&h, &payload);
        assert_eq!(head[..8], START);
        assert_eq!(tail[4..], END);
        let mut covered = head[8..].to_vec();
        covered.extend_from_slice(&payload);
        assert_eq!(u32::from_le_bytes(tail[..4].try_into().unwrap()), crc32(&covered));
    }

    #[test]
    fn png_is_well_formed() {
        let png = png_rgb565(&SAMPLE, 4, 3);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // Walk the chunks and check every CRC.
        let mut at = 8;
        let mut kinds = Vec::new();
        while at < png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let body = &png[at + 4..at + 8 + len];
            let crc = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
            assert_eq!(crc32(body), crc);
            kinds.push(body[..4].to_vec());
            if &body[..4] == b"IDAT" {
                let z = &body[4..];
                // One stored, final block holding 3 rows of 1 + 4 * 3 bytes.
                assert_eq!(&z[..3], &[0x78, 0x01, 0x01]);
                assert_eq!(u16::from_le_bytes([z[3], z[4]]), 39);
                assert_eq!(&z[7..11], &[0, 0xFF, 0, 0], "filter byte then red");
            }
            at += 12 + len;
        }
        assert_eq!(kinds, [b"IHDR".to_vec(), b"IDAT".to_vec(), b"IEND".to_vec()]);
    }

    #[test]
    fn png_splits_stored_blocks() {
        let (w, h) = (480, 100); // 144_100 bytes of rows: three stored blocks
        let png = png_rgb565(&alloc::vec![0xFFFF; w * h], w, h);
        let raw = h * (1 + w * 3);
        assert_eq!(png.len(), 8 + 25 + 12 + 2 + raw + 3 * 5 + 4 + 12);
    }

    #[test]
    fn adler_matches_reference() {
        let mut a = Adler32::new();
        a.update(b"Wikipedia");
        assert_eq!(a.finish(), 0x11E6_0398);
        // Long input exercises the deferred modulo.
        let mut a = Adler32::new();
        a.update(&alloc::vec![0xFF; 100_000]);
        let (mut x, mut y) = (1u64, 0u64);
        for _ in 0..100_000 {
            x = (x + 0xFF) % 65_521;
            y = (y + x) % 65_521;
        }
        assert_eq!(a.finish() as u64, (y << 16) | x);
    }

    #[test]
    fn rgb_expansion_hits_both_ends() {
        assert_eq!(rgb565_to_rgb888(0xFFFF), [255, 255, 255]);
        assert_eq!(rgb565_to_rgb888(0x0000), [0, 0, 0]);
        assert_eq!(rgb565_to_rgb888(0xF800), [255, 0, 0]);
    }
}
