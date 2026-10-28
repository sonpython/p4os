# ESP32-P4 v1.0: in-package 32 MB AP HEX PSRAM bring-up, register level

Date: 2026-10-28. Source: ESP-IDF v5.5 (`~/ref/esp-idf`, commit 8c750b0). Its soc headers have a single `register/` tree, which targets silicon v0.x/v1.x (`ESP32P4_REV_MAX_FULL` 199). All paths below are relative to `components/`.

## Outcome

- **Use 20 MHz first.** It is the only speed IDF runs with no timing tuning. IDF sets `MSPI_TIMING_PSRAM_NEEDS_TUNING 1` for 80, 200 and 250 MHz and leaves it unset for 20 MHz (`esp_hw_support/mspi_timing_tuning/port/esp32p4/mspi_timing_tuning_configs.h:18-29`). At 20 MHz the IDF flow is complete without any tuning code. The DQS phase and delay lines keep their reset values (phase 0 = 67.5°, delay 0).
- **80 MHz is not "no tuning" in IDF.** IDF sweeps the DQS phase (4 values) and then the delay line (31 values) using SPI1 reads (`tuning_scheme_impl/mspi_timing_by_dqs.c:112-221`). Two options:
  - Port that sweep. It needs about 40 lines once the SPI1 read/write primitive from step 7 exists.
  - Run 80 MHz on the reset defaults and gate it on a memory test. This is unsanctioned. One point in its favour: IDF's own mode-register reads run at the final speed before tuning, and on this board they read the vendor ID correctly at 200 MHz.
- **Ranking:** (1) 20 MHz, exact IDF parameters; (2) 80 MHz plus the ported DQS sweep; (3) 80 MHz untuned behind a self-test.
- **Mapping is simple.** The PSRAM has its own 1024-entry MMU, written through the PSRAM controller's index/content register pair. Each entry is `0xC00 | page`, with 64 KiB pages. Vaddr 0x4800_0000 is entry 0, so 32 MB uses entries 0..511.
- **Two ROM dependencies:**
  - Mode-register access goes through the ROM SPI1 user-command helpers. Their source is not public; calling them from Rust at fixed addresses is the low-risk path.
  - Cache invalidation goes through ROM `Cache_*_Addr`. A documented register-level equivalent exists for this one.

Base addresses (from `soc/esp32p4/register/soc/reg_base.h` and `soc/esp32p4/ld/esp32p4.peripherals.ld:15-16`):

| Block | Base |
|---|---|
| SPIMEM2 = PSRAM MSPI0 (AXI/cache side, MMU) | 0x5008_E000 |
| SPIMEM3 = PSRAM MSPI1 (user commands) | 0x5008_F000 |
| HP_SYS_CLKRST | 0x500E_6000 |
| IOMUX_MSPI_PIN | 0x500E_1200 |
| PMU | 0x5011_5000 |
| LP_CLKRST | 0x5011_1000 |
| LPPERI | 0x5012_0000 |
| I2C_ANA_MST | 0x5012_4000 |
| EFUSE | 0x5012_D000 |
| CACHE | 0x3FF1_0000 |

Speed-dependent constants (`esp_psram/device/esp_psram_impl_ap_hex.c:31-61`). The 20 and 80 MHz rows are identical except for the MPLL frequency and the divider.

| | 20 MHz | 80 MHz |
|---|---|---|
| MPLL | 400 MHz | 320 MHz |
| Bus divider (core = MPLL/1) | 20 | 4 |
| `SRAM_CLK` / `SPI1 CLOCK` value | 0x0013_0913 | 0x0003_0103 |
| Read dummy bits (cache/SPI1) | 18 | 18 |
| Register-read dummy bits | 8 | 8 |
| Write dummy bits | 8 | 8 |
| MR0.read_latency code | 2 (10 clk, fixed) | 2 |
| MR4.wr_latency code | 2 | 2 |

## Ordered step list

The notation `R[hi:lo]=v` means read-modify-write of that field. Fields are taken from the `*_reg.h` headers named in each step.

### Step 1: power the PSRAM/MPLL domain (LDO VO2 at 1.9 V)

The defaults are channel 2 at 1900 mV (`esp_hw_support/port/esp32p4/Kconfig.ldo:36-66`). The Kconfig help says "LDO regulator channel that used to power PSRAM and MPLL", so this step must come before step 2. Channel 2 maps to unit 1, and unit 1 maps to `PMU.ext_ldo[3]` = `PMU_EXT_LDO_P1_0P1A` (`hal/esp32p4/include/hal/ldo_ll.h:34,156`; `pmu_reg.h` lines 4336-4440 cover the P1_0P1A/ANA pair). The order follows `esp_hw_support/ldo/esp_ldo_regulator.c:88-96`.

1. Pick `dref` and `mul`:
   - If the eFuse block version is ≥ 1 and both VO2 calibration fields are non-zero, use them. The block version is `EFUSE 0x5012_D04C`, major [12:11] ×100 + minor [10:8]. The calibration fields are `dref` = `0x5012_D04C[31:28]` and `mul` = `0x5012_D050[5:3]`.
   - Otherwise use `dref=7, mul=5`, which gives 0.85 V × 2.25 = 1.9125 V. This is the result of IDF's search loop with K=Vos=C defaults (`ldo_ll.h:69-143`).
2. `0x5011_51D4` (P1_0P1A_ANA): set `[31:28]=dref`, `[25:23]=mul`, and `[26]=1` (en_vdet, "ripple suppression", `ldo_ll.h:276-280`).
3. `0x5011_51D0` (P1_0P1A): set `[14]=0` (tieh: use Vref×mul, not 3.3 V), `[7]=1` (force_tieh_sel = software owner), `[11:9]=0` (tieh_sel), then `[8]=1` (xpd = enable). See `ldo_ll.h:154-213`.
4. Read 0x5011_51D0 before writing and log it. This shows whether the bootloader or eFuse already powered VO2. IDF inserts no settle delay; a ~100 µs wait costs nothing.

### Step 2: MPLL on, set to 400 MHz (20 MHz case) or 320 MHz (80 MHz case)

The flow follows `esp_hw_support/clk_ctrl_os.c:142-198` → `esp_hw_support/port/esp32p4/rtc_clk.c:590-622` → `hal/esp32p4/include/hal/clk_tree_ll.h:152-158,445-460`.

1. `0x5011_515C` (PMU_RF_PWC) `[24]=1`, MSPI_PHY_XPD (default 0).
2. `0x5011_1040` (LP_CLKRST_HP_CLK_CTRL) `[28]=1`, HP_MPLL_500M_CLK_EN (default 1).
3. `0x500E_60BC` (HP_SYS_CLKRST_ANA_PLL_CTRL0) `[9]=0`, MSPI_CAL_STOP cleared to start calibration (`hal/esp32p4/include/hal/regi2c_ctrl_ll.h:90-110`).
4. Analog regi2c, slave block 0x63 (MPLL), register layout from `soc/esp32p4/include/soc/regi2c_mpll.h`:
   - reg 3: `v |= 3<<4` (DHREF)
   - reg 1: write `v & 0xDF`, then `v | 0x20` (pulses IR_CAL_RSTB, bit 5)
   - reg 2: write `(div<<3)|1` with `ref_div=1`, where `div = f/20 - 1`. That is 0x99 for 400 MHz and 0x79 for 320 MHz, from MPLL = 40 MHz × (div+1)/(ref_div+1).
5. Poll `0x500E_60BC[8]` (MSPI_CAL_END) until it reads 1, then set `[9]=1` (CAL_STOP).

There is no regi2c function in the P4 ROM ld. IDF uses a C implementation (`esp_rom/patches/esp_rom_regi2c_esp32p4.c:88-169`), and it is register-only:

- The LP I2C master clock is `0x5012_0000[27]`, reset value 1.
- Select the block: `I2C_ANA_MST_ANA_CONF2 0x5012_4020[23:0]=0` and `ANA_CONF1 0x5012_401C[23:0]=0`. Then set `ANA_CONF2 |= BIT(9)`, which selects MSPI_XTAL/MPLL for block 0x63.
- Read: wait for `I2C0_CTRL 0x5012_4000[25]` (busy) to clear. Write `0x63 | reg<<8`, wait for busy to clear again, and take the data from `[23:16]`.
- Write: wait for busy to clear, write `0x63 | reg<<8 | 1<<24 | data<<16`, then wait for busy to clear.

### Step 3: PSRAM controller clock gates, reset and clock source

Sources: `esp_psram_impl_ap_hex.c:381-386` and `hal/esp32p4/include/hal/psram_ctrlr_ll.h:292-383`.

1. `0x500E_6014` (SOC_CLK_CTRL0) `[31]=1`, psram_sys_clk_en.
2. `0x500E_6030` (PERI_CLK_CTRL00) `[14]=1`, psram_pll_clk_en.
3. Reset pulse on `0x500E_60C0` (HP_RST_EN0): `[23]` 1→0 (rst_en_dual_mspi_axi), then `[25]` 1→0 (rst_en_dual_mspi_apb). Do **not** touch `[22]`/`[24]`; those reset the flash MSPI.
4. `0x500E_6030[13:12]=1`, source MPLL (0 XTAL, 1 MPLL, 2 SPLL, 3 CPLL).
5. `0x500E_6030[15]=1` (core_clk_en) and `[23:16]=0` (core divider 1). Both are already reset values. IDF only writes them on the tuning path (`mspi_timing_config.c:29-35`), so writing them explicitly is free.

### Step 4: pads

Sources: `esp_psram_impl_ap_hex.c:388-389` and `hal/esp32p4/include/hal/mspi_ll.h:214-253`.

1. Drive strength 2:
   - `[13:12]=2` at 0x500E_121C, 220, 224, 228, 22C, 230, 234, 238 (D, Q, WP, HOLD, DQ4-7), 240 (CK), 244 (CS), and 248..264 (DQ8-15, step 4).
   - `[16:15]=2` at 0x500E_123C (DQS0) and 0x500E_1268 (DQS1).
2. DQS receivers on: `[0]=1` (XPD) at 0x500E_123C and 0x500E_1268.
3. Deviation from IDF, recommended: write phase `[2:1]=0` and delays `[10:7]=[20:17]=0` on both DQS registers, and DLC `[7:4]=0` on every data/CK/CS pad. This is what `mspi_timing_psram_config_clear_tuning_regs` does (`mspi_timing_by_dqs.c:199-207`). It guarantees reset defaults even if an earlier IDF app tuned for 200 MHz and the reset you came through did not clear IOMUX_MSPI.

### Step 5: CS timing (SPIMEM2 `SMEM_AC`, 0x5008_E1A0)

`[0]=1` cs_setup, `[6:2]=3` setup_time, `[1]=1` cs_hold, `[11:7]=3` hold_time, `[30:25]=2` hold_delay. Sources: `esp_psram_impl_ap_hex.c:325-333` and `psram_ctrlr_ll.h:440-475`. ECC is off, as in IDF's default, so the ECC hold field is not written.

### Step 6: bus clock and DLL

Sources: `esp_psram_impl_ap_hex.c:396-399` and `psram_ctrlr_ll.h:396-431`.

- `0x5008_E050` (SPIMEM2 SRAM_CLK) = 0x0013_0913 (÷20) or 0x0003_0103 (÷4). The encoding is `N=div-1 [23:16]`, `H=div/2-1 [15:8]`, `L=div-1 [7:0]`.
- `0x5008_F014` (SPIMEM3 CLOCK): same value.
- `0x5008_E190[5]=1` (SMEM_TIMING_CALI.dll for MSPI2) and `0x5008_E180[5]=1` (TIMING_CALI.dll, which IDF uses "for MSPI3").

### Step 7: AP HEX mode registers over SPI1 (SPIMEM3)

IDF calls ROM helpers on `spi_num=3` in OPI-DTR mode with `cs_mask=1<<1` (`psram_ctrlr_ll.h:706-750`). The ROM addresses are in `esp_rom/esp32p4/ld/esp32p4.rom.ld:99-101`:

```
esp_rom_spi_set_op_mode(int spi, u32 mode=7 /*OPI_DTR*/)                     @0x4FC0_0110
esp_rom_spi_cmd_config(int spi, *const esp_rom_spi_cmd_t)                    @0x4FC0_0108
esp_rom_spi_cmd_start(int spi, *mut u8 rx, u16 rx_len, u8 cs_mask, bool we)  @0x4FC0_010C
```

`esp_rom_spi_cmd_t` is `#[repr(C)] { cmd:u16, cmd_bitlen:u16, addr:*const u32, addr_bitlen:u32, tx:*const u32, tx_bitlen:u32, rx:*mut u32, rx_bitlen:u32, dummy_bitlen:u32 }` (`esp_rom/esp32p4/include/esp32p4/rom/opi_flash.h:53-63`). Use 4-byte-aligned buffers. Every transaction uses `cmd_bitlen=16` and `addr_bitlen=32`.

Transaction sequence (`esp_psram_impl_ap_hex.c:142-220`, values set at :401-412):

| # | cmd | addr | dummy | data | action |
|---|---|---|---|---|---|
| 1 | 0x4040 (reg read) | 0 | 8 | rx 16 b | `b0`=MR0, `b1`=MR1 |
| 2 | 0x4040 | 4 | 8 | rx 16 b | `b0`=MR4, `b1`=x |
| 3 | 0xC0C0 (reg write) | 0 | 0 | tx 16 b `[MR0', MR1]` | `MR0' = (MR0 & 0xC0) \| 0x28` (lt=1 bit5, read_latency=2 bits4:2, drive_str=0 bits1:0) |
| 4 | 0xC0C0 | 4 | 0 | tx 16 b `[MR4', x]` | `MR4' = (MR4 & 0x1F) \| 0x40` (wr_latency=2 bits7:5) |
| 5 | 0x4040 | 8 | 8 | rx 8 b | MR8 |
| 6 | 0xC0C0 | 8 | 0 | tx 16 b `[MR8', 0x00]` | `MR8' = (MR8 & 0xB0) \| 0x4B` (bl=3 bits1:0, bt=0 bit2, rbx=1 bit3, x16=1 bit6) |
| 7 | verify: read addr 0 (16 b), 2 (16 b), 4 (8 b), 8 (8 b) | | 8 | | require `MR1[4:0]==0x0D`, `MR2[2:0]==7` (256 Mbit → 32 MB), `MR8[6]==1` |

In IDF the second byte of write #6 is whatever follows the struct in memory. Send 0x00.

If you later want to drop the ROM, the direct register equivalent uses SPIMEM3 user mode (field positions from `spi1_mem_s_reg.h`):

| Register | Address | Fields |
|---|---|---|
| CMD | 0x5008_F000 | `[18]` USR = start; poll until 0 |
| ADDR | 0x5008_F004 | address |
| CTRL | 0x5008_F008 | `[9]` FCMD_OCT, `[6]` FADDR_OCT, `[5]` FDIN_OCT, `[4]` FDOUT_OCT |
| USER | 0x5008_F018 | `[31]` cmd, `[30]` addr, `[29]` dummy, `[28]` miso, `[27]` mosi |
| USER1 | 0x5008_F01C | `[31:26]` addr_bitlen-1, `[5:0]` dummy_cyclelen-1 |
| USER2 | 0x5008_F020 | `[31:28]` cmd_bitlen-1, `[15:0]` cmd |
| MOSI_DLEN / MISO_DLEN | 0x5008_F024 / 028 | `[9:0]` bitlen-1 |
| MISC | 0x5008_F034 | `[0]` CS0_DIS=1, `[1]` CS1_DIS=0 |
| W0..W15 | 0x5008_F058.. | data |
| DDR | 0x5008_F0D4 | `[0]` FMEM_DDR_EN, `[1]` VAR_DUMMY, others |

What the ROM's OPI-DTR mode sets in DDR beyond `DDR_EN`, and how it converts `dummyBitLen` to cycles in DDR, **is not visible in any source**. Treat this path as unverified until you diff the SPIMEM3 registers after a ROM call.

### Step 8: AXI/cache-side controller (SPIMEM2)

Source: `esp_psram_impl_ap_hex.c:289-323`. Field offsets are from `spi_mem_s_reg.h:246-919,1451-1472`.

| Register | Address | Write |
|---|---|---|
| SRAM_DWR_CMD | 0x5008_E04C | = 0xF000_8080 (bitlen-1=15 `[31:28]`, SYNC_WRITE 0x8080) |
| SRAM_DRD_CMD | 0x5008_E048 | = 0xF000_0000 (SYNC_READ 0x0000) |
| CACHE_SCTRL | 0x5008_E040 | `[0]=1` 4-byte addr, `[19:14]=31` addr bitlen-1, `[3]=1` + `[27:22]=7` write dummy, `[4]=1` + `[11:6]=17` read dummy, `[5]=1` rcmd, `[20]=1` wcmd, `[21]=1` sram_oct |
| SRAM_CMD | 0x5008_E044 | `[23]=1` sdummy_wout; `[21],[20],[19],[18]=1` (scmd/saddr/sdout/sdin_oct); `[26],[27]=1` (sdin_hex/sdout_hex) |
| SMEM_DDR | 0x5008_E0D8 | `[0]=1` ddr_en, `[1]=1` var_dummy, `[2]=0`, `[3]=0` (no swap) |
| CACHE_FCTRL | 0x5008_E03C | `[0]=1` axi_req_en, `[31]=0` close_axi_inf_en |
| CTRL1 | 0x5008_E00C | `[25]=1` ar_splice, `[26]=1` aw_splice |

### Step 9: finish

Source: `esp_psram_impl_ap_hex.c:435-437`.

- 20 MHz: no tuning.
- 80 MHz: either run the DQS sweep (option 2) or skip it (option 3).
  - Phase is written at `[2:1]` of 0x500E_123C and 0x500E_1268. Delay lines are at the same registers plus the DLC fields.
  - Test I/O uses SPI1: write cmd 0x8080 with 8 dummy bits, read cmd 0x0000 with 18 dummy bits, ≤64 B per transaction, to PSRAM address 0x80 (`mspi_timing_by_dqs.c:74-110`).
  - Disable `0x5008_F0D4[1]` during the sweep.
- Then set `0x5008_E0D8[1]=1` and `0x5008_F0D4[1]=1` (variable dummy on both).

### Step 10: MMU map 32 MB at 0x4800_0000

Sources: `esp_psram/system_layer/esp_psram.c:229-266` and `hal/esp32p4/include/hal/mmu_ll.h:264-292`. Bit definitions are in `soc/esp32p4/include/soc/ext_mem_defs.h:59-96`.

```
for i in 0..512 { W(0x5008_E380, i); W(0x5008_E37C, 0x800 /*VALID*/ | 0x400 /*ACCESS_PSRAM*/ | i); }
```

- The entry ID is `(vaddr & 0x3FF_FFFF) >> 16`; physical page = `paddr >> 16`, masked to `0x3FF`.
- If flash encryption is on (odd popcount of the eFuse flash_crypt_cnt), also OR in `0x1000` (PSRAM_SENSITIVE) (`mmu_ll.h:108-114,286-288`).
- `cache_ll_l1_enable_bus` is a no-op on P4 (`hal/esp32p4/include/hal/cache_ll.h:1021-1024`). No bus enable is needed.
- The bootloader's `mmu_hal_init` already invalidated every PSRAM entry (`bootloader_support/src/esp32p4/bootloader_esp32p4.c:164-166`, `hal/mmu_hal.c:18-39`).
- `MMU_POWER_CTRL` 0x5008_E384 resets to FORCE_PU=1. Leave it alone.

### Step 11: cache maintenance

The L1 DCache and L2 are write-back; external memory is accessed through L2 (`CACHE_LL_LEVEL_EXT_MEM 2`). Invalidate once after mapping:

- ROM: `Cache_Invalidate_Addr(map=0x30 /*L1_DCACHE BIT4 | L2 BIT5*/, 0x4800_0000, 0x0200_0000)` at 0x4FC0_03E4 (`esp32p4.rom.ld:186`; `rom/cache.h:228-235,566`). `Cache_WriteBack_Addr` at 0x4FC0_03F4 is only needed for DMA coherency and for self-tests.
- Register equivalent: write `CACHE_SYNC_MAP` 0x3FF1_009C = 0x30, `SYNC_ADDR` 0x3FF1_00A0, and `SYNC_SIZE` 0x3FF1_00A4. Then write `SYNC_CTRL` 0x3FF1_0098 = BIT0 (INVALIDATE_ENA; BIT2 = WRITEBACK, BIT3 = WB+INV) and poll `[4]` SYNC_DONE. The ROM's alignment handling is unknown, so pass line-aligned ranges.

The L2 cache configuration needs no PSRAM-specific change. The bootloader enables L2 for flash XIP, and the same cache serves PSRAM. Keep L2 within 0x4FF8_0000..0x4FFC_0000, which the kernel heap already assumes.

### Step 12: self-test

1. Write a pattern with volatile stores.
2. Call `Cache_WriteBack_Addr(0x30, …)`, then `Cache_Invalidate_Addr(0x30, …)`.
3. Read back the first and last 64 KiB plus an address-in-address sweep.

## Pseudo-Rust sketch

```rust
const SPIMEM2: usize = 0x5008_E000; const SPIMEM3: usize = 0x5008_F000;
const CLKRST: usize = soc::HP_SYS_CLKRST; const IOMUX_MSPI: usize = 0x500E_1200;
fn modify(a: usize, mask: u32, val: u32) { mmio::write(a, (mmio::read(a) & !mask) | (val & mask)); }
fn field(a: usize, hi: u32, lo: u32, v: u32) { let m = ((1u32 << (hi - lo + 1)) - 1) << lo; modify(a, m, v << lo); }

#[repr(C)] struct RomSpiCmd { cmd: u16, cmd_bitlen: u16, addr: *const u32, addr_bitlen: u32,
    tx: *const u32, tx_bitlen: u32, rx: *mut u32, rx_bitlen: u32, dummy_bitlen: u32 }
type SetOpMode = unsafe extern "C" fn(i32, u32);
type CmdConfig = unsafe extern "C" fn(i32, *const RomSpiCmd);
type CmdStart  = unsafe extern "C" fn(i32, *mut u8, u16, u8, bool);
type CacheOp   = unsafe extern "C" fn(u32, u32, u32) -> i32;
const ROM_SET_OP_MODE: usize = 0x4FC0_0110; const ROM_CMD_CONFIG: usize = 0x4FC0_0108;
const ROM_CMD_START: usize = 0x4FC0_010C; const ROM_CACHE_INVAL: usize = 0x4FC0_03E4;
const ROM_CACHE_WB: usize = 0x4FC0_03F4;

fn psram_xfer(cmd: u16, addr: u32, dummy: u32, tx: Option<u16>, rx_bits: u32) -> u32 {
    let a = addr; let t = [tx.unwrap_or(0) as u32]; let mut r = [0u32];
    let c = RomSpiCmd { cmd, cmd_bitlen: 16, addr: &a, addr_bitlen: 32,
        tx: if tx.is_some() { t.as_ptr() } else { core::ptr::null() }, tx_bitlen: if tx.is_some() { 16 } else { 0 },
        rx: if rx_bits > 0 { r.as_mut_ptr() } else { core::ptr::null_mut() }, rx_bitlen: rx_bits, dummy_bitlen: dummy };
    unsafe {
        core::mem::transmute::<usize, SetOpMode>(ROM_SET_OP_MODE)(3, 7);
        core::mem::transmute::<usize, CmdConfig>(ROM_CMD_CONFIG)(3, &c);
        core::mem::transmute::<usize, CmdStart>(ROM_CMD_START)(3, r.as_mut_ptr() as *mut u8, (rx_bits / 8) as u16, 1 << 1, false);
    }
    r[0]
}

pub fn psram_init(speed_80: bool) -> Result<usize, u8> {
    let (mpll_div_byte, bus_div) = if speed_80 { (0x79, 4) } else { (0x99, 20) };
    ldo_vo2_enable_1v9();                                   // step 1
    mpll_enable_and_set(mpll_div_byte);                     // step 2 (regi2c, cal poll)
    mmio::set_bits(CLKRST + 0x14, 1 << 31);                 // step 3
    mmio::set_bits(CLKRST + 0x30, 1 << 14);
    for b in [23u32, 25] { mmio::set_bits(CLKRST + 0xC0, 1 << b); mmio::clear_bits(CLKRST + 0xC0, 1 << b); }
    field(CLKRST + 0x30, 13, 12, 1); field(CLKRST + 0x30, 15, 15, 1); field(CLKRST + 0x30, 23, 16, 0);
    pads_drive2_dqs_on_defaults();                          // step 4
    field(SPIMEM2 + 0x1A0, 0, 0, 1); field(SPIMEM2 + 0x1A0, 6, 2, 3);   // step 5
    field(SPIMEM2 + 0x1A0, 1, 1, 1); field(SPIMEM2 + 0x1A0, 11, 7, 3); field(SPIMEM2 + 0x1A0, 30, 25, 2);
    let clk = ((bus_div - 1) << 16) | ((bus_div / 2 - 1) << 8) | (bus_div - 1);
    mmio::write(SPIMEM2 + 0x50, clk); mmio::write(SPIMEM3 + 0x14, clk);   // step 6
    mmio::set_bits(SPIMEM2 + 0x190, 1 << 5); mmio::set_bits(SPIMEM2 + 0x180, 1 << 5);
    let m01 = psram_xfer(0x4040, 0, 8, None, 16);           // step 7
    let m4 = psram_xfer(0x4040, 4, 8, None, 16);
    psram_xfer(0xC0C0, 0, 0, Some((((m01 & 0xC0) | 0x28) | (m01 & 0xFF00)) as u16), 0);
    psram_xfer(0xC0C0, 4, 0, Some((((m4 & 0x1F) | 0x40) | (m4 & 0xFF00)) as u16), 0);
    let m8 = psram_xfer(0x4040, 8, 8, None, 8);
    psram_xfer(0xC0C0, 8, 0, Some(((m8 & 0xB0) | 0x4B) as u16), 0);
    let mr1 = (psram_xfer(0x4040, 0, 8, None, 16) >> 8) & 0x1F;
    let mr2 = psram_xfer(0x4040, 2, 8, None, 16) & 0x7;
    if mr1 != 0x0D { return Err(mr1 as u8); }
    let size = match mr2 { 1 => 4, 3 => 8, 5 => 16, 7 => 32, 6 => 64, _ => 0 } << 20;
    mmio::write(SPIMEM2 + 0x4C, 0xF000_8080); mmio::write(SPIMEM2 + 0x48, 0xF000_0000); // step 8
    let sc = SPIMEM2 + 0x40;
    field(sc, 0, 0, 1); field(sc, 19, 14, 31); field(sc, 3, 3, 1); field(sc, 27, 22, 7);
    field(sc, 4, 4, 1); field(sc, 11, 6, 17); field(sc, 5, 5, 1); field(sc, 20, 20, 1); field(sc, 21, 21, 1);
    mmio::set_bits(SPIMEM2 + 0x44, (1 << 23) | (0xF << 18) | (3 << 26));
    modify(SPIMEM2 + 0xD8, 0xF, 0b0011);
    modify(SPIMEM2 + 0x3C, (1 << 31) | 1, 1);
    mmio::set_bits(SPIMEM2 + 0x0C, (1 << 25) | (1 << 26));
    if speed_80 { /* optional DQS phase+delayline sweep (option 2) */ }   // step 9
    mmio::set_bits(SPIMEM2 + 0xD8, 1 << 1); mmio::set_bits(SPIMEM3 + 0xD4, 1 << 1);
    for i in 0..(size >> 16) as u32 {                        // step 10
        mmio::write(SPIMEM2 + 0x380, i); mmio::write(SPIMEM2 + 0x37C, 0xC00 | i);
    }
    unsafe { core::mem::transmute::<usize, CacheOp>(ROM_CACHE_INVAL)(0x30, 0x4800_0000, size as u32); } // step 11
    Ok(size)                                                // step 12: run memtest before handing to heap
}
```

## Open risks

1. **The ROM SPI1 helpers are a black box.** If one misbehaves, nothing documents what it writes. Before writing a register-only replacement, dump SPIMEM3 0x00..0xE0 after one call. They also rely on the ROM `.bss`/`.data` below 0x4FF4_0000. The kernel heap ends at 0x4FF3_CFC0 and must stay clear of that ROM-reserved band.
2. **80 MHz without tuning is outside IDF's supported envelope.** It can pass a short test and still fail across temperature. Options 1 and 2 avoid this.
3. **VO2 state at entry is unknown.** It may be controlled by eFuse or hardware (`force_tieh_sel=0`). Taking software ownership with an uncalibrated dref/mul gives ≈1.91 V, which is within AP HEX's 1.7–1.95 V class, but it confirms nothing on this board. Log the eFuse calibration values and the pre-existing register values.
4. **Warm-reset residue.** PMU, IOMUX_MSPI and the PSRAM's own mode registers may keep their 200 MHz settings across a non-POR reset. Step 4.3 and the unconditional MR rewrite cover this. A PSRAM global reset command is not issued by IDF and was not researched.
5. **The second byte of each 16-bit MR write is undefined in IDF.** This report sends the read-back byte for MR0/MR4 and 0x00 for MR8. Check with the step-7 readback.
6. **MPLL sharing.** Changing MPLL also changes PLL_F50M/F25M (`clk_tree_ll.h:682-700`). Nothing in the kernel uses them today; flash runs from SPLL.
7. **Cache sync register path.** Its size and alignment semantics are inferred from header text only. Prefer the ROM call.
8. **v1.0 vs v3.** This IDF tree has no rev-3 register set. Upstream master's `esp_psram_impl_ap_hex.c` has no revision branches either; it adds a 250 MHz/MPLL 500 option and ECC page/split tweaks. Register-layout differences on v3 were not checked. Nothing found affects v1.0.

## Limitations

- ROM internals were not disassembled, so the exact register writes made by `esp_rom_spi_*` and `Cache_*` are unknown.
- The AP Memory datasheet was not consulted. MR bit semantics come from IDF's bitfield struct (`esp_psram_impl_ap_hex.c:63-118`).
- Nothing here was run on hardware.

## Unresolved questions

- Does the kernel want 20 MHz now, or should the ~40-line DQS sweep be ported to reach 80 MHz safely?
- Is flash encryption enabled on this board? It decides whether the MMU entries need bit 12.
- Is a PSRAM global reset needed for robust warm restarts? This needs datasheet confirmation of the AP HEX reset command.
