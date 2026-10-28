//! FAT filesystem on the SD card, exposed to apps under `/sd`.
//!
//! `fatfs` (rust-fatfs, long file names enabled) runs on top of `SdDisk`,
//! a byte-addressed view of the card. Whole sectors go straight to the
//! card; partial sector accesses use a one-sector cache and write through
//! immediately, so nothing needs flushing on power loss beyond FAT's own
//! metadata ordering.

use crate::sdmmc::{self, BLOCK};
use crate::sync::Mutex;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions, IoBase, Read, Seek, SeekFrom, Write};
use p4os_sdk::{DirEntry, FileReader, FsError};

/// Most sectors moved per card command when reading straight into the
/// caller's buffer.
const MAX_DIRECT_BLOCKS: usize = 64;

/// The card error is only read through `Debug` when a failure is logged.
#[derive(Debug)]
#[allow(dead_code)]
pub enum DiskError {
    Sd(sdmmc::Error),
    UnexpectedEof,
    WriteZero,
}

impl fatfs::IoError for DiskError {
    fn is_interrupted(&self) -> bool {
        false
    }
    fn new_unexpected_eof_error() -> Self {
        DiskError::UnexpectedEof
    }
    fn new_write_zero_error() -> Self {
        DiskError::WriteZero
    }
}

pub struct SdDisk {
    /// First sector of the FAT volume (0 for a card without partitions).
    base: u64,
    size: u64,
    pos: u64,
    cache: Vec<u8>,
    cache_lba: Option<u64>,
    /// Whole sectors written back-to-back are collected here and sent as
    /// one multi-block write: one command per sector is far slower because
    /// every SD write command waits for the card's programming time.
    pending: Vec<u8>,
    pending_lba: u64,
}

impl SdDisk {
    fn new(base: u64, size: u64) -> Self {
        Self { base, size, pos: 0, cache: vec![0; BLOCK], cache_lba: None, pending: Vec::new(), pending_lba: 0 }
    }

    fn flush_pending(&mut self) -> Result<(), DiskError> {
        if !self.pending.is_empty() {
            sdmmc::write_blocks(self.base + self.pending_lba, &self.pending).map_err(DiskError::Sd)?;
            self.pending.clear();
        }
        Ok(())
    }

    fn load(&mut self, lba: u64) -> Result<(), DiskError> {
        if self.cache_lba != Some(lba) {
            sdmmc::read_blocks(self.base + lba, &mut self.cache).map_err(DiskError::Sd)?;
            self.cache_lba = Some(lba);
        }
        Ok(())
    }
}

impl IoBase for SdDisk {
    type Error = DiskError;
}

impl Read for SdDisk {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, DiskError> {
        self.flush_pending()?;
        let remaining = self.size.saturating_sub(self.pos) as usize;
        let len = buf.len().min(remaining);
        if len == 0 {
            return Ok(0);
        }
        let lba = self.pos / BLOCK as u64;
        let offset = (self.pos % BLOCK as u64) as usize;
        let n = if offset == 0 && len >= BLOCK {
            let blocks = (len / BLOCK).min(MAX_DIRECT_BLOCKS);
            let n = blocks * BLOCK;
            sdmmc::read_blocks(self.base + lba, &mut buf[..n]).map_err(DiskError::Sd)?;
            n
        } else {
            self.load(lba)?;
            let n = (BLOCK - offset).min(len);
            buf[..n].copy_from_slice(&self.cache[offset..offset + n]);
            n
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for SdDisk {
    fn write(&mut self, buf: &[u8]) -> Result<usize, DiskError> {
        let lba = self.pos / BLOCK as u64;
        let offset = (self.pos % BLOCK as u64) as usize;
        let n = if offset == 0 && buf.len() >= BLOCK {
            let n = (buf.len() / BLOCK).min(MAX_DIRECT_BLOCKS) * BLOCK;
            let pending_blocks = (self.pending.len() / BLOCK) as u64;
            let contiguous = !self.pending.is_empty() && lba == self.pending_lba + pending_blocks;
            if !contiguous || self.pending.len() + n > MAX_DIRECT_BLOCKS * BLOCK {
                self.flush_pending()?;
                self.pending_lba = lba;
            }
            self.pending.extend_from_slice(&buf[..n]);
            let blocks = (n / BLOCK) as u64;
            if self.cache_lba.is_some_and(|c| c >= lba && c < lba + blocks) {
                self.cache_lba = None;
            }
            n
        } else {
            self.flush_pending()?;
            self.load(lba)?;
            let n = (BLOCK - offset).min(buf.len());
            self.cache[offset..offset + n].copy_from_slice(&buf[..n]);
            sdmmc::write_blocks(self.base + lba, &self.cache).map_err(DiskError::Sd)?;
            n
        };
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), DiskError> {
        self.flush_pending()
    }
}

impl Seek for SdDisk {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, DiskError> {
        let new = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => self.size as i64 + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if new < 0 {
            return Err(DiskError::UnexpectedEof);
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

type Fs = FileSystem<SdDisk>;

static FS: Mutex<Option<Fs>> = Mutex::new(None);

/// Find the FAT volume: a partition from the MBR, or the whole card when
/// sector 0 is itself a FAT boot sector ("superfloppy" layout).
fn volume_start(sector0: &[u8]) -> u64 {
    let is_boot_sector = matches!(sector0[0], 0xEB | 0xE9) && &sector0[510..512] == [0x55, 0xAA];
    let looks_fat = &sector0[0x52..0x57] == b"FAT32" || &sector0[0x36..0x39] == b"FAT";
    if is_boot_sector && looks_fat {
        return 0;
    }
    for p in 0..4 {
        let e = &sector0[446 + p * 16..462 + p * 16];
        if matches!(e[4], 0x01 | 0x04 | 0x06 | 0x0B | 0x0C | 0x0E) {
            return u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
        }
    }
    0
}

pub fn mount() -> Result<(), String> {
    let blocks = sdmmc::init().map_err(|e| alloc::format!("card: {:?}", e))?;
    let mut s0 = vec![0u8; BLOCK];
    sdmmc::read_blocks(0, &mut s0).map_err(|e| alloc::format!("read: {:?}", e))?;
    let base = volume_start(&s0);
    let disk = SdDisk::new(base, (blocks - base) * BLOCK as u64);
    let fs = FileSystem::new(disk, FsOptions::new()).map_err(|e| alloc::format!("fat: {:?}", e))?;
    crate::kprintln!("fs: FAT volume at sector {} ({:?})", base, fs.fat_type());
    *FS.lock() = Some(fs);
    Ok(())
}

/// First sector of the FAT32 partition created by `format`: 1 MiB, the
/// alignment SD cards and desktop systems expect.
const PARTITION_START: u64 = 2048;
/// Folders the built-in apps use.
const STANDARD_DIRS: [&str; 7] = ["Music", "Pictures", "Documents", "Notes", "Paint", "Apps", "p4os"];

/// Erase the card and create the p4os layout: an MBR with one FAT32 (LBA)
/// partition at 1 MiB, volume label P4OS, and the standard folders.
pub fn format() -> Result<(), String> {
    // Drop the mounted filesystem first (flushes and releases the card).
    *FS.lock() = None;
    let blocks = sdmmc::init().map_err(|e| alloc::format!("card: {:?}", e))?;
    let sectors = blocks - PARTITION_START;

    let mut mbr = vec![0u8; BLOCK];
    let entry = &mut mbr[446..462];
    entry[0] = 0x00; // not bootable
    entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS unused (LBA only)
    entry[4] = 0x0C; // FAT32 with LBA
    entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&(PARTITION_START as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&(sectors as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    sdmmc::write_blocks(0, &mbr).map_err(|e| alloc::format!("write MBR: {:?}", e))?;

    let mut disk = SdDisk::new(PARTITION_START, sectors * BLOCK as u64);
    let options = FormatVolumeOptions::new().fat_type(FatType::Fat32).bytes_per_cluster(32 * 1024).volume_label(*b"P4OS       ");
    let t0 = crate::time::ticks();
    fatfs::format_volume(&mut disk, options).map_err(|e| alloc::format!("format: {:?}", e))?;
    disk.flush().map_err(|e| alloc::format!("flush: {:?}", e))?;
    crate::kprintln!("fs: FAT32 structures written in {} ms", crate::time::ticks() - t0);

    mount()?;
    with_fs(|fs| {
        for dir in STANDARD_DIRS {
            fs.root_dir().create_dir(dir).map_err(map_err)?;
        }
        fs.flush().map_err(map_err)
    })
    .map_err(|e| alloc::format!("create folders: {:?}", e))?;
    crate::kprintln!("fs: formatted {} MiB FAT32, folders: {:?}", sectors / 2048, STANDARD_DIRS);
    Ok(())
}

fn sd_path(path: &str) -> Result<&str, FsError> {
    let rest = path.strip_prefix("/sd").ok_or(FsError::NotFound)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return Err(FsError::NotFound);
    }
    Ok(rest.trim_matches('/'))
}

fn map_err<E>(e: fatfs::Error<E>) -> FsError {
    match e {
        fatfs::Error::NotFound => FsError::NotFound,
        fatfs::Error::InvalidInput => FsError::NotADirectory,
        _ => FsError::Io,
    }
}

fn with_fs<R>(f: impl FnOnce(&Fs) -> Result<R, FsError>) -> Result<R, FsError> {
    let guard = FS.lock();
    match guard.as_ref() {
        Some(fs) => f(fs),
        None => Err(FsError::NoMedium),
    }
}

#[derive(Default)]
pub struct SdFs;

impl p4os_sdk::FileSystem for SdFs {
    fn list(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        if path == "/" {
            let mounted = FS.lock().is_some();
            return Ok(if mounted { vec![DirEntry { name: "sd".into(), is_dir: true, size: 0 }] } else { Vec::new() });
        }
        let rel = sd_path(path)?;
        with_fs(|fs| {
            let root = fs.root_dir();
            let dir = if rel.is_empty() { root } else { root.open_dir(rel).map_err(map_err)? };
            let mut out = Vec::new();
            for entry in dir.iter() {
                let e = entry.map_err(map_err)?;
                let name = e.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                out.push(DirEntry { name, is_dir: e.is_dir(), size: e.len() });
            }
            out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
            Ok(out)
        })
    }

    fn read(&mut self, path: &str) -> Result<Vec<u8>, FsError> {
        let rel = sd_path(path)?;
        with_fs(|fs| {
            let mut f = fs.root_dir().open_file(rel).map_err(map_err)?;
            let mut out = Vec::new();
            let mut chunk = vec![0u8; 32 * 1024];
            loop {
                let n = f.read(&mut chunk).map_err(map_err)?;
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Ok(out)
        })
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let rel = sd_path(path)?;
        with_fs(|fs| {
            let mut f = fs.root_dir().create_file(rel).map_err(map_err)?;
            f.truncate().map_err(map_err)?;
            f.write_all(data).map_err(map_err)?;
            f.flush().map_err(map_err)?;
            fs.flush().map_err(map_err)
        })
    }

    fn open(&mut self, path: &str) -> Result<Box<dyn FileReader>, FsError> {
        let rel = String::from(sd_path(path)?);
        let size = with_fs(|fs| {
            let f = fs.root_dir().open_file(&rel).map_err(map_err)?;
            let mut f = f;
            f.seek(SeekFrom::End(0)).map_err(map_err)
        })?;
        Ok(Box::new(SdFile { path: rel, offset: 0, size }))
    }

    fn create_dir(&mut self, path: &str) -> Result<(), FsError> {
        let rel = sd_path(path)?;
        with_fs(|fs| {
            fs.root_dir().create_dir(rel).map_err(map_err)?;
            fs.flush().map_err(map_err)
        })
    }

    fn remove(&mut self, path: &str) -> Result<(), FsError> {
        let rel = sd_path(path)?;
        with_fs(|fs| {
            fs.root_dir().remove(rel).map_err(map_err)?;
            fs.flush().map_err(map_err)
        })
    }
}

/// Streaming reader that reopens the file per read, so it holds no borrow
/// of the filesystem between calls (the audio thread reads it).
struct SdFile {
    path: String,
    offset: u64,
    size: u64,
}

impl FileReader for SdFile {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, FsError> {
        let n = with_fs(|fs| {
            let mut f = fs.root_dir().open_file(&self.path).map_err(map_err)?;
            f.seek(SeekFrom::Start(self.offset)).map_err(map_err)?;
            f.read(buf).map_err(map_err)
        })?;
        self.offset += n as u64;
        Ok(n)
    }

    fn size(&self) -> u64 {
        self.size
    }
}
