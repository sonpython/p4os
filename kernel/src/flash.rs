//! SPI flash access for the kernel's slice of the `storage` data partition.
//!
//! The kernel executes in place from this same flash through the cache, so
//! nothing may be fetched from flash (code, rodata) or PSRAM (both sit
//! behind the L2 cache) while the SPI1 controller drives the chip. Every
//! operation therefore runs `rom_op`, which lives in internal SRAM
//! (`.sram_text`, see link.x), with interrupts masked and the L2 cache
//! suspended, the sequence ESP-IDF v5.5 uses on the P4
//! (`esp_cache_suspend_ext_mem_cache`): write back the L1 data cache,
//! `Cache_Suspend_L2_Cache`, flash command, `Cache_Resume_L2_Cache`.
//! Data crosses the boundary through a bounce buffer in SRAM `.bss`, and
//! the calling thread's stack must be in internal SRAM too. The other core
//! fetches through the same cache, so it is parked in SRAM for the
//! duration (`smp::with_other_core_parked`).
//!
//! The flash commands are the ROM's legacy SPI flash driver
//! (`esp_rom_spiflash_*`), configured for this chip by the 2nd-stage
//! bootloader, which uses the same functions.
//!
//! Reads use the ROM read under the same critical section rather than a
//! memory-mapped window: it needs no MMU entry of our own and reuses the
//! erase/write path, and settings are only read at boot and after writes.
//! After a write or erase, any cache lines that might hold the old
//! contents (pages mapped by the MMU, which normally covers only the
//! kernel image) are invalidated.

use crate::sync::Mutex;
use core::arch::asm;

pub const SECTOR: u32 = 4096;
/// The `storage` data partition per the partition table: 0x710000, 6 MiB.
pub const PARTITION: (u32, u32) = (0x71_0000, 0x60_0000);
/// The part of it this kernel owns: the first 64 KiB. Everything above
/// belongs to other users, everything below holds the bootloader,
/// partition table and the running kernel; neither is ever touched.
pub const REGION: (u32, u32) = (PARTITION.0, PARTITION.0 + 0x1_0000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    OutOfRange,
    Unaligned,
    /// The calling thread's stack is not in internal SRAM, so it would be
    /// unreachable while the cache is suspended.
    StackNotInSram,
    /// The ROM flash driver state left by the bootloader looks wrong.
    RomNotReady,
    /// The other core could not be parked, so the cache stayed in use.
    OtherCoreBusy,
    /// `esp_rom_spiflash_result_t`: 1 = error, 2 = timeout.
    Rom(i32),
}

// ROM entry points, from esp_rom/esp32p4/ld/esp32p4.rom.ld (ESP-IDF v5.5,
// chip revisions before v3.0).
const ROM_SPIFLASH_ERASE_SECTOR: usize = 0x4FC0_014C;
const ROM_SPIFLASH_WRITE: usize = 0x4FC0_0154;
const ROM_SPIFLASH_READ: usize = 0x4FC0_0158;
const ROM_CACHE_INVALIDATE_ADDR: usize = 0x4FC0_03E4;
const ROM_CACHE_WRITEBACK_ALL: usize = 0x4FC0_0414;
const ROM_CACHE_SUSPEND_L2: usize = 0x4FC0_0508;
const ROM_CACHE_RESUME_L2: usize = 0x4FC0_050C;
/// `esp_rom_spiflash_legacy_data_t *rom_spiflash_legacy_data`.
const ROM_SPIFLASH_LEGACY_DATA: usize = 0x4FF3_FFE8;

// Cache_* `map` arguments (rom/cache.h).
const CACHE_MAP_L1_ICACHE_MASK: u32 = 0b11;
const CACHE_MAP_L1_DCACHE: u32 = 1 << 4;
const CACHE_MAP_L2_CACHE: u32 = 1 << 5;

// Flash MMU (SPI_MEM_C at 0x5008C000): 1024 entries of 64 KiB pages
// mapping 0x40000000.. onto flash.
const MMU_ITEM_CONTENT: usize = 0x5008_C37C;
const MMU_ITEM_INDEX: usize = 0x5008_C380;
const MMU_ENTRIES: u32 = 1024;
const MMU_PAGE: u32 = 0x1_0000;
const MMU_VALID: u32 = 1 << 12;
const MMU_PAGE_MASK: u32 = 0x7FF;
const FLASH_VADDR: u32 = 0x4000_0000;

/// HP L2 memory below the L2 cache: where stacks and `.bss` live.
const SRAM: (usize, usize) = (0x4FF0_0000, 0x4FF8_0000);
/// The ROM keeps its own data above the kernel heap (`_heap_end`).
const ROM_DATA: (usize, usize) = (0x4FF3_CFC0, 0x4FF4_0000);

const OP_READ: u32 = 0;
const OP_WRITE: u32 = 1;
const OP_ERASE: u32 = 2;

/// Bounce buffer for one sector. The mutex also serialises flash access.
static BUF: Mutex<[u32; SECTOR as usize / 4]> = Mutex::new([0; SECTOR as usize / 4]);

/// The flash command itself. Runs from SRAM with only SRAM and ROM
/// reachable, so it must not call into `.text` or load from `.rodata`:
/// plain integer code and ROM calls only. Re-check the disassembly
/// (`objdump -d --section=.sram_text`) after changing it.
#[unsafe(link_section = ".sram_text")]
#[inline(never)]
unsafe extern "C" fn rom_op(op: u32, addr: u32, buf: *mut u32, len: u32) -> i32 {
    unsafe {
        let mstatus: u32;
        asm!("csrrci {0}, mstatus, 0x8", out(reg) mstatus);
        let writeback: unsafe extern "C" fn(u32) -> i32 = core::mem::transmute(ROM_CACHE_WRITEBACK_ALL);
        let suspend: unsafe extern "C" fn() -> u32 = core::mem::transmute(ROM_CACHE_SUSPEND_L2);
        let resume: unsafe extern "C" fn(u32) = core::mem::transmute(ROM_CACHE_RESUME_L2);
        // Dirty L1 lines must not be evicted towards the suspended L2.
        writeback(CACHE_MAP_L1_DCACHE);
        let autoload = suspend();
        let rc = if op == OP_ERASE {
            let erase: unsafe extern "C" fn(u32) -> i32 = core::mem::transmute(ROM_SPIFLASH_ERASE_SECTOR);
            erase(addr / SECTOR)
        } else if op == OP_WRITE {
            let write: unsafe extern "C" fn(u32, *const u32, i32) -> i32 = core::mem::transmute(ROM_SPIFLASH_WRITE);
            write(addr, buf, len as i32)
        } else {
            let read: unsafe extern "C" fn(u32, *mut u32, i32) -> i32 = core::mem::transmute(ROM_SPIFLASH_READ);
            read(addr, buf, len as i32)
        };
        resume(autoload);
        if mstatus & 0x8 != 0 {
            asm!("csrsi mstatus, 0x8");
        }
        rc
    }
}

fn stack_in_sram() -> bool {
    let sp: usize;
    unsafe { asm!("mv {0}, sp", out(reg) sp) };
    (SRAM.0..SRAM.1).contains(&sp)
}

fn check_range(addr: u32, len: usize) -> Result<(), Error> {
    let end = (addr as u64) + len as u64;
    if addr < REGION.0 || end > REGION.1 as u64 {
        return Err(Error::OutOfRange);
    }
    Ok(())
}

/// Run one ROM command on `len` bytes of the locked bounce buffer.
fn run(op: u32, addr: u32, buf: &mut [u32; SECTOR as usize / 4], len: u32) -> Result<(), Error> {
    if !stack_in_sram() {
        return Err(Error::StackNotInSram);
    }
    rom_ready()?;
    let ptr = buf.as_mut_ptr();
    let rc = crate::smp::with_other_core_parked(|| unsafe { rom_op(op, addr, ptr, len) })
        .map_err(|_| Error::OtherCoreBusy)?;
    match rc {
        0 => Ok(()),
        e => Err(Error::Rom(e)),
    }
}

/// Size of the flash chip as configured by the bootloader in the ROM
/// driver state, after sanity checks.
pub fn rom_ready() -> Result<u32, Error> {
    // esp_rom_spiflash_chip_t: device_id, chip_size, block_size,
    // sector_size, page_size, status_mask.
    let data = unsafe { core::ptr::read_volatile(ROM_SPIFLASH_LEGACY_DATA as *const usize) };
    if !(ROM_DATA.0..ROM_DATA.1 - 24).contains(&data) && !(0x4FC0_0000..0x4FC4_0000).contains(&data) {
        return Err(Error::RomNotReady);
    }
    let field = |i: usize| unsafe { core::ptr::read_volatile((data + 4 * i) as *const u32) };
    let (chip_size, sector_size) = (field(1), field(3));
    if chip_size < REGION.1 || sector_size != SECTOR {
        return Err(Error::RomNotReady);
    }
    Ok(chip_size)
}

pub fn read(addr: u32, out: &mut [u8]) -> Result<(), Error> {
    check_range(addr, out.len())?;
    let mut buf = BUF.lock();
    let mut done = 0;
    while done < out.len() {
        // The ROM reads whole words from word-aligned addresses.
        let pos = addr + done as u32;
        let start = pos & !3;
        let skip = (pos - start) as usize;
        let n = (out.len() - done).min(SECTOR as usize - skip);
        let words = (skip + n).div_ceil(4) as u32;
        run(OP_READ, start, &mut buf, words * 4)?;
        let bytes = words_as_bytes(&buf);
        out[done..done + n].copy_from_slice(&bytes[skip..skip + n]);
        done += n;
    }
    Ok(())
}

/// Erase the 4 KiB sector starting at `addr`.
pub fn erase_sector(addr: u32) -> Result<(), Error> {
    if addr % SECTOR != 0 {
        return Err(Error::Unaligned);
    }
    check_range(addr, SECTOR as usize)?;
    let mut buf = BUF.lock();
    run(OP_ERASE, addr, &mut buf, 0)?;
    invalidate_mapped(addr, SECTOR);
    Ok(())
}

/// Program erased flash. `addr` must be word aligned; a partial last word
/// is padded with 0xFF, which leaves those bits unchanged.
pub fn write(addr: u32, data: &[u8]) -> Result<(), Error> {
    if addr % 4 != 0 {
        return Err(Error::Unaligned);
    }
    check_range(addr, data.len())?;
    let mut buf = BUF.lock();
    for (i, chunk) in data.chunks(SECTOR as usize).enumerate() {
        let words = chunk.len().div_ceil(4);
        let bytes = words_as_bytes_mut(&mut buf);
        bytes[..chunk.len()].copy_from_slice(chunk);
        bytes[chunk.len()..words * 4].fill(0xFF);
        let at = addr + (i as u32) * SECTOR;
        run(OP_WRITE, at, &mut buf, words as u32 * 4)?;
    }
    invalidate_mapped(addr, data.len() as u32);
    Ok(())
}

fn words_as_bytes(w: &[u32; SECTOR as usize / 4]) -> &[u8; SECTOR as usize] {
    unsafe { &*(w as *const _ as *const [u8; SECTOR as usize]) }
}

fn words_as_bytes_mut(w: &mut [u32; SECTOR as usize / 4]) -> &mut [u8; SECTOR as usize] {
    unsafe { &mut *(w as *mut _ as *mut [u8; SECTOR as usize]) }
}

/// Serialises the MMU index/content register pair between the cores.
static MMU: crate::spin::SpinLock<()> = crate::spin::SpinLock::new(());

/// Flash MMU entry `i`: the physical page it maps, if valid.
fn mmu_entry(i: u32) -> Option<u32> {
    let v = {
        let _mmu = MMU.lock();
        crate::mmio::write(MMU_ITEM_INDEX, i);
        crate::mmio::read(MMU_ITEM_CONTENT)
    };
    (v & MMU_VALID != 0).then_some(v & MMU_PAGE_MASK)
}

/// Drop cached copies of `[addr, addr+len)` from every virtual page that
/// maps it, so readers through the cache see the new contents.
fn invalidate_mapped(addr: u32, len: u32) {
    let end = addr + len;
    for i in 0..MMU_ENTRIES {
        let Some(page) = mmu_entry(i) else { continue };
        let (pstart, pend) = (page * MMU_PAGE, (page + 1) * MMU_PAGE);
        let (s, e) = (addr.max(pstart), end.min(pend));
        if s < e {
            let vaddr = FLASH_VADDR + i * MMU_PAGE + (s - pstart);
            let invalidate: unsafe extern "C" fn(u32, u32, u32) -> i32 =
                unsafe { core::mem::transmute(ROM_CACHE_INVALIDATE_ADDR) };
            let map = CACHE_MAP_L1_ICACHE_MASK | CACHE_MAP_L1_DCACHE | CACHE_MAP_L2_CACHE;
            // The cache sync engine is shared with psram.rs and both cores.
            let _sync = crate::psram::CACHE_OP.lock();
            unsafe { invalidate(map, vaddr, e - s) };
        }
    }
}

/// Shell `flash`: ROM driver state and where the flash MMU maps pages.
pub fn diagnose() {
    match rom_ready() {
        Ok(size) => crate::kprintln!("flash: {} MiB, ROM driver ready", size >> 20),
        Err(e) => crate::kprintln!("flash: ROM driver not usable: {:?}", e),
    }
    crate::kprintln!(
        "flash: storage {:#x}+{:#x}, kernel config region {:#x}..{:#x}",
        PARTITION.0, PARTITION.1, REGION.0, REGION.1
    );
    let mut mapped = 0;
    let mut first: Option<(u32, u32)> = None;
    for i in 0..MMU_ENTRIES {
        if let Some(page) = mmu_entry(i) {
            mapped += 1;
            first.get_or_insert((i, page));
            if (page * MMU_PAGE..(page + 1) * MMU_PAGE).contains(&REGION.0) {
                crate::kprintln!("flash: config region is mapped at {:#x}", FLASH_VADDR + i * MMU_PAGE);
            }
        }
    }
    if let Some((i, page)) = first {
        crate::kprintln!(
            "flash: {} MMU pages mapped, first {:#x} -> flash {:#x}",
            mapped, FLASH_VADDR + i * MMU_PAGE, page * MMU_PAGE
        );
    }
}
