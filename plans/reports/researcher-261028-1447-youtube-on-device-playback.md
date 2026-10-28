# Watching YouTube on the ESP32-P4 itself (no PC/Pi proxy)

Date: 2026-10-28. Scope: stream URL acquisition, decode feasibility, on-device alternatives and a phased plan for p4os on the Guition JC4880P443C (ESP32-P4 rev v1.0, 2x RV32 @360 MHz, 32 MiB PSRAM, ESP32-C6 Wi-Fi over SDIO). Empirical tests were run from the Raspberry Pi 5 on the same LAN, logged out, with no cookies, against four public videos. Only ranged reads (up to 2 MB) were made; no full video was downloaded.

## Tóm tắt (tiếng Việt)

1. **Có cách**, và không cần JS engine: client InnerTube `VISIONOS` (đang là client "JS-less" mặc định của yt-dlp 2026.10.26) trả về URL trực tiếp, không có `signatureCipher`, không có tham số `n`, và không cần PO token. Đã kiểm chứng ngày 2026-10-28: dùng HTTP Range đọc ở giữa và cuối file đều trả về 206 với tốc độ 30–200 Mbit/s.
2. Có hai điều kiện: phải gửi `visitorData` (lấy ở lần gọi đầu, gọi lại lần hai), và **phải dùng Range**. Request không có Range bị bóp còn khoảng 20–40 KB/s.
3. Kết quả các client khác: `ANDROID_VR` bị chặn sau khoảng 130–500 KB (từ 2026-08-17). `ANDROID` chỉ trả itag 18. `IOS` không có format nào. `WEB`, `MWEB` và các client `TV*` cần JS player.
4. Phát hiện quan trọng: **mọi luồng H.264 của YouTube đều là Main profile, CABAC, có B-frame và weighted prediction**, kể cả itag 18 (MIME khai là `avc1.42001E` nhưng không đúng). Vì vậy decoder Baseline (tinyH264 của Espressif) không dùng được.
5. Đã có decoder H.264 **thuần Rust, build được `no_std`** cho `riscv32imafc-unknown-none-elf`: `rusty_h264-decoder` và `rust_h264`. Cả hai giải mã **bit-exact** so với ffmpeg trên luồng YouTube 144p, 240p và 360p, kiểm tra trên Pi.
6. Ước tính trên P4: khoảng 20–30 nghìn chu kỳ cho mỗi macroblock. 144p chiếm 25–35% của một core, 240p@25 chiếm 55–85% của một core (khả thi), **360p không khả thi** khi chỉ dùng một luồng giải mã.
7. Âm thanh nên dùng Opus (itag 249/250, khoảng 50–65 kbps). `opus-rs` và `mousiki` build được `no_std` cho RV32. Chưa có decoder AAC `no_std` thuần Rust.
8. Băng thông cần dưới 0,5 Mbit/s, chỉ bằng khoảng 1/60 thông lượng ESP-Hosted SDIO. Nút cổ chai là CPU (giải mã, chuyển YUV sang RGB, TLS khi tải dồn dập), không phải mạng.
9. Đề xuất theo tầng: (1) chỉ âm thanh, (2) video 144p/240p với rusty_h264 và Opus, (3) 360p chỉ làm thử nghiệm. Storyboard JPEG 160x90 chỉ ra 1 khung hình mỗi 2–5 giây, nên chỉ dùng làm "slideshow" dự phòng.
10. Rủi ro chính: YouTube đổi client thường xuyên (vòng đời tính bằng tháng) và vi phạm ToS mục 1 và 3. Nên để bảng client trong file cấu hình trên thẻ SD và giữ Pi proxy làm phương án dự phòng.

## 1. Stream URL acquisition without a JS engine

### 1.1 Empirical results (Pi, 2026-10-28, logged out)

Client parameters (name, version, UA, client-name id) were taken from yt-dlp 2026.10.26 `extractor/youtube/_base.py`. A POST was sent to `https://www.youtube.com/youtubei/v1/player` with `visitorData`, and then ranged GETs were made on each URL.

| Client | Player result | Formats with direct `url` | `signatureCipher` / `n` | Mid-file ranged GET | Verdict |
|---|---|---|---|---|---|
| **VISIONOS** 1.02 | OK, but needs `visitorData` (without it: `LOGIN_REQUIRED` "Sign in to confirm you're not a bot") | 16–32: 160, 133, 134, 278, 242, 243, 394, 395, 139, 140, 249, 250, 251, …, plus an HLS manifest | none / none | **206** at 0, L/2 and L−300 KB, for every itag tested | **Works** |
| ANDROID 21.26.364 | OK | **only itag 18** (640x360 muxed) | none / none | **206** at all offsets | Works, but 360p only |
| ANDROID_VR 1.65.10 | OK, 11–29 formats | all itags, including 18 | none / none | 206 for the first ~64–130 KB, then **403** from about 500 KB on. itag 18 returns 403 at once | Broken (POT enforced) |
| IOS 21.26.4 | OK, but `streamingData` has 0 formats (HLS only for some videos) | 0 | n/a | n/a | Useless |
| MWEB, TV, TV_SAMSUNG, TV_SIMPLY | UNPLAYABLE "The page needs to be reloaded" | 0 | needs JS player (`signatureTimestamp`, sig + `n`) | n/a | Needs a JS engine |
| WEB, WEB_SAFARI, WEB_EMBEDDED | UNPLAYABLE / ERROR | 0 | needs JS, PO token, SABR | n/a | Needs a JS engine |

Other facts measured in the same runs:

- **Throttling is now by request shape, not by `n`.** Unranged GETs on VISIONOS adaptive URLs trickled at about 20–40 KB/s (196–330 KB in 8–10 s), which is *below* the 240p bitrate. Ranged GETs of 64 KB–2 MB ran at 25–200 Mbit/s from the Pi. **A player must use `Range:` requests.** yt-dlp does the same with `http_chunk_size`.
- **URLs are IP-bound** (`ip=` parameter) and expire after about 6 h (`expiresInSeconds` 21540). The device must make its own player request. It works over IPv4 (re-tested with IPv4 forced), which matches p4os's IPv4-only smoltcp.
- **visitorData bootstrap, without loading the 1 MB homepage.** The first player call returns `responseContext.visitorData` even when it says `LOGIN_REQUIRED`. Repeating the call with that value returned `OK` for all three videos tested.
- **Response size.** Player JSON is 66–190 KB and search JSON is 255 KB. Both are fine to parse in PSRAM.
- **Rate limits.** About 70 player calls over about 25 minutes from one IP caused no blocking once `visitorData` was sent. Sustained-use limits were not tested.
- **SABR is coming.** Every response includes `serverAbrStreamingUrl`. yt-dlp's code notes that newer ANDROID_VR versions are "SABR only", and the web client is already SABR-only.

### 1.2 How the tools handle it today

| Tool | Approach (Oct 2026) | Source and credibility |
|---|---|---|
| yt-dlp 2026.10.26 | Default clients are `tv_samsung, visionos, web` with a JS runtime (deno + yt-dlp-ejs for sig/`n`). With no JS runtime the only client is **`visionos`**. Logged in, it uses `web_embedded, tv_samsung, web`. `android_vr` was removed from the defaults on 2026-08-18 ([#17461](https://github.com/yt-dlp/yt-dlp/pull/17461): "ALL formats … are 403'd"). Maintainers state that JS-less extraction "won't be for long" ([#17836](https://github.com/yt-dlp/yt-dlp/issues/17836)). | Source code plus maintainer issues. High. The [PO Token wiki](https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide) lags the code (it still lists android_vr as POT-free). |
| NewPipe | Recent releases added a SABR workaround that "excludes made-for-kids videos". That is the same limitation as visionos/android_vr, so it probably uses a similar app-type client. | [Flathub release notes](https://flathub.org/it/apps/net.newpipe.NewPipe). Medium (inferred). |
| Invidious / Piped | Public instances are being IP-blocked, and Invidious now needs `invidious-companion` (Deno) to mint PO tokens. Its own docs say the instance list is short and recommend self-hosting. | [docs.invidious.io/instances](https://docs.invidious.io/instances/), [2026 overview](https://www.sumguy.com/invidious-piped-redlib-nitter-2026). Medium–high. |

### 1.3 Authenticated (OAuth TV) requests

These do not help. yt-dlp removed OAuth login after YouTube blocked it ("Login with OAuth is no longer supported"; [commit](https://gitea.smoothnet.org/nhyatt/yt-dlp/commit/b8635c1d4779da195e71aa281f73aaad702c935e)). Cookie-authenticated extraction uses `web_embedded/tv/web`, and all of those need the JS player. Logging in only matters for age-gated, members-only or made-for-kids content. It also puts the Google account at risk. Not tested, because the brief said no account.

### 1.4 Fragility and ToS (plainly)

- **Lifetime.** A JS-less client has lasted months, not years. ANDROID_VR was the go-to client until 2026-08-17. VISIONOS has replaced it for now, and maintainers expect even that to end. Plan for the client table to change several times a year.
- **ToS.** Fetching streams with a spoofed client breaks YouTube ToS "Permissions and Restrictions" item 1 (access/download except as permitted) and item 3 (automated means), and arguably item 2 (circumventing restrictions) ([ToS](https://www.youtube.com/static?template=terms&gl=GB)). For logged-out personal use the realistic consequence is bot-checks or IP throttling, not account action. Do not attach a Google account to this.

## 2. Decode feasibility on the P4

### 2.1 What the low itags really contain (verified with ffprobe and the `trace_headers` bitstream filter)

| itag | Declared | Actual | Notes |
|---|---|---|---|
| 18 | `avc1.42001E` (Baseline) + AAC-LC | **H.264 Main (profile_idc 77), CABAC, B-slices, weighted pred (explicit P, implicit B), 3 refs**, 640x360, plus AAC-LC 44.1 kHz | The declared codec string is wrong. Progressive MP4, `moov` at the front (98 KB). |
| 160 / 133 / 134 | `avc1.4D400C` / `4D4015` / `4D401E` | Main, CABAC, B, weighted, 3 refs at 256x144 / 426x240 / 640x360 | fMP4 (`sidx` + `moof`/`mdat`). Average bitrates about 77 / 160 / 315 kbps. |
| 394 / 395 | AV1 Main 8-bit | 144p / 240p, about 56 / 116 kbps | Lowest bitrate, but the heaviest to decode. |
| 278 / 242 | VP9 profile 0 | 144p / 240p | WebM. |
| 139 | `mp4a.40.5` | **HE-AAC** (SBR) | Avoid: SBR roughly doubles decode cost. |
| 140 | `mp4a.40.2` | AAC-LC 44.1 kHz, about 130 kbps | |
| 249 / 250 / 251 | Opus | Opus 48 kHz stereo, about 46 / 61 / 129 kbps, in WebM | Best audio choice. |

So **a Constrained-Baseline decoder cannot play any YouTube stream.** Espressif's esp_h264 software decoder (tinyH264, "constrained baseline profile only") is ruled out. The P4 also has **no** hardware H.264 decoder ([esp_h264 readme](https://components.espressif.com/components/espressif/esp_h264/versions/1.4.1/readme)).

### 2.2 Pure-Rust H.264 decoders (tested)

Both crates were downloaded and checked for real YouTube compatibility and `no_std` support.

| Crate | Features | `no_std` | Bit-exact vs ffmpeg on YouTube itag 160 / 133 / 134 | Pi 5 (A76 @2.4 GHz, 1 thread) | Licence / maturity |
|---|---|---|---|---|---|
| **rusty_h264-decoder 0.16.1** | Baseline, Main, most of High; CABAC; B; weighted; `forbid(unsafe)` | yes: `default-features=false, features=["libm"]`. **Builds for `riscv32imafc-unknown-none-elf`.** | **2127/2127, 1002/1002, 513/513 frames exact** | ~2,900–3,060 ns/MB (`asm` is x86-only, so on ARM this is scalar Rust) | BSD-2. Created 2026-06, 17.5k LoC, 30k downloads. Young, single vendor (Mata Network). |
| rust_h264 0.5.0 | Baseline/Main/High, CABAC, B, weighted, MBAFF | yes: `default-features=false`. **Builds for RV32.** | **all frames exact** | ~4,800–6,400 ns/MB (with NEON kernels) | MIT/Apache-2.0 (same as p4os). Created 2026-04, 25k LoC, 8 stars, single author. |

For reference, ffmpeg with one thread took about the same wall time as rusty_h264 on these clips, including process start.

**Estimate for the P4.** rusty_h264 runs at about 7,000 A76 cycles per macroblock. The P4 HP core manages about 3.5 CoreMark/MHz per core ([6.92 for two cores](https://www.cnx-software.com/2023/01/06/espressif-esp32-p4-a-400-mhz-general-purpose-dual-core-risc-v-microcontroller/)), against roughly 3x that per clock on an A76. That gives **about 20–30k cycles/MB on the P4**, with PSRAM reference fetches pushing toward the upper end.

Cross-check: tinyH264 on the P4 decodes 640x480@25 on one core = 30,000 MB/s, about 12k cycles/MB for the cheaper Baseline/CAVLC path. The same crate shows Main/CABAC to be about 1.4x Baseline cost (107 vs 150 Mpx/s), which gives about 17k cycles/MB before any Rust-vs-tuned-C gap. The two estimates agree.

| Video | MB/frame | MB/s @25 | MB/s @30 | P4 load at 20–30k cyc/MB (one core = 360 Mcyc/s) |
|---|---|---|---|---|
| 256x144 (itag 160) | 144 | 3,600 | 4,320 | 72–130 Mcyc/s, **20–36% of one core** |
| 426x240 (itag 133) | 405 | 10,125 | 12,150 | 200–365 Mcyc/s, **55–100% of one core** |
| 640x360 (itag 134/18) | 920 | 23,000 | 27,600 | 460–830 Mcyc/s, **more than one core**. The single-threaded decoder caps out around 12–18 fps. |

Frame-threaded decoding exists in both crates, but only behind `std`. Porting it to p4os threads is possible, but B-frame dependencies limit the gain to about 1.3–1.6x. Espressif's dual-task tinyH264 gained only 1.24x.

### 2.3 Other codecs, audio and containers

- **AV1/VP9: no.** AV1 needs roughly 2–3x the H.264 decode work at equal resolution, and no `no_std` pure-Rust AV1 or VP9 decoder exists (rav1d is a std/threads transpile of dav1d). The bitrate saving (56 vs 77 kbps at 144p) is irrelevant on this link.
- **Opus.** [`opus-rs`](https://docs.rs/crate/opus-rs/latest) 0.1.40 (`no_std`, no-alloc, port of libopus 1.6, 215k downloads) and [`mousiki`](https://docs.rs/crate/mousiki/0.2.0) 0.2.1 both **build for RV32 `no_std`** (opus-rs needs `features=["libm"]`). Decoding YouTube Opus was not verified on host, and P4 cost was not measured. Expect about 15–30 Mcyc/s for 48 kHz stereo CELT (estimate).
- **AAC.** Symphonia's AAC is pure Rust but std-bound ([no_std limited](https://www.mintlify.com/pdeljanov/Symphonia/resources/faq)). Prefer Opus, which every tested video had (249/250/251).
- **Containers.**
  - fMP4 (itags 160/133/134): a `sidx`/`moof`/`trun` parser is about 400–600 LoC to write.
  - WebM/EBML for Opus: about 300–500 LoC, using the `Cues` at `indexRange` for seeking.
  - Progressive MP4 for itag 18: the full `stbl` tables, about 600 LoC.
  - The [`mpeg-ts`](https://docs.rs/crate/mpeg-ts/0.3.0) crate (`no_std`+alloc) would cover the HLS-TS fallback.
  - Writing these in-tree is simpler than adopting std crates.

### 2.4 Write vs adopt vs port C (project rule: entirely Rust)

| Option | Effort | Risk | Rule fit |
|---|---|---|---|
| **Vendor rusty_h264-decoder (pin a version)** | Days to integrate (alloc in PSRAM, feed Annex-B from the fMP4 demuxer), plus 1–2 weeks to profile and tune on the P4 | Crate is 4 months old, so pin and vendor it. Bit-exactness already shown on YouTube streams. | Pure Rust, `forbid(unsafe)` |
| Vendor rust_h264 (fallback) | Same | Slower on host, larger | Pure Rust, licence identical to p4os |
| Write your own Main-profile progressive decoder (CABAC, P/B, weighted pred, deblock, DPB/POC; no interlace, no 8x8) | About 10–15k LoC; about 4–8 months for one experienced developer to reach conformance | High (CABAC and B-direct are subtle) | Pure Rust |
| Port C (openh264 / ffmpeg h264 via FFI) | Weeks | Medium | **Violates the all-Rust rule.** No benefit now that Rust decoders exist. |

## 3. On-device alternatives

| Alternative | What was measured | Assessment |
|---|---|---|
| **Audio-only** (Opus 249/250 over ranged GETs) | 249 is about 46 kbps; ranges at 15–200 Mbit/s | Cheapest and most robust tier. Uses about 5–10% of a core. |
| **Storyboard slideshow plus audio** | Spec in the player JSON: L2 = **160x90**, 5x5 sprite JPEG (800x450, about 40 KB), **one frame every 2 s** (3.5-min video) or **5 s** (10.5-min video). L0/L1 are 48x27 and 80x45. | A "podcast with pictures" mode, not video. It costs almost nothing: zune-jpeg already decodes these in software, and HW JPEG is not needed. Good as a degraded mode or seek-preview. |
| HLS from VISIONOS (`hlsManifestUrl`) | Variants include avc1.4D400C 144p; segments are **MPEG-TS** (`0x47` sync), fetched with 200. Audio is a separate group. | A fallback if adaptive URLs gain PO-token enforcement. The PO Token Guide says HLS is generally not POT-gated. Needs a TS demuxer. |
| Invidious/Piped public instances | Not used, because instance availability in 2026 is poor (see 1.2) | Not reliable. A self-hosted instance is just a proxy. |
| MJPEG through the HW JPEG decoder | YouTube serves no MJPEG | Only possible through a proxy. p4os also has no HW JPEG driver yet: `media/` uses zune-jpeg. |
| SABR (UMP/protobuf streaming) | `serverAbrStreamingUrl` is present everywhere | The likely future-proof path, but a large protocol to reimplement (googlevideo/yt-dlp work is still experimental). Not now. |

## 4. Recommendation

**Build Tier 1 (audio-only) and Tier 2 (240p H.264 + Opus) on-device, using VISIONOS with ranged GETs and a vendored rusty_h264-decoder. Treat 360p as an experiment and storyboards as a degraded mode. Keep a Pi-proxy fallback for when the client is blocked.**

Ranking:

1. VISIONOS + itag 133/160 + 249/250 + rusty_h264, on-device.
2. The same, with rust_h264 as the decoder.
3. Storyboard + Opus mode.
4. Pi proxy.
5. Writing your own decoder or porting C. Both are rejected.

### 4.1 Budgets (Tier 2 at 240p@25, the design point)

| Resource | Use | Headroom |
|---|---|---|
| CPU, core 1 | H.264 decode 200–300 Mcyc/s | 15–45% left |
| CPU, core 0 | Opus 15–30, demux/HTTP about 5, TLS (software AES-GCM) about 5 at steady rate, YUV420 to RGB565 with scale to 480x270 portrait about 40 (about 110 for 800x450 landscape in software, about 0 with PPA), GUI/touch | OK in portrait. Landscape fullscreen wants a PPA driver. |
| Network | 160 kbps video + 46–61 kbps audio, about 0.25 Mbit/s average and about 0.35 peak | ESP-Hosted SDIO reaches about 30 Mbit/s TCP on P4+C6 ([esp-hosted-mcu #112](https://github.com/espressif/esp-hosted-mcu/issues/112)), so more than 80x headroom. **Pace the reads.** A 1 MB range arriving at full Wi-Fi speed could make software TLS consume a whole core for about 0.3 s. |
| PSRAM | Player JSON ≤ 200 KB; DPB plus output queue about 10 x 155 KB = 1.5 MB; two 1 MiB range buffers; 0.5 s PCM = 96 KB | About 4 MB of 32 MiB |
| HTTP | HTTP/1.1, `Range`, keep-alive. One TLS handshake per 1 MiB range (about 50 s of 240p) is acceptable. | `net/` currently sends HTTP/1.0 with `Connection: close`, so this needs extending. |

### 4.2 Phased plan

| Phase | Work | Acceptance criteria |
|---|---|---|
| 0. YouTube client in `net/` (host-testable in `sim`) | InnerTube player (VISIONOS, two-step `visitorData`) and search (WEB `youtubei/v1/search`); format picker; client table (name, version, UA, id) **loaded from a JSON file on SD** so a YouTube change means editing a file, not reflashing | Host test against 3 public videos: player `OK`, itags 133/160/249 present, mid-file ranged GET returns 206 |
| 1. Audio-only player | HTTP/1.1 Range + keep-alive; WebM/EBML demux with Cues seek; `opus-rs` (or mousiki) decode to ES8311 at 48 kHz; search list with `mqdefault.jpg` thumbnails | On the board (headphones or a speaker are needed: none is fitted): a 10-min video plays with 0 underruns; core load < 25%; seek < 3 s |
| 2. 144p/240p video | fMP4 demux (`sidx`/`moof`/`trun`); vendored rusty_h264 on a dedicated core-1 thread; audio-clock A/V sync with a late-frame drop policy; software YUV to RGB565 nearest-neighbour scale to 480x270 | 240p25 sustains ≥ 24 fps over 5 min (frame counter); A/V drift < 80 ms; CRC of the first 100 decoded frames on the board equals the host decode of the same bytes |
| 3. (optional) PPA plus 360p trial | PPA SRM driver for colour conversion, scaling and rotation to landscape 800x450; profile the hot loops (MC, IDCT, deblock) and try PIE SIMD or `unsafe` kernels in a fork | Landscape 240p with < 5% CPU for display; 360p ≥ 20 fps, otherwise drop 360p for good |
| 4. Resilience | Fallback chain: VISIONOS adaptive, then VISIONOS HLS-TS, then the storyboard + audio mode, then a "use proxy" prompt; surface `playabilityStatus.reason` in the UI | Forcing each failure shows the next mode within 5 s |

### 4.3 What a Pi-proxy fallback adds

The Pi would run yt-dlp with deno and a PO-token provider. Those absorb client churn, SABR, made-for-kids and age-gated videos (with cookies), and live streams. The Pi would then transcode with ffmpeg to whatever the P4 handles most cheaply: H.264 Baseline or MJPEG at 480x270, plus MP3 or Opus, sent over a simple HTTP stream. That makes the board a thin client with near-zero codec risk and any source quality. The cost is that the Pi must be on, plus the extra hop. Keep it as Tier 4, not the default.

### 4.4 Adoption risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| VISIONOS gains PO-token enforcement or goes SABR-only | High within 6–12 months | Video and audio stop | SD-configurable client table; HLS fallback; Pi proxy |
| Made-for-kids videos unavailable (a known VISIONOS limit) | Certain | Some videos fail | Show the reason in the UI; proxy |
| rusty_h264 is young (4 months, 16 minor versions) | Medium | Bugs or API churn | Pin, vendor, keep the frame-CRC test; rust_h264 as a drop-in alternative |
| P4 decode cost at the high end of the estimate | Medium | 240p drops to about 20 fps | Fall back to 144p automatically; Phase 3 kernel tuning |
| TLS handshake or bulk-decrypt cost on the P4 is unmeasured | Medium | Stalls at range boundaries | Prefetch the next range at 50% buffer; read-rate pacing |

## Limitations of this research

- No code ran on the P4. All P4 cycle figures are extrapolated from Pi 5 runs and Espressif's tinyH264 table, so treat them as ±50%.
- Opus decoding with the Rust crates was compile-checked for RV32 but not run on YouTube data. AAC was not evaluated beyond availability.
- Four videos from one residential IP were tested over about 25 minutes. Long-session rate limits, region locks, live streams, Shorts and age-gated content were not tested.
- Authenticated paths were not tested (no account, by instruction).
- p4os TLS throughput and handshake time on the device are unknown. ESP-Hosted figures come from third-party reports, not this board.

## Unresolved questions

1. How long will VISIONOS stay usable? Watch yt-dlp `_DEFAULT_JSLESS_CLIENTS` in `_base.py`/`_video.py`, which is the best early-warning signal.
2. What is the real rusty_h264 cycles/MB on the P4 HP core, and how badly do PSRAM reference fetches hurt? This needs a one-day board benchmark before committing to Phase 2.
3. Is a speaker or headphone output going to be fitted? Audio is electrically verified only.
4. Do you want a landscape fullscreen player? If so, the PPA driver moves from optional to required.

## Sources

- yt-dlp 2026.10.26 source (`yt_dlp/extractor/youtube/_base.py` `INNERTUBE_CLIENTS`, `_video.py` default client lists). Primary source, read locally.
- yt-dlp issues: [#17461 remove android_vr](https://github.com/yt-dlp/yt-dlp/pull/17461), [#17836 JS-less only via visionos](https://github.com/yt-dlp/yt-dlp/issues/17836), [#17904 bot-detection](https://github.com/yt-dlp/yt-dlp/issues/17904), [PO Token Guide](https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide)
- [esp_h264 1.4.1 readme](https://components.espressif.com/components/espressif/esp_h264/versions/1.4.1/readme) (software decoder: baseline only, P4 performance table, no HW decoder)
- [rusty_h264-decoder](https://docs.rs/crate/rusty_h264-decoder/0.7.0/source/README.md), [rust_h264](https://docs.rs/crate/rust_h264/0.4.0) (crates downloaded and tested locally)
- [opus-rs](https://docs.rs/crate/opus-rs/latest), [mousiki](https://docs.rs/crate/mousiki/0.2.0), [ruopus](https://docs.rs/crate/ruopus/latest), [mpeg-ts](https://docs.rs/crate/mpeg-ts/0.3.0)
- [esp-hosted-mcu #112 throughput](https://github.com/espressif/esp-hosted-mcu/issues/112), [ESP32-P4 CoreMark (CNX)](https://www.cnx-software.com/2023/01/06/espressif-esp32-p4-a-400-mhz-general-purpose-dual-core-risc-v-microcontroller/)
- [Invidious instances](https://docs.invidious.io/instances/), [Invidious/Piped 2026](https://www.sumguy.com/invidious-piped-redlib-nitter-2026), [NewPipe release notes](https://flathub.org/it/apps/net.newpipe.NewPipe)
- [YouTube Terms of Service](https://www.youtube.com/static?template=terms&gl=GB)
- Probe scripts and raw outputs (scratch, not committed): `/tmp/claude-1000/-home-michaelphan-projects-esp32p4/277f04d6-3515-4ef3-9a81-7bda58b8843a/scratchpad/yt/`, `.../h264bench/`, `.../h264nostd/`, `.../opusnostd/`
