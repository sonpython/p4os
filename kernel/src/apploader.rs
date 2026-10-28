//! Load user app images from the flash "storage" partition and start them
//! as isolated U-mode threads.
//!
//! Slots of 64 KiB start at flash offset 0x720000 (slot N at
//! 0x720000 + N * 0x10000, N < 14; 0x710000..0x71FFFF belongs to another
//! user of the partition). The slots are mapped read-only through the flash
//! cache MMU at the top of the flash window, 0x43F00000, where nothing else
//! lives. Images are checked (header, layout, CRC, relocations) before any
//! RAM is allocated. Format: `p4os_abi` and scripts/mkapp.py.

use crate::usermode::{self, AppStatus, Region, UserApp};
use crate::{irq, kprintln, mmio, pmp, psram};
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use p4os_abi::{Header, Image, ImageError, SLOT_SIZE};

pub const SLOTS: usize = 14;
const FLASH_BASE: usize = 0x72_0000;
const MMU_PAGE: usize = 64 * 1024;

/// Flash MMU (SPI_MEM_C, ESP-IDF `mmu_ll_write_entry` for MMU id 0).
const FLASH_MMU_ITEM_CONTENT: usize = 0x5008_C37C;
const FLASH_MMU_ITEM_INDEX: usize = 0x5008_C380;
const FLASH_MMU_VALID: u32 = 1 << 12;
/// First of the 16 MMU entries at the top of the 64 MiB flash window.
const MAP_FIRST_ENTRY: usize = 0x3F0;
const MAP_VADDR: usize = 0x4000_0000 + MAP_FIRST_ENTRY * MMU_PAGE;
const _: () = assert!(SLOTS * SLOT_SIZE <= 16 * MMU_PAGE && SLOT_SIZE == MMU_PAGE);
const _: () = assert!(FLASH_BASE + SLOTS * SLOT_SIZE <= 0x80_0000);

/// ROM cache maintenance (ESP-IDF `esp32p4.rom.ld`, `rom/cache.h`).
const ROM_CACHE_INVALIDATE_ADDR: usize = 0x4FC0_03E4;
const ROM_CACHE_WRITEBACK_ADDR: usize = 0x4FC0_03F4;
const CACHE_MAP_L1_ICACHE_MASK: u32 = (1 << 0) | (1 << 1);
const CACHE_MAP_L1_DCACHE: u32 = 1 << 4;
const CACHE_MAP_L2_CACHE: u32 = 1 << 5;

/// Thread names per slot (the scheduler keeps `&'static str` names).
const THREAD_NAMES: [&str; SLOTS] = [
    "app0", "app1", "app2", "app3", "app4", "app5", "app6", "app7", "app8", "app9", "app10", "app11", "app12",
    "app13",
];

#[derive(Debug)]
pub enum LoadError {
    NoSuchSlot,
    Image(ImageError),
    Unsupported(&'static str),
    OutOfMemory,
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            LoadError::NoSuchSlot => write!(f, "slot must be 0..{}", SLOTS - 1),
            LoadError::Image(e) => write!(f, "{}", e),
            LoadError::Unsupported(why) => write!(f, "user mode unavailable: {}", why),
            LoadError::OutOfMemory => f.write_str("out of memory"),
        }
    }
}

impl From<ImageError> for LoadError {
    fn from(e: ImageError) -> Self {
        LoadError::Image(e)
    }
}

static MAPPED: AtomicBool = AtomicBool::new(false);

/// Map the app slots. Call once during boot, before core 1 starts: the
/// MMU index/content register pair is shared state (`flash.rs` walks it
/// too), so it is not rewritten at run time.
pub fn init() {
    irq::free(|| {
        for page in 0..SLOTS {
            mmio::write(FLASH_MMU_ITEM_INDEX, (MAP_FIRST_ENTRY + page) as u32);
            mmio::write(FLASH_MMU_ITEM_CONTENT, FLASH_MMU_VALID | ((FLASH_BASE / MMU_PAGE + page) as u32));
        }
    });
    MAPPED.store(true, Ordering::Release);
}

/// The raw contents of `slot`, read through the flash cache.
fn slot_bytes(slot: usize) -> Result<&'static [u8], LoadError> {
    if slot >= SLOTS {
        return Err(LoadError::NoSuchSlot);
    }
    if !MAPPED.load(Ordering::Acquire) {
        return Err(LoadError::Unsupported("app slots not mapped"));
    }
    let addr = MAP_VADDR + slot * SLOT_SIZE;
    // Drop anything cached for this window, e.g. from before a reflash.
    psram::cache_invalidate(addr, SLOT_SIZE);
    Ok(unsafe { core::slice::from_raw_parts(addr as *const u8, SLOT_SIZE) })
}

/// Header of a valid image in `slot` (checksum verified).
pub fn probe(slot: usize) -> Result<Header, LoadError> {
    Ok(Image::parse(slot_bytes(slot)?)?.header)
}

/// Print the valid slots (shell `apps`).
pub fn list() {
    let mut found = 0;
    for slot in 0..SLOTS {
        match probe(slot) {
            Ok(h) => {
                found += 1;
                kprintln!(
                    "slot {:>2}: {:<16} {:>6} bytes, {:>3} KiB RAM",
                    slot,
                    h.name(),
                    h.stored_size().unwrap_or(0),
                    h.ram_size().div_ceil(1024)
                );
            }
            Err(LoadError::Image(ImageError::BadMagic)) => {}
            Err(e) => kprintln!("slot {:>2}: invalid ({})", slot, e),
        }
    }
    if found == 0 {
        kprintln!("no apps (flash one with scripts/flash-app.sh SLOT IMAGE)");
    }
}

/// Load the app in `slot` into fresh RAM and start it with `arg` in a0.
pub fn run(slot: usize, arg: u32) -> Result<(usize, Arc<AppStatus>), LoadError> {
    let image = Image::parse(slot_bytes(slot)?)?;
    let hart = pmp::app_hart().map_err(LoadError::Unsupported)?;
    let h = image.header;

    let mut region = Region::alloc(h.ram_size() as usize).ok_or(LoadError::OutOfMemory)?;
    let base = region.base() as u32;
    let mem = region.bytes_mut();
    mem[..image.load.len()].copy_from_slice(image.load);
    for off in image.relocs() {
        // Offsets were bounds-checked against the image by Image::parse.
        let word = &mut mem[off as usize..off as usize + 4];
        let value = u32::from_le_bytes([word[0], word[1], word[2], word[3]]).wrapping_add(base);
        word.copy_from_slice(&value.to_le_bytes());
    }
    sync_code(base as usize, h.rx_size as usize);

    let app = UserApp::new(String::from(h.name()), region, h.rx_size as usize, h.entry as usize, arg);
    // Logged before the start: the app runs on another core and would
    // interleave its own output with this line.
    kprintln!("app '{}': core {}, {} bytes RAM at {:#010x}", h.name(), hart, h.ram_size(), base);
    Ok(usermode::spawn(hart, THREAD_NAMES[slot], app))
}

/// Make freshly written code visible to instruction fetch: write the data
/// cache back and drop stale instruction cache lines (both cores).
fn sync_code(addr: usize, size: usize) {
    let dcache = if psram::BASE <= addr && addr < psram::BASE + 0x0400_0000 {
        CACHE_MAP_L1_DCACHE | CACHE_MAP_L2_CACHE
    } else {
        CACHE_MAP_L1_DCACHE // internal SRAM is cached by L1 only
    };
    unsafe {
        let writeback: unsafe extern "C" fn(u32, u32, u32) -> i32 = core::mem::transmute(ROM_CACHE_WRITEBACK_ADDR);
        let invalidate: unsafe extern "C" fn(u32, u32, u32) -> i32 = core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR);
        writeback(dcache, addr as u32, size as u32);
        invalidate(CACHE_MAP_L1_ICACHE_MASK, addr as u32, size as u32);
        core::arch::asm!("fence");
    }
}
