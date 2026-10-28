//! ESP-IDF application descriptor.
//!
//! The 2nd-stage bootloader refuses an image whose segment 0 does not start
//! with this structure, and reads the eFuse block revision range and MMU page
//! size from it. Layout matches `esp_app_desc_t` (256 bytes).

#[repr(C)]
pub struct AppDesc {
    magic_word: u32,
    secure_version: u32,
    reserv1: [u32; 2],
    version: [u8; 32],
    project_name: [u8; 32],
    time: [u8; 16],
    date: [u8; 16],
    idf_ver: [u8; 32],
    app_elf_sha256: [u8; 32],
    min_efuse_blk_rev_full: u16,
    max_efuse_blk_rev_full: u16,
    mmu_page_size: u8,
    reserv3: [u8; 3],
    reserv2: [u32; 18],
}

const _: () = assert!(core::mem::size_of::<AppDesc>() == 256);

const fn padded<const N: usize>(s: &str) -> [u8; N] {
    let bytes = s.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < bytes.len() && i < N - 1 {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

#[unsafe(link_section = ".app_desc")]
#[used]
#[unsafe(no_mangle)]
pub static APP_DESC: AppDesc = AppDesc {
    magic_word: 0xABCD_5432,
    secure_version: 0,
    reserv1: [0; 2],
    version: padded(env!("CARGO_PKG_VERSION")),
    project_name: padded("p4os"),
    time: [0; 16],
    date: [0; 16],
    idf_ver: padded("none"),
    app_elf_sha256: [0; 32],
    min_efuse_blk_rev_full: 0,
    max_efuse_blk_rev_full: 99,
    // log2(64 KiB): the flash MMU page size the bootloader was built with.
    mmu_page_size: 16,
    reserv3: [0; 3],
    reserv2: [0; 18],
};
