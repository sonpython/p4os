# Board identity and vendor BSP: Guition JC4880P443C (ESP32-P4 + C6, 4.3" 480x800 ST7701S)

Date: 2026-10-28. Scope: identify the board, extract vendor bring-up facts, rank causes of the black screen, and list core best practices for p4os.

## 1. Outcome

The board is a **Guition JC4880P443C_I_W** (module JC-ESP32P4-M3, ESP32-P4 + ESP32-C6, 4.3" IPS 480x800 ST7701S over 2-lane MIPI-DSI, GT911 touch, ES8311 codec). Confidence is very high.

It is **not** the JC8012P4A1C. That board is 10.1" 800x1280 with a JD9365 panel, LCD reset on GPIO9, and a 1500 Mbps lane rate. Do not port JC8012 display parameters.

The vendor's display recipe matches the registers we already reproduce (500 Mbps, 34 MHz requested and 30 MHz actual, 12/42/42 and 2/8/166 timing, the same init table, reset on GPIO5, backlight on GPIO23, DSI PHY powered from LDO3 at 2.5 V). The board has no hidden LCD power switch.

Because the static configuration already matches, the black screen comes from something a register dump cannot show. Section 5 gives a discriminating test.

## 2. Identity evidence

| # | Evidence | Source |
|---|----------|--------|
| 1 | The factory image embeds the build path `/home/jczn/esp/JC4880P443/demo/common_components/espressif__esp32_p4_function_ev_board/esp32_p4_function_ev_board.c` and `.../bsp_extra/src/bsp_board_extra.c`. "jczn" is Guition's vendor (Shenzhen Jingcai Intelligent). | `strings out/factory-app.bin` |
| 2 | The vendor JC4880P443 package's `esp_lcd_st7701_mipi.c` `vendor_specific_init_default[]` matches our extracted table byte for byte (FF 77 01 00 00 13 / EF 08 / ... / C0 63 00 / B0 80 09 53 0C ...). The "C0 3B 00" table in the image is the stock RGB-variant default from the component and is unused. | `~/ref/jc4880/wegi1_ESP32P4-JC4880P443C-I-W/1-Demo/idf_examples/ESP-IDF/common_components/espressif__esp_lcd_st7701/esp_lcd_st7701_mipi.c` |
| 3 | The vendor BSP is a fork of Espressif's `esp32_p4_function_ev_board`. That explains the "ESP32_P4_EV" tag, the "Install EK79007 LCD control panel" log line (the vendor reused the 1024x600 Kconfig branch but creates an ST7701), and the "ESP32-P4-Function-EV-Board" string. | same package, `espressif__esp32_p4_function_ev_board/esp32_p4_function_ev_board.c` L455-515 |
| 4 | The vendor schematic `JC4880P443_V1.0.pdf` shows GPIO5 to FPC pin 23 (panel RESX), LCD_PWM=GPIO23 driving MP3202 EN, I2C on GPIO7/8, PA_CTRL=GPIO11, and C6_CHIP_PU=GPIO54. | `~/ref/jc4880/wegi1_.../5-Schematic/JC4880P443_V1.0.pdf` |
| 5 | The JC8012 package the coordinator downloaded contains its own `xiaozhi-esp32-main/main/boards/guition-jc4880p443/config.h`. It lists the same pins (RST 5, BL 23, LDO3 2500 mV, 500 Mbps, 34 MHz, TOUCH_RST 22, TOUCH_INT 21). | `~/ref/guition/jc8012/JC8012P4A1C_I_W_Y/1-Demo/IDF_5.5.4/JC8012P4A1C_I_W_Y_Old_Panel/xiaozhi-esp32-main/main/boards/guition-jc4880p443/` |
| 6 | ESPHome's `mipi_dsi` preset "JC4880P443" uses the same table, 500 Mbps, 34 MHz, the same porches and reset_pin=5. | [esphome mipi_dsi models/guition.py](https://github.com/esphome/esphome/blob/dev/esphome/components/mipi_dsi/models/guition.py), [docs](https://next.esphome.io/components/display/mipi_dsi/) |
| 7 | Community board support packages agree: ultramcu's field notes and BSP, bigbag examples, and csvke's BSP. | [ultramcu/guition-jc4880p443c-i-w](https://github.com/ultramcu/guition-jc4880p443c-i-w), [ultramcu/guition-jc4880p4-bsp](https://github.com/ultramcu/guition-jc4880p4-bsp), [bigbag/JC4880P443C-examples](https://github.com/bigbag/JC4880P443C-examples) |

On credibility: items 1-5 are vendor or first-party artefacts. Item 6 is maintained upstream code. Item 7 is community code that has been tested on hardware. All seven sources agree on every display parameter.

## 3. Pin and power table

| Function | Pin / resource | Polarity and notes | Source |
|---|---|---|---|
| LCD reset (RESX) | **GPIO5** (FPC pin 23) | Active low. Vendor sequence: drive low 10 ms, then high and wait 120 ms. ultramcu uses 20/120. | schematic; vendor `panel_st7701_reset` |
| LCD backlight | **GPIO23** (LCD_PWM) to MP3202 boost EN | Active high with a 10K pull-down (off at reset). Vendor uses LEDC low-speed, timer 1, 20 kHz, 10-bit. A static high gives full current (~74 mA through the 3R9‖2R2 sense resistors). | schematic; BSP `bsp_display_brightness_init` |
| LCD logic power | ESP_3V3 (FPC pins 4/21/29) | **No GPIO-controlled switch.** TE (FPC pin 22) is not connected. | schematic |
| DSI PHY power | On-chip **LDO VO3 = 2500 mV** | Must be on before the DSI bus starts. | BSP `display.h` L66-67 |
| DSI | 2 lanes, **500 Mbps**, DPI requested at 34 MHz (30 MHz actual from 240 MHz / 8), RGB565, 480x800, H 12/42/42, V 2/8/166, burst video | The `esp_brookesia_phone` BSP source says 750 Mbps, but the shipped image ran at 500 (it matches the component macro and `esp_draw_bit`). | `esp_lcd_st7701.h` L154/177-195; JTAG dump |
| I2C (touch + codec) | SDA **GPIO7**, SCL **GPIO8** | Pulled up on the carrier board. Devices: GT911 at 0x5D (0x14 is the alternate address), ES8311 at 0x18. | BSP L46-47 |
| Touch GT911 | RST **GPIO22**, INT **GPIO21** | The vendor BSP leaves both as NC (it never resets the chip), and the factory firmware does not drive them. ultramcu claims RST is GPIO3, which conflicts with the schematic and the vendor xiaozhi config. | module pins 22/23; xiaozhi config.h |
| I2S (ES8311) | MCLK 13, BCLK 12, LRCK 10, DOUT 9, DIN 48 | ES8311 runs as an I2S slave with MCLK used. | BSP L50-54 |
| Speaker PA enable | **GPIO11** (PA_CTRL) | Active high (`BSP_POWER_AMP_IO`). | BSP L55; schematic |
| microSD | SDMMC slot 0 IOMUX: CLK 43, CMD 44, D0-D3 39-42 | **TF_VCC comes from on-chip LDO VO4 (3.3 V)** through an always-on P-FET. The GPIO45 enable is not populated. LDO4 must be enabled or CMD times out (0x107). | BSP L124-128; ultramcu notes §3 |
| C6 over ESP-Hosted SDIO | CLK 18, CMD 19, D0-D3 14-17, reset **GPIO54** (C6_CHIP_PU, active high, R81 10K pull-up) | These pins are internal to the module, so they are not on the carrier schematic. They match esp-hosted P4 defaults and the factory output-enable mask (14..19). | ultramcu pin table; module pin 74 |
| Boot / LED / UART0 | BOOT GPIO35, LED GPIO26, UART0 TX 37 / RX 38 | | ultramcu |
| Ethernet (unpopulated variant) | RMII 28-31/34/35/49/50, MDIO 52, PHY_RSTN 51 | | schematic net list |

## 4. Display bring-up must-dos (vendor order)

From `bsp_display_new_with_handles` plus `esp_lcd_st7701_mipi.c` plus IDF v5.5 `esp_lcd_panel_dpi.c`:

1. Configure the backlight LEDC with duty 0, so the backlight stays off.
2. Acquire LDO3 at 2500 mV for the DSI PHY.
3. Create the DSI bus: enable host and PHY power, run the PHY reset sequence, write PLL test registers 0x44 / 0x19 / 0x17 / 0x18, then wait for PLL lock and for all lanes to reach stop state. Put the host in command mode with LP commands.
4. Create the DPI panel: DPI clock, host video timing, bridge setup (`dsi_en`, `dpi_config_update`). Video is **not** started yet.
5. Hardware reset on GPIO5: low for 10 ms, then high, then wait 120 ms.
6. Read the ID with 0x04. The factory read returns FF FF FF, which is harmless. Then select page 0 (`FF 77 01 00 00 00`), send **MADCTL 0x36=0x00** and **COLMOD 0x3A=0x55**, then the 36-entry table, ending with `0x11 [00]` and a 120 ms wait, then `0x29 [00]` and a 20 ms wait. Both are DCS short writes with one parameter (data type 0x15).
7. Only now start video (`dpi_panel_init`): arm DW-GDMA with one LLI per frame, set `cmd_video_mode=0`, set LPCLK to AUTO (`auto_clklane_ctrl=1`, `txrequestclkhs=1`), set bridge `dpi_en=1`, then `dpi_config_update=1`. Enable the bridge underrun interrupt.
8. Re-arm the DMA in each transfer-done interrupt.
9. Write back the framebuffer from cache to PSRAM after every CPU write (`esp_cache_msync` C2M).
10. Set backlight brightness last (`bsp_display_backlight_on`).

Our `kernel/src/display.rs` follows steps 2-9 in this order, as of my reading at the start of this session. During the research it was briefly switched to 1500 Mbps; the coordinator reports it is now back to 500 Mbps.

### 4a. From panel init to first visible pixels (vendor details, by question)

- **Reset vs LDO and PHY order.** LDO3 comes on first, then the DSI bus (PHY PLL locked, lanes in stop state), then the DPI panel object, which includes the bridge configuration. **Only after all of that is RESX pulsed.**
  - `esp_lcd_new_panel_st7701_mipi` configures GPIO5 as an output without setting a level, so the pin drives the output register's reset value of 0. From that moment until `panel_reset()` the panel is therefore held in reset.
  - The reset is then a 10 ms low, a release to high, and a 120 ms wait. There is no extra high-low-high pre-pulse, but the pre-pulse the coordinator added is harmless.
  - Source: `esp_lcd_st7701_mipi.c` (the `gpio_config` in `esp_lcd_new_panel_st7701_mipi` and `panel_st7701_reset`) and BSP L459-513.
- **Delay between 0x11/0x29 and DPI start.** The sequence is `0x11 [00]`, a 120 ms wait, `0x29 [00]`, a 20 ms wait, and then `st7701->init(panel)`, which is IDF `dpi_panel_init`. That call immediately arms the DMA, enters video mode, sets the clock lane to AUTO, and sets `dpi_en` plus `config_update`.
  - So video starts about **20 ms after DISPON**, with no other delay.
  - The commented-out `disp_on_off` and `disp_sleep` calls in the BSP confirm that nothing is sent after video starts.
- **LP vs HS for commands.** All DCS and generic packets go in **LP** mode with **ACK requested**. During the command phase the clock lane is held in **LP** (IDF `esp_lcd_new_dsi_bus` L103 sets `CLOCK_LANE_STATE_LP`), and the switch to AUTO happens only at video start.
  - Source: IDF v5.5 `esp_lcd_panel_io_dbi.c` L42-57.
  - One-parameter commands use DCS short write 0x15. Zero-parameter commands use 0x05, but the vendor never sends any, because 0x11 and 0x29 both carry a 0x00 parameter.
  - IDF does not wait for each command to finish transmitting. It only waits while the FIFO is full. Our per-command wait for completion is stricter, which is fine.
  - A first-pixel trap: if the clock lane is left in continuous HS (`lpclk` = 1 instead of 0 during commands), some panels miss LP commands. Keep HOST+0x94 at 0 until video starts, then set it to 3.
- **Backlight.** Vendor: LEDC low-speed timer 1, **20 kHz**, 10-bit, duty 0 at init, then `bsp_display_backlight_on()` (100%) from `main.cpp` L52, after the panel and LVGL have started.
  - The MP3202 datasheet recommends **20 kHz to 1 MHz** for PWM on EN. Lower frequencies cause audible noise or flicker, and soft-start re-runs on every EN rising edge.
  - A static high is a valid 100% setting, and there is **no minimum-duty requirement for the panel to show pixels**. The backlight is purely the boost converter's LED current.
  - If you PWM it, use at least 20 kHz.
  - Source: [MP3202 datasheet (LCSC mirror)](https://atta.szlcsc.com/upload/public/pdf/source/20251223/3B85A73FF7B4A22D747736133FCEA4E3.pdf), [MPS product page](https://www.avnet.com/americas/product/mps/mp3202dj-lf-z/evolve-3277139/).
- **GT911 reset and address selection on GPIO22/21.** The vendor sets `rst_gpio_num` and `int_gpio_num` to NC, so **no address-select sequence and no hardware reset ever happen**. The driver talks to the default 0x5D (`ESP_LCD_TOUCH_IO_I2C_GT911_CONFIG`) and reads product ID 0x8140 and config version 0x8047 ("TouchPad_ID 0x39,0x31,0x31 Config_Version:250").
  - If p4os wants a deterministic address, follow the registry driver (`esp-bsp/components/lcd_touch/esp_lcd_touch_gt911/esp_lcd_touch_gt911.c` L108-143):
    1. Drive RST (GPIO22) low and INT (GPIO21) low, then wait 10 ms.
    2. Set INT to 0 for address 0x5D or 1 for 0x14, then wait 1 ms.
    3. Release RST high and wait 10 ms, then a further 50 ms.
    4. Switch INT back to a floating input.
  - Otherwise do not touch either pin, which is the vendor behaviour.
  - Touch does not depend on the display, and the vendor initialises it after the panel.

## 5. Black screen: likely causes, ranked

Context: register values match the factory dump, all init commands are acknowledged, and the backlight is visibly "dim". Command ACKs only prove the **LP escape-mode** path. Video travels in **HS** mode through a separate path (PHY HS timing and PLL, clock lane, bridge, DW-GDMA, PSRAM). A static dump also cannot show self-clearing trigger bits or the D-PHY test-interface registers.

1. **Lane rate drifted from what the panel accepts.** The coordinator reports this is now reverted to 500 Mbps. After reverting, confirm the PHY test writes are `0x44=0x0E, 0x17=0x01, 0x18=0x11, 0x18=0x81` (hsfreqrange 0x07, N=2, M=50), and that the HOST+0x98/0x9C HS timings are back to the factory values.

2. **The HS video path is not delivering, or delivers zeros.** To tell these apart, run `display::test_pattern(true)`, which uses the DSI host's VPG colour bars and bypasses the bridge, DMA and PSRAM.
   - If the bars appear, the panel and link are good, and the fault is in the DMA or framebuffer path. Check that `FRAMES` increments at about 53 Hz and `UNDERRUNS` stays at 0. Make sure the framebuffer is not still all zeros: `alloc_zeroed` produces RGB565 0x0000, which is black. Confirm `cache_writeback` really reaches PSRAM (L1 and L2 for the P4 external-memory cache), and check the DW-GDMA handshake and channel status (CH+0x30) for errors.
   - If the bars do not appear, the panel or HS link is at fault. Check the PHY HS timing registers (HOST+0x98/0x9C) for the lane rate in use, and confirm `phy_status` (HOST+0xB0) shows the clock lane in HS while in video mode.

3. **The panel is not really reset, because GPIO5 is not toggling at the pad.** The brief says GPIO5 is not driven, although the code calls `gpio::output(5, …)`. GPIO0-15 are LP-capable pads on the P4, and IDF's `gpio_config` calls `rtc_gpio_deinit` first (the "deinit rtc gpio failed" string is in the image). Verify these over JTAG after init:
   - `LP_IOMUX_PAD5_REG` (0x5012_B01C) bit 4 `mux_sel` must be 0 (digital).
   - `LP_IOMUX_LP_PAD_HOLD_REG` (0x5012_B04C) bit 5 must be 0 (no hold).
   - `GPIO_ENABLE` bit 5 must be 1, and `GPIO_IN` bit 5 must follow the output.

   Without a real reset, the ST7701 carries state over from the previous session or a JTAG reset. It still ACKs commands but can stay in a non-displaying state.

4. **GPIO23 is not at a solid high.** With MP3202 EN held high, the backlight should be *brighter* than the factory's 91% PWM, not dim. Check `GPIO_IN` bit 23 and `FUNC23_OUT_SEL`, which must be 0x100. The alternative is that "dim" is simply backlight glow through an IPS panel showing black. That would be harmless but fits causes 2 and 3.

5. **MADCTL/COLMOD are not sent before the table** (low likelihood). The vendor sends 0x36=0x00 and 0x3A=0x55 on page 0 before the vendor table. Leaving them out normally causes wrong colours or wrong orientation, not a black screen, but add them to match the vendor.

These are ruled out by evidence: a missing LCD power-enable GPIO (VCI is on fixed 3V3), wrong porches or table (four sources agree), PLL N/M (N=2, M=50, hsfreqrange 0x07 matches IDF `mipi_dsi_hal_configure_phy_pll` at 500 Mbps), and the order of video start versus 0x29 (the vendor also starts video after DISPON).

## 6. Core best practices (survey)

| Area | Mature reference | Practice to copy |
|---|---|---|
| Display pipeline | IDF v5.5 `esp_lcd/dsi/esp_lcd_panel_dpi.c`, `hal/mipi_dsi_hal.c` | Use one DW-GDMA LLI per frame and re-arm it in the done ISR, which also gives a vsync point. Use a 64-bit source width with burst 16 into the bridge FIFO at `MIPI_DSI_BRG_MEM_BASE`. Bridge settings: burst 256, empty threshold 768, `raw_num` = W·H·bpp/64, underrun discard = h_size. Check the underrun interrupt in the DMA ISR. Keep the framebuffer 64/128-byte aligned and do a C2M cache sync after drawing. Page-flip only at the DMA-done boundary. |
| Chip revision | IDF v5.5 vs master; [NuttX P4 docs](https://nuttx.apache.org/docs/latest/platforms/risc-v/esp32p4/boards/esp32p4-function-ev-board/index.html) | Our chip is **rev v1.0** (engineering-sample era). IDF master, IDF 6.x and NuttX target rev v3 by default, and several HP_SYS_CLKRST and LDO fields moved between revisions. Take register maps from IDF **v5.5** only, which is what `~/ref/esp-idf` is checked out at. |
| Power rails | Vendor BSP; ultramcu notes | LDO3 at 2.5 V powers the DSI PHY. LDO4 at 3.3 V powers TF_VCC and the GPIO39-48 IO domain. Bring up both early and never let a driver re-acquire them. |
| Touch | `esp_lcd_touch_gt911` | Poll over I2C at 0x5D, then fall back to 0x14. Read status register 0x814E, then points from 0x8150, then write 0 back to 0x814E. Do not toggle RST/INT unless you need a specific address, because the vendor leaves both untouched. |
| SD | IDF `sdmmc` plus `sd_pwr_ctrl_by_on_chip_ldo` | Use slot 0 IOMUX with 4-bit width. Enable LDO4 first. Initialise at 400 kHz, then move to 40 MHz (high speed). |
| Audio | `esp_codec_dev` ES8311 (esp-adf) | Run the ES8311 as an I2S slave with MCLK = 256·fs from GPIO13. Configure it over I2C at 0x18. Raise PA GPIO11 only after the codec is configured, to avoid a pop. |
| Wi-Fi | esp-hosted-mcu **v2.x** (matches the IDF 5.5-era strings) | Use 4-bit SDIO on pins 14-19 and reset the C6 with a GPIO54 pulse. The C6 firmware is the vendor's `JC-C6-slave_v2.3.2.bin`, so the host protocol must match v2.3.x. Note that the vendor stack retries Wi-Fi init by resetting the C6 over GPIO54. |

## 7. Limitations

- I did not run anything on hardware, as the brief required. Ranks 2-4 need the VPG test and the JTAG reads described in section 5 to settle.
- The C6 SDIO pins sit inside the JC-ESP32P4-M3 module and do not appear in any schematic I have. They rest on community data, esp-hosted defaults and the factory output-enable mask.
- I did not find the exact source of the factory "esp_brookesia_demo" project. Its build path matches the vendor demo tree, but the published `esp_brookesia_phone` copy says 750 Mbps, so it is not the identical build.
- I did not consult the ST7701S datasheet for its maximum lane rate. The case against 1500 Mbps rests on every working configuration using 500 Mbps.

## Unresolved questions

1. Does the VPG colour-bar test show bars on the panel? This single result decides between cause 2a and cause 2b.
2. What do `LP_IOMUX_PAD5_REG`, the hold register, and `GPIO_IN` bits 5 and 23 read after `display::init()`?
3. Why does the factory I2C scan see both 0x14 and 0x5D? One of them may be another device, such as an RTC footprint on SDA1/SCL1, rather than GT911.

Status: DONE_WITH_CONCERNS
Summary: The board is the Guition JC4880P443C (ST7701S 480x800). Vendor, schematic and ESPHome sources confirm our display parameters (GPIO5 reset, GPIO23 active-high boost EN, LDO3 2.5 V, 500 Mbps, 30 MHz, same init table), so the black screen is in the HS video path or the GPIO pad layer, and a VPG test separates the two. Concern: none of the ranked causes has been confirmed on hardware yet, and the C6 SDIO pins rest on community data rather than a schematic.
