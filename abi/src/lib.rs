//! The contract between the p4os kernel and isolated user-mode apps.
//!
//! * Syscalls: `ecall` with the call number in `a7`, arguments in `a0..a5`
//!   and the result in `a0`. Negative results are errors ([`err`]).
//! * App images: a 64-byte [`Header`] followed by the position-independent
//!   load image and a table of relocation offsets. The design note is in
//!   `plans/261028-1217-user-mode-apps/design.md`.

#![cfg_attr(not(test), no_std)]

/// Syscall numbers (`a7`).
pub mod nr {
    /// `exit(code) -> !`
    pub const EXIT: u32 = 0;
    /// `write(fd, ptr, len) -> bytes written`. Only fd 1 (console) exists.
    pub const WRITE: u32 = 1;
    /// `sleep_ms(ms) -> 0`
    pub const SLEEP_MS: u32 = 2;
    /// `uptime_ms() -> milliseconds since boot (wraps)`
    pub const UPTIME_MS: u32 = 3;
    /// `yield() -> 0`
    pub const YIELD: u32 = 4;
    /// `getpid() -> thread id`
    pub const GETPID: u32 = 5;
}

/// Error results, returned as negative values in `a0`.
pub mod err {
    pub const EBADF: i32 = -9;
    pub const EFAULT: i32 = -14;
    pub const EINVAL: i32 = -22;
    pub const ENOSYS: i32 = -38;
}

/// The console file descriptor accepted by `write`.
pub const FD_CONSOLE: u32 = 1;

/// "P4AP" in file order.
pub const MAGIC: u32 = u32::from_le_bytes(*b"P4AP");
pub const VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 64;
/// PMP granularity on the ESP32-P4 HP core. Region boundaries (end of the
/// executable part, end of memory, stack size) are multiples of this.
pub const REGION_ALIGN: u32 = 128;
/// Each flash slot holds one image (header, load image and relocations).
pub const SLOT_SIZE: usize = 64 * 1024;
/// Upper bound on the RAM an app may request (image, bss and stack).
pub const MAX_APP_MEM: u32 = 256 * 1024;
pub const MIN_STACK: u32 = 1024;
pub const NAME_LEN: usize = 16;

/// Parsed and validated image header.
///
/// Memory layout after loading at `base` (128-byte aligned):
/// `[base, base+rx_size)` code and read-only data (R+X),
/// `[base+rx_size, base+mem_size)` data and zeroed bss (R+W),
/// `[base+mem_size, base+mem_size+stack_size)` the stack (R+W).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub name: [u8; NAME_LEN],
    /// Bytes of load image following the header.
    pub file_size: u32,
    pub rx_size: u32,
    pub mem_size: u32,
    pub stack_size: u32,
    /// Entry point offset from the load base.
    pub entry: u32,
    /// Number of `u32` offsets after the load image. At each offset the
    /// loader adds the load base to the word stored there.
    pub reloc_count: u32,
    /// CRC-32 (IEEE) of the load image followed by the relocation table.
    pub crc32: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    Truncated,
    BadMagic,
    BadVersion,
    BadLayout,
    TooLarge,
    BadReloc,
    BadChecksum,
}

impl core::fmt::Display for ImageError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(match self {
            ImageError::Truncated => "image truncated",
            ImageError::BadMagic => "no app image",
            ImageError::BadVersion => "unsupported image version",
            ImageError::BadLayout => "invalid memory layout",
            ImageError::TooLarge => "image too large",
            ImageError::BadReloc => "invalid relocation",
            ImageError::BadChecksum => "checksum mismatch",
        })
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl Header {
    /// Parse the header at the start of `bytes` and check that the layout
    /// is self-consistent and fits in `avail` bytes of storage (header
    /// included). Does not verify the checksum; see [`Image::parse`].
    pub fn parse(bytes: &[u8], avail: usize) -> Result<Header, ImageError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ImageError::Truncated);
        }
        if le32(bytes, 0) != MAGIC {
            return Err(ImageError::BadMagic);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        let header_size = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        if version != VERSION || header_size != HEADER_SIZE {
            return Err(ImageError::BadVersion);
        }
        let mut name = [0u8; NAME_LEN];
        name.copy_from_slice(&bytes[8..8 + NAME_LEN]);
        let h = Header {
            name,
            file_size: le32(bytes, 24),
            rx_size: le32(bytes, 28),
            mem_size: le32(bytes, 32),
            stack_size: le32(bytes, 36),
            entry: le32(bytes, 40),
            reloc_count: le32(bytes, 44),
            crc32: le32(bytes, 48),
        };
        h.validate(avail)?;
        Ok(h)
    }

    fn validate(&self, avail: usize) -> Result<(), ImageError> {
        let aligned = |v: u32| v % REGION_ALIGN == 0;
        if !aligned(self.rx_size) || !aligned(self.mem_size) || !aligned(self.stack_size) {
            return Err(ImageError::BadLayout);
        }
        if self.rx_size == 0 || self.rx_size > self.mem_size || self.file_size > self.mem_size {
            return Err(ImageError::BadLayout);
        }
        if self.entry >= self.rx_size || self.entry % 2 != 0 || self.stack_size < MIN_STACK {
            return Err(ImageError::BadLayout);
        }
        let total = self.mem_size.checked_add(self.stack_size).ok_or(ImageError::TooLarge)?;
        if total > MAX_APP_MEM {
            return Err(ImageError::TooLarge);
        }
        if self.stored_size().is_none_or(|s| s > avail) {
            return Err(ImageError::TooLarge);
        }
        Ok(())
    }

    /// Bytes the image occupies in storage, header included.
    pub fn stored_size(&self) -> Option<usize> {
        let relocs = (self.reloc_count as usize).checked_mul(4)?;
        HEADER_SIZE.checked_add(self.file_size as usize)?.checked_add(relocs)
    }

    /// RAM needed to run the app (image, bss and stack).
    pub fn ram_size(&self) -> u32 {
        self.mem_size + self.stack_size
    }

    /// The app name up to the first NUL, or `"?"` if it is not UTF-8.
    pub fn name(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        core::str::from_utf8(&self.name[..len]).unwrap_or("?")
    }
}

/// A complete, checksummed image borrowed from storage.
#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    pub header: Header,
    pub load: &'a [u8],
    relocs: &'a [u8],
}

impl<'a> Image<'a> {
    /// Validate header, checksum and every relocation offset.
    pub fn parse(bytes: &'a [u8]) -> Result<Image<'a>, ImageError> {
        let header = Header::parse(bytes, bytes.len())?;
        let load_end = HEADER_SIZE + header.file_size as usize;
        let end = header.stored_size().ok_or(ImageError::TooLarge)?;
        let image = Image { header, load: &bytes[HEADER_SIZE..load_end], relocs: &bytes[load_end..end] };
        if crc32(&bytes[HEADER_SIZE..end]) != header.crc32 {
            return Err(ImageError::BadChecksum);
        }
        if image.relocs().any(|off| off % 4 != 0 || off as u64 + 4 > header.file_size as u64) {
            return Err(ImageError::BadReloc);
        }
        Ok(image)
    }

    /// Offsets (from the load base) of words that need the base added.
    pub fn relocs(&self) -> impl Iterator<Item = u32> + 'a {
        self.relocs.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
    }
}

/// CRC-32 (IEEE 802.3, reflected, as used by zlib).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(name: &str, load: &[u8], relocs: &[u32], tweak: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
        let mut body = load.to_vec();
        for r in relocs {
            body.extend_from_slice(&r.to_le_bytes());
        }
        let mut h = Vec::new();
        h.extend_from_slice(&MAGIC.to_le_bytes());
        h.extend_from_slice(&VERSION.to_le_bytes());
        h.extend_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        let mut n = [0u8; NAME_LEN];
        n[..name.len()].copy_from_slice(name.as_bytes());
        h.extend_from_slice(&n);
        for v in [load.len() as u32, 128, 256, 1024, 4, relocs.len() as u32, crc32(&body)] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.resize(HEADER_SIZE, 0);
        h.extend_from_slice(&body);
        tweak(&mut h);
        h
    }

    #[test]
    fn crc_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn parses_valid_image() {
        let bytes = build("hello", &[0u8; 200], &[128, 196], |_| {});
        let img = Image::parse(&bytes).unwrap();
        assert_eq!(img.header.name(), "hello");
        assert_eq!(img.header.ram_size(), 256 + 1024);
        assert_eq!(img.load.len(), 200);
        assert_eq!(img.relocs().collect::<Vec<_>>(), [128, 196]);
    }

    #[test]
    fn rejects_bad_magic_and_short_input() {
        assert_eq!(Header::parse(&[0xFF; 64], 64), Err(ImageError::BadMagic));
        assert_eq!(Header::parse(&[0; 10], 64), Err(ImageError::Truncated));
    }

    #[test]
    fn rejects_corrupted_payload() {
        let mut bytes = build("x", &[1u8; 64], &[], |_| {});
        bytes[HEADER_SIZE + 3] ^= 1;
        assert_eq!(Image::parse(&bytes).unwrap_err(), ImageError::BadChecksum);
    }

    #[test]
    fn rejects_relocation_outside_image() {
        let bytes = build("x", &[0u8; 64], &[62], |_| {});
        assert_eq!(Image::parse(&bytes).unwrap_err(), ImageError::BadReloc);
        let bytes = build("x", &[0u8; 64], &[61], |_| {});
        assert_eq!(Image::parse(&bytes).unwrap_err(), ImageError::BadReloc);
    }

    #[test]
    fn rejects_inconsistent_layout() {
        // Entry outside the executable part.
        let bytes = build("x", &[0u8; 64], &[], |h| h[40..44].copy_from_slice(&200u32.to_le_bytes()));
        assert_eq!(Image::parse(&bytes).unwrap_err(), ImageError::BadLayout);
        // Unaligned stack size.
        let bytes = build("x", &[0u8; 64], &[], |h| h[36..40].copy_from_slice(&1000u32.to_le_bytes()));
        assert_eq!(Image::parse(&bytes).unwrap_err(), ImageError::BadLayout);
        // Memory request above the limit.
        let bytes = build("x", &[0u8; 64], &[], |h| h[36..40].copy_from_slice(&(MAX_APP_MEM).to_le_bytes()));
        assert_eq!(Image::parse(&bytes).unwrap_err(), ImageError::TooLarge);
    }

    #[test]
    fn rejects_image_larger_than_storage() {
        let bytes = build("x", &[0u8; 64], &[0, 4], |_| {});
        assert_eq!(Header::parse(&bytes, bytes.len() - 1), Err(ImageError::TooLarge));
        // A huge relocation count must not overflow the size computation.
        let bytes = build("x", &[0u8; 64], &[], |h| h[44..48].copy_from_slice(&u32::MAX.to_le_bytes()));
        assert_eq!(Header::parse(&bytes, SLOT_SIZE), Err(ImageError::TooLarge));
    }
}
