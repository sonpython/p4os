# ESP32-P4 EV Board 7" EK79007 MIPI-DSI: bare-metal bring-up sequence

Date: 2026-10-28 · Chip: ESP32-P4 v1.0 · Reference: ESP-IDF v5.5.0 (`8c750b0`), esp-bsp `f259a6a` (BSP 5.2.3), esp_lcd_ek79007 2.0.2 (esp-iot-solution `35b831c`), cross-checked against esp-hal `5c0820c` (`esp-hal/src/mipi_dsi`).

## Outcome

The IDF stack reduces to about 70 MMIO writes. Five blocks are involved: LDO VO3 at 2.5 V, the HP_SYS_CLKRST gates, the DSI host plus D-PHY, the DSI bridge, and a single **DW-GDMA** channel. The DW-GDMA channel streams the PSRAM framebuffer into the bridge FIFO at `0x5010_5000`. DMA2D is **not** needed. IDF uses DMA2D only for `draw_bitmap` copies.

You must provide two things yourself:
- **A per-frame ISR.** IDF uses a single-shot LLI and re-arms it in the DW-GDMA "DMA transfer done" interrupt (source 24). Without the ISR, only one frame is shown.
- **Working PSRAM and cache write-back.** You need PSRAM up, with ROM `Cache_WriteBack_Addr`, before the DMA can see your pixels.

The BSP values are 2 lanes at 1000 Mbps per lane and a nominal 52 MHz DPI clock. In practice the DPI clock comes out at **60 MHz** (240/4), which gives about 69.7 Hz. That is what the IDF demo actually ran at. Backlight is **GPIO26** and reset is **GPIO27** (active low). The "GPIO 23" warning in your log is explained in §4.

Base addresses used below:

| Block | Base |
|---|---|
| HP_SYS_CLKRST | `0x500E_6000` |
| DSI_HOST | `0x500A_0000` |
| DSI_BRG | `0x500A_0800` |
| DSI_BRG pixel FIFO (DMA dst) | `0x5010_5000` |
| DW_GDMA (DMAC) | `0x5008_1000` |
| PMU | `0x5011_5000` |
| EFUSE | `0x5012_D000` |
| GPIO | `0x500E_0000` |
| IO_MUX | `0x500E_1000` |
| USB_WRAP | `0x5008_0000` |

(reg_base.h:9-169, esp32p4.peripherals.ld:99-104)

---

## 1. Ordered bring-up (IDF order, RGB565, 1 framebuffer)

RMW means read-modify-write of the named field only. "=" means a full write of the value IDF ends up with (assuming reset defaults).

### Step 0. Backlight off, reset pin as output
- Configure GPIO26 and GPIO27 as GPIO outputs and drive both **low** (see §4). The BSP does this through LEDC duty 0 (BSP .c:474-499, 571). With GPIO27 low, the panel is held in reset.

### Step 1. DSI PHY power: LDO VO3 → 2.5 V (BSP .c:537-547, display.h:57-58)
Channel 3 maps to unit 2, then to `PMU.ext_ldo[1]` = `EXT_LDO_P0_0P2A` (ldo_ll.h:156,187,207; pmu_reg.h:4137,4205).

1. `PMU+0x1C4` (EXT_LDO_P0_0P2A_ANA). IDF writes `dref[31:28]`, `mul[25:23]`, sets `en_vdet[26]=1` and leaves `en_cur_lim[27]=0`. Nominal value without eFuse trim: dref=9, mul=6, giving **`0x9700_0000`** (esp-hal writes `0x9300_0000`, without en_vdet).
   - eFuse trim (ldo_ll.h:69-143). Read `EFUSE+0x4C` (RD_MAC_SYS_2), bits[10:8] minor and [12:11] major. If `major*100+minor >= 1`, read `EFUSE+0x50` (RD_MAC_SYS_3): `ldo_vo3_k[13:6]`, `vos[19:14]`, `c[25:20]`.
   - Nonzero fields give K=975±(k&0x7F) with sign bit 0x80, Vos=±(vos&0x1F)-3 with sign bit 0x20, and C=990±(c&0x1F) with sign bit 0x20.
   - Then pick the (dref 0..15, mul 0..7) that minimises `|2500*80000 - (vref20*K + 20*Vos)*(4000 + mul*C)|`, where vref20 = `dref<9 ? 10+dref : 20+(dref-9)*2`. Port this loop; do not hard-code 9/6 if the eFuse holds trim values.
2. `PMU+0x1C0` (EXT_LDO_P0_0P2A) = **`0x4020_0180`**. This sets force_tieh_sel[7]=1 (SW owner), xpd[8]=1 (on), tieh_sel=0 and tieh=0, and keeps target1=0x40 and target0=0x80 at their defaults. Matches esp-hal `dphy_ldo_power_on()` (mipi_dsi/mod.rs:348-357).

### Step 2. Clocks and resets (esp_lcd_mipi_dsi_bus.c:41-60; mipi_dsi_ll.h:30-170)
| Register | Action |
|---|---|
| `CLKRST+0x18` SOC_CLK_CTRL1 | set bit12 `dsi_sys_clk_en` (DSI host + bridge APB) |
| `CLKRST+0xC0` HP_RST_EN0 | bit26 `rst_en_dsi_brg` 1 → 0 |
| `CLKRST+0x2C` REF_CLK_CTRL2 | ensure bit8 `ref_20m_clk_en`=1 (default 1). PLL_F20M = SPLL 480 / (REF_CLK_CTRL1[23:16]=23, +1) |
| `CLKRST+0x38` PERI_CLK_CTRL02 | `mipi_dsi_dphy_clk_src_sel[31:30]` = 0 (PLL_F20M, the IDF default, clk_tree_defs.h:442) |
| `CLKRST+0x3C` PERI_CLK_CTRL03 | set bit0 `dphy_cfg_clk_en`, bit1 `dphy_pll_refclk_en` |

### Step 3. DSI host + D-PHY init (mipi_dsi_hal.c:15-28)
| Reg (DSI_HOST+) | Value | Meaning |
|---|---|---|
| `0xA4` PHY_IF_CFG | RMW `n_lanes[1:0]=1` | 2 data lanes |
| `0x04` PWR_UP | `1` | host out of shutdown |
| `0xA0` PHY_RSTZ | `0x1` → `0x1` (rstz=0) → `0x3` → `0x7` → `0xF` | shutdownz, rstz pulse 0→1, enableclk, forcepll |

### Step 4. D-PHY PLL for 1000 Mbps from a 20 MHz reference (mipi_dsi_hal.c:39-104, mipi_dsi_periph.c:9-49)
- N=1, M=50 (even, exact), hsfreqrange for [1000,1050) = `0x2A`.
- Each test-interface write `phy_write(addr,val)` is this sequence (phy_ll.h:182-209):
  `TST_CTRL0(0xB4)=0; TST_CTRL1(0xB8)=0x10000|addr; TST_CTRL0=2; TST_CTRL0=0; TST_CTRL1=val; TST_CTRL0=2; TST_CTRL0=0`
- Writes, in order: `(0x44,0x54)`, `(0x19,0x30)`, `(0x17,0x00)`, `(0x18,0x11)`, `(0x18,0x81)`.
- Poll `0xB0` PHY_STATUS bit0 `phy_lock`, then `(PHY_STATUS & 0x94) == 0x94` (clock lane + lane0 + lane1 in stop state; phy_ll.h:87-103).

### Step 5. Host link parameters (esp_lcd_mipi_dsi_bus.c:100-122)
| Reg | Value |
|---|---|
| `0x34` MODE_CFG | `1` (command mode) |
| `0x94` LPCLK_CTRL | `0` (clock lane LP) |
| `0x9C` PHY_TMR_CFG | `0x0032_0068` (hs2lp=50, lp2hs=104) |
| `0x98` PHY_TMR_LPCLK_CFG | `0x002E_0080` (clk hs2lp=46, lp2hs=128) |
| `0x2C` PCKHDL_CFG | `0x19` (eotp_tx_en, ecc_rx_en, crc_rx_en) |
| `0x08` CLKMGR_CFG | `0x0C06` (to_clk_div=125/10=12, tx_esc_div=125/18=6) |
| `0x78,0x7C,0x80,0x84,0x88,0x8C` timeouts | `0` |
| `0xF4` PHY_TMR_RD_CFG | `6000` (`0x1770`) |
| `0xA4` PHY_IF_CFG | RMW `phy_stop_wait_time[15:8]=0x3F`, giving `0x3F01` |

### Step 6. Command-mode (DBI) config (esp_lcd_panel_io_dbi.c:42-57)
- `0x68` CMD_MODE_CFG = **`0x010F_7F02`**. This sets ack_rqst_en, all gen/dcs short/long writes and reads to LP, and max_rd_pkt_size LP.

### Step 7. DPI clock + DW-GDMA channel + DPI/bridge config (esp_lcd_panel_dpi.c:252-331)
DPI clock (clk_tree_defs.h:427; mipi_dsi_hal.c:253-258):
- `CLKRST+0x28` REF_CLK_CTRL1 bit30 `ref_240m_clk_en`=1 (default 1). PLL_F240M = SPLL 480 / (REF_CLK_CTRL0[23:16]=1, +1).
- `CLKRST+0x3C` PERI_CLK_CTRL03: `dpiclk_src_sel[6:5]=1` (PLL_F240M), `dpiclk_div_num[15:8]=3` (÷4, so **60 MHz**), `dpiclk_en[7]=1`.

DW-GDMA group and channel 0 (dw_gdma.c:104-125, 223-291; dw_gdma_hal.c:12-18; dw_gdma_ll.h:90-109, 625-806):
| Register | Value |
|---|---|
| `CLKRST+0x14` SOC_CLK_CTRL0 | set bit13 `gdma_cpu_clk_en` |
| `CLKRST+0x18` SOC_CLK_CTRL1 | set bit5 `gdma_sys_clk_en` |
| `CLKRST+0xC0` HP_RST_EN0 | bit21 `rst_en_gdma` 1 → 0 |
| `DMAC+0x58` RESET0 | `1`, poll until 0 |
| `DMAC+0x10` CFG0 | `0x3` (dmac_en, int_en) |
| `DMAC+0x120` CH1_CFG0 | RMW `src_multblk_type[1:0]=3`, `dst_multblk_type[3:2]=3` (linked list) |
| `DMAC+0x124` CH1_CFG1 | RMW: tt_fc[2:0]=1 (M2P, DMAC flow ctrl), hs_sel_src[3]=0, hs_sel_dst[4]=0 (HW), dst_per[13:12]=0 (DSI), ch_prior[19:17]=1, src_osr_lmt[26:23]=4, dst_osr_lmt[30:27]=1. Field value `0x0A02_0001` |
| `DMAC+0x180` CH1_INTSTATUS_ENABLE0 | `0xFFFF_FFFF` |
| `DMAC+0x190` CH1_INTSIGNAL_ENABLE0 | bit1 `dma_tfr_done` (dw_gdma.c:687) |

("CH1" in the register names is IDF channel index 0. Channel n lives at `DMAC+0x100*(n+1)`.)

DSI host DPI side (mipi_dsi_host_ll.h:173-403):
| Reg | Value |
|---|---|
| `0x0C` DPI_VCID | `0` |
| `0x10` DPI_COLOR_CODING | `0` (16-bit config 1 = RGB565) |
| `0x14` DPI_CFG_POL | `0` |
| `0x38` VID_MODE_CFG | **`0x0000_FF02`**: vid_mode_type=2 (burst), lp_vsa/vbp/vfp/vact/hbp/hfp=1, frame_bta_ack_en=1, lp_cmd_en=1 |
| `0x3C` VID_PKT_SIZE | `1024` |
| `0x40` VID_NUM_CHUNKS / `0x44` VID_NULL_SIZE | `0` / `0` |
| `0x48` VID_HSA_TIME | `20` |
| `0x4C` VID_HBP_TIME | `333` |
| `0x50` VID_HLINE_TIME | `2819` (=2133+20+333+333) |
| `0x54` VSA / `0x58` VBP / `0x60` VACTIVE / `0x5C` VFP | `1` / `23` / `600` / `12` |

Host horizontal values are in lane-byte clocks: `trunc(px * 1000/60/8)` with a float ratio of 2.08333 (mipi_dsi_hal.c:236-245). esp-hal rounds instead (dpi.rs). Copy the IDF numbers.

DSI bridge (mipi_dsi_brg_ll.h:98-316, struct offsets from mipi_dsi_bridge_struct.h):
| Reg (DSI_BRG+) | Value |
|---|---|
| `0x18` PIXEL_TYPE | `0x2` (raw_type=2 RGB565, dpi_config=0, data_in_type=0 RGB) |
| `0x38` DPI_H_CFG0 | `0x0400_054A` (hdisp=1024, htotal=1354) |
| `0x3C` DPI_H_CFG1 | `0x000A_00A0` (hsync=10, hbank=160) |
| `0x30` DPI_V_CFG0 | `0x0258_027C` (vdisp=600, vtotal=636) |
| `0x34` DPI_V_CFG1 | `0x0001_0017` (vsync=1, vbank=23) |
| `0x0C` RAW_NUM_CFG | `0x8002_5800` (raw_num_total = 1024·600·16/64 = 153600, raw_num_total_set) |
| `0x40` DPI_MISC_CONFIG | `0x4000` (fifo_underrun_discard_vcnt[15:4]=1024; dpi_en later) |
| `0x88` DMA_FLOW_CTRL | `0x10` (flow controller = DMA, multiblk_num=1) |
| `0x6C` DMA_FRAME_INTERVAL | RMW `dma_multiblk_en[28]=0` |
| `0x08` DMA_REQ_CFG | `256` |
| `0x8C` RAW_BUF_ALMOST_EMPTY_THRD | `768` |
| `0x04` EN | `1` |
| `0x44` DPI_CONFIG_UPDATE | `1` |

### Step 8. Panel reset (esp_lcd_ek79007.c:215-231)
GPIO27 low for 10 ms, then high, then wait 20 ms. (`reset_active_high` is not set by the BSP, so the reset level is 0.)

### Step 9. EK79007 init over DCS, LP mode (esp_lcd_ek79007.c:126-188; mipi_dsi_hal.c:106-161)
For each command, poll `0x74` CMD_PKT_STATUS bit1 `gen_cmd_full`==0, then write `0x6C` GEN_HDR = `(param<<16)|(cmd<<8)|(vc<<6)|DT`. All table entries are short packets, so the payload FIFO (`0x70`) is never used. See §2 for the exact words.
- Recommended bare-metal addition (IDF does not do this): before each delay and before switching to video mode, poll CMD_PKT_STATUS bit0 `gen_cmd_empty` and bit2 `gen_pld_w_empty`, so the 120 ms starts after 0x11 has actually been sent.

### Step 10. Start the stream (esp_lcd_panel_dpi.c:404-458; dw_gdma.c:381-458, 534-574)
LLI: 64 bytes, 64-byte aligned, in **internal L2MEM**. IDF writes it through the non-cacheable alias (addr + `0x4000_0000`) and puts the **cached** address in LLP (dw_gdma.c:44-45, 406-416).

| LLI offset | Value |
|---|---|
| +0x00 SAR lo | framebuffer address (PSRAM, as seen by the CPU) |
| +0x04 SAR hi | 0 |
| +0x08 DAR lo | `0x5010_5000` |
| +0x0C DAR hi | 0 |
| +0x10 BLOCK_TS | `153599` (`0x257FF`) = 1 228 800·8/64 − 1 |
| +0x18 LLP lo | `0x0000_0001` (next = 0, lms=1 memory port; single LLI, "singly" list) |
| +0x1C LLP hi | 0 |
| +0x20 CTL lo | **`0x001E_1B41`**: sms=1 (mem), dms=0 (DSI), sinc=0, dinc=1 (fixed), src/dst width=64-bit, src_msize=512, dst_msize=256 |
| +0x24 CTL hi | **`0xC010_8840`**: arlen_en, arlen=16, awlen_en, awlen=16, ioc_blktfr=0, last=1, valid=1 |
| others | 0 |

Then:
1. `DMAC+0x128` CH1_LLP0 = `(lli_cached_addr & ~0x3F) | 1`, and `DMAC+0x12C` = 0.
2. `DMAC+0x18` CHEN0 = `0x101` (ch_en + write-enable for channel index 0).
3. DSI_HOST `0x34` MODE_CFG = `0` (video mode).
4. DSI_HOST `0x94` LPCLK_CTRL = `0x3` (auto clock lane + request HS).
5. DSI_BRG `0x40` DPI_MISC_CONFIG |= 1 (`0x4001`), then `0x44` = `1`.
6. DSI_BRG `0x50` INT_ENA |= 1 (underrun flag, checked in the ISR).

### Step 11. Per-frame ISR (DW-GDMA source **24**, `ETS_DW_GDMA_INTR_SOURCE`; esp_lcd_panel_dpi.c:79-113)
1. `st = DMAC+0x188`, then write `DMAC+0x198 = st` (clear).
2. `e = BRG+0x5C`, then write `BRG+0x54 = e`. Bit0 means underrun, which indicates PSRAM bandwidth trouble.
3. Re-arm: write LLI CTL hi = `0xC010_8840` again. The DMA clears `valid` in the LLI after it consumes it (esp-hal vdma.rs:140-147). Rewrite CH1_LLP0, then write CHEN0 = `0x101`.
4. Double buffering means changing the LLI SAR here.
- Alternative (esp-hal, dpi.rs:34, vdma.rs:91-147): a ring of 2 LLIs without `last`, re-armed in the block-done ISR. Either way an ISR (or a tight poll) runs once per frame.

### Step 12. Backlight on
Drive GPIO26 high. BSP "100 %" is LEDC duty 1023/1024 at 5 kHz, which is effectively high (BSP .c:511-534).

**Recommended first milestone (no PSRAM or DMA needed):** after step 9, set DSI_BRG `0x40` dpi_en=0 and write `0x44`=1. Then set VID_MODE_CFG bit16 `vpg_en` (vertical colour bars), MODE_CFG=0 and LPCLK_CTRL=3 (esp_lcd_panel_dpi.c:579-602; host_ll.h:303-328). This tests the LDO, PHY, DCS and panel on their own.

### Framebuffer requirements
- 1024×600×2 = 1 228 800 B, in PSRAM, aligned to at least 64 B (128 B is safer). IDF aligns to the external-memory cache line (dpi.c:219-235).
- After the CPU writes, call ROM `Cache_WriteBack_Addr(CACHE_MAP_L1_DCACHE /*BIT4*/, addr, size)` and then `Cache_WriteBack_Addr(CACHE_MAP_L2_CACHE /*BIT5*/, addr, size)`. The ROM entry is `0x4FC0_03F4` (esp32p4.rom.ld:190; cache_ll.h:644-693). Without this the DMA reads stale PSRAM.

## 2. EK79007 init table (verbatim, esp_lcd_ek79007.c:114-124, plus the lane command at :146-148)

| # | cmd | data | delay | DSI DT | GEN_HDR word |
|---|---|---|---|---|---|
| 0 | `0xB2` (pad control) | `0x10` (2-lane; `0x00` = 4-lane) | 0 | 0x15 | `0x0010_B215` |
| 1 | `0x80` | `0x8B` | 0 | 0x15 | `0x008B_8015` |
| 2 | `0x81` | `0x78` | 0 | 0x15 | `0x0078_8115` |
| 3 | `0x82` | `0x84` | 0 | 0x15 | `0x0084_8215` |
| 4 | `0x83` | `0x88` | 0 | 0x15 | `0x0088_8315` |
| 5 | `0x84` | `0xA8` | 0 | 0x15 | `0x00A8_8415` |
| 6 | `0x85` | `0xE3` | 0 | 0x15 | `0x00E3_8515` |
| 7 | `0x86` | `0x88` | 0 | 0x15 | `0x0088_8615` |
| 8 | `0x11` (sleep out) | none (`data_bytes=0`) | **120 ms** | 0x05 | `0x0000_1105` |

There is no `0x29` (display on) for EK79007 in this path. The BSP only sends it for ILI9881C (BSP .c:671). The BSP leaves `lane_num` at 0, which the driver treats as 2 lanes (`0xB2 0x10`). MADCTL default is `0x01` (bit0 SHLR, bit1 UPDN), used only for mirroring.

## 3. Panel timings (esp_lcd_ek79007.h:126-143; BSP display.h:44-58)

| Parameter | Value |
|---|---|
| Active | 1024 × 600 |
| HSYNC / HBP / HFP | 10 / 160 / 160 → htotal 1354 |
| VSYNC / VBP / VFP | 1 / 23 / 12 → vtotal 636 |
| DPI clock requested | 52 MHz (about 60.4 Hz) |
| DPI clock actual | 240 / floor(240/52)=4 → **60 MHz → 69.67 Hz** |
| Lanes / rate | 2 × 1000 Mbps (actual 1000, PLL exact) |
| Colour | RGB565 in and out, RGB element order, little-endian pixels |
| Polarities | all active-high (DPI_CFG_POL=0) |

To get a true 52 MHz you would need PLL_F160M (÷3 = 53.3) or APLL. IDF does not do this and the demo worked at 60 MHz, so keep 60 MHz.

## 4. GPIOs

| Signal | GPIO | Polarity | Source |
|---|---|---|---|
| Backlight PWM | **26** | active-high (duty 0 = off) | esp32_p4_function_ev_board.h:89-93; Espressif EV-board user guide (DuPont wire from adapter J6 PWM to J1 GPIO26) |
| LCD reset | **27** | active-low | same; esp_lcd_ek79007.c:69-82, 215-231 |
| (non-EK79007 builds) backlight | 23 | | header :94-98 |

About the "GPIO 23 is not usable" warning: it comes from LEDC `_ledc_set_pin` when the pin is already reserved. `bsp_display_brightness_init()` is called twice, at BSP .c:571 and again from `bsp_display_start` at :1030. The warning therefore means only that your demo build had its backlight macro set to 23. That is the non-1024×600 branch, or an older BSP (^1/^2 used 23). It does not prove the wiring. Every BSP revision since `1f0872c2` (2024-07-30, "HW v1.4 + EK79007") maps the EK79007 panel to 26/27. **Check which J1 pin the adapter's PWM jumper is on before you rely on 26.**

GPIO26 and GPIO27 are the USB-OTG 1.1 internal PHY pins (io_mux_reg.h:184-193). Before using them as GPIO, make sure `USB_WRAP+0x0` OTG_CONF bit18 `usb_pad_enable` = 0 (gpio_ll.h:605-616; default 0).

Simple GPIO output, shown for n = 26 and 27:
1. IO_MUX pad `0x500E_106C` (GPIO26) / `0x500E_1070` (GPIO27): RMW `mcu_sel[14:12] = 1` (PIN_FUNC_GPIO). Optionally set `fun_drv[11:10]` (default 2).
2. `GPIO+0x558+4n` FUNCn_OUT_SEL_CFG = `0x100` (out_sel=256 is the GPIO_OUT register, oen_sel=0, no invert). This gives `0x500E_05C0` for 26 and `0x500E_05C4` for 27.
3. `GPIO+0x24` ENABLE_W1TS = `1<<n`.
4. Drive the pin with `GPIO+0x08` OUT_W1TS / `GPIO+0x0C` OUT_W1TC = `1<<n`.

## 5. Citations (paths relative to the repo roots under `~/ref`)

- **BSP** (`esp-bsp/bsp/esp32_p4_function_ev_board/`)
  - `include/bsp/display.h:32-58`: format, lanes, 1000 Mbps, LDO chan 3 / 2500 mV.
  - `include/bsp/esp32_p4_function_ev_board.h:89-98`: pins.
  - `esp32_p4_function_ev_board.c:474-547, 565-633, 1030`: LEDC, LDO, bus, DBI, EK79007, reset/init order.
- **EK79007** (`esp-iot-solution/components/display/lcd/esp_lcd_ek79007/`)
  - `esp_lcd_ek79007.c:22-28, 114-124, 126-188, 205-231`
  - `include/esp_lcd_ek79007.h:47, 126-143`
- **IDF esp_lcd** (`esp-idf/components/esp_lcd/dsi/`)
  - `esp_lcd_mipi_dsi_bus.c:17-122`
  - `esp_lcd_panel_io_dbi.c:42-84`
  - `esp_lcd_panel_dpi.c:79-158, 218-331, 404-458, 579-602`
  - `mipi_dsi_priv.h:32-33`
- **IDF HAL** (`esp-idf/components/hal/`)
  - `mipi_dsi_hal.c:15-28, 39-161, 230-258`
  - `esp32p4/include/hal/mipi_dsi_ll.h:30-170`, `mipi_dsi_host_ll.h:28-61, 69-768`, `mipi_dsi_phy_ll.h:29-241`, `mipi_dsi_brg_ll.h:16-316`
  - `dw_gdma_ll.h:18-29, 90-159, 236-304, 583-918, 947-1228`
  - `ldo_ll.h:23-280`
  - `gpio_ll.h:605-616`
  - `cache_ll.h:27, 644-693`
  - `dw_gdma_hal.c:12-18`
- **IDF soc** (`esp-idf/components/soc/esp32p4/`)
  - `register/soc/reg_base.h:9-169`
  - `mipi_dsi_host_reg.h` (offsets list) and `_struct.h`
  - `mipi_dsi_bridge_reg.h:14-36, 538-549` and `_struct.h`
  - `hp_sys_clkrst_reg.h:147-1374, 3064-3288`
  - `dw_gdma_reg.h:41-2375`
  - `pmu_reg.h:4134-4335`, `pmu_struct.h:702-733, 864`
  - `efuse_reg.h:746-893`
  - `io_mux_reg.h:184-193, 203-329`, `gpio_reg.h:29-125, 9764-10736`
  - `include/soc/clk_tree_defs.h:426-443`, `include/soc/interrupts.h:18-43`
  - `mipi_dsi_periph.c:9-49`
  - `ld/esp32p4.peripherals.ld:99-104`
- **IDF hw_support** (`esp-idf/components/esp_hw_support/`)
  - `ldo/esp_ldo_regulator.c:44-100`
  - `dma/dw_gdma.c:42-49, 104-125, 223-291, 381-458, 534-574, 625-690`
  - `port/esp32p4/esp_clk_tree.c:92-119`
- **ROM** (`esp-idf/components/esp_rom/esp32p4/`): `ld/esp32p4.rom.ld:186-191`, `include/esp32p4/rom/cache.h:230-231, 630`.
- **esp-hal** (`esp-hal/esp-hal/src/mipi_dsi/`, independent cross-check): `mod.rs:158-357`, `dpi.rs:25-45, 240-460`, `vdma.rs:24-200`.
- **Board doc:** docs.espressif.com/projects/esp-dev-kits/en/latest/esp32p4/esp32-p4-function-ev-board/user_guide.html (RST_LCD → GPIO27, PWM → GPIO26).

Source weighting: IDF v5.5 register headers and LL are authoritative for silicon v1.0, because v5.5 ships one register set for P4 with no `hw_ver3` split. The BSP and EK79007 component are the vendor's board config. esp-hal agrees on the PLL, LDO, LLI and bridge values, but it targets newer PAC metadata. For example it pulses a bridge `dsi_brig_rst` bit that does not exist in the v5.5 v1.0 struct, and it uses XTAL as the default PHY reference. Where they differ, follow IDF.

## 6. Open risks

1. **PSRAM is a hard prerequisite.** The DW-GDMA reads the framebuffer through the MSPI/MMU mapping. If PSRAM is not initialised and timing-tuned by the kernel, step 10 will show garbage or underruns. Mitigation: do the colour-bar milestone first.
2. **The per-frame ISR is mandatory.** You must route source 24 through the interrupt matrix to a CLIC line (kernel `irq.rs`). If the re-arm is late, the bridge underruns and the panel goes blue; IDF calls this unrecoverable (dpi.c:90-95).
3. **Cache coherency.** CPU writes to the framebuffer need an L1+L2 write-back. LLI writes need either the non-cacheable alias or a write-back; IDF uses the alias.
4. **Backlight pin ambiguity.** Your log implies GPIO23 was configured, while the docs and BSP say 26. It depends on the physical jumper.
5. **LDO trim.** The 9/6 values are uncalibrated. Port the eFuse trim math, or measure VDD_MIPI_DPHY. Over-voltage is bounded (2.7 V recommended max) but untested here.
6. **Timing quirk inherited from IDF.** The DPI clock is 60 MHz, not 52 MHz, and the host line time (2819 byte clocks) is slightly shorter than the DPI line (2820.8). This works in the shipped demo, but any change to the lane rate or DPI clock changes these numbers. Recompute them with the same truncating formula.
7. **No completion handshake on DCS writes in IDF.** Commands are enqueued in LP mode and the delays start from enqueue. Waiting for FIFO-empty in bare metal is safer and costs nothing.
8. **Not covered:**
   - PSRAM/MSPI init.
   - Interrupt-matrix register details (already in the kernel).
   - LEDC PWM dimming (plain GPIO is enough).
   - AXI-ICM QoS tuning (IDF does not set it by default).
   - DMA2D/PPA.
   - Touch (GT911).
   - Reading the panel ID. Your "FF FF FF" shows that DCS reads are not a reliable presence check on this panel.

Status: DONE_WITH_CONCERNS
Summary: Full register-level DSI/DPI bring-up for EK79007 (LDO, clocks, PHY PLL, host, bridge, DW-GDMA LLI, DCS table, GPIOs) extracted from IDF v5.5 + BSP + EK79007 driver and cross-checked against esp-hal.
Concerns/Blockers: Requires kernel PSRAM init and a per-frame DW-GDMA ISR (source 24); backlight GPIO is 26 per BSP/docs but your log suggests a 23-wired build, so verify the jumper.
