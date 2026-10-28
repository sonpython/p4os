# On-device YouTube playback (ytplay): implementation report

Date: 2026-10-28. Branch: worktree `agent-a2cecd7ff2f732f45`, merged with main 13c2682 (TLSF heap, lock screen). Not pushed.

## Outcome

The playback engine is in a new crate, `ytplay/`. It plays YouTube end to end in the simulator, against the live YouTube service from the Pi. The acceptance video 5-EBS0tyJyc (45 min) plays in audio-only, 144p and 240p modes, and seeking works. On the board, decoding is benchmarked and bit-exact with the host. One end-to-end playback session also ran on the board: lookup, ranged fetches, decode, A/V sync and seek all worked, but video reached only 1–4 fps. That session exposed three faults: SRAM ran out, the Opus decoder overflowed the audio thread's stack, and a storyboard JPEG overflowed the GUI thread's stack. All three are fixed on the host side. The fixes have not been re-measured on the board: the host's USB over-current fault took the board offline, and the coordinator stopped all board access.

## Phases

| Phase | State |
|---|---|
| 0. Decoder choice and board benchmark | Done |
| 1. Stream acquisition | Done; verified live from the simulator |
| 2. Audio-only mode | Done on the host. On the board, the first run decoded audio (rising `sum` counter, 0 underruns) before the faults above; the fixed build is not yet verified |
| 3. Video | Done on the host. On the board, decoding is bit-exact but the end-to-end frame rate is unproven after the fixes |
| 4. Fallbacks | Storyboard + audio mode, clear error codes, URL refresh: done. HLS: not done (see questions) |

## Phase 0: decoder

- **Licences.** rusty_h264-decoder 0.16.1 is BSD-2-Clause, (c) 2026 Mata Network. rust_h264 0.5.0 is MIT/Apache-2.0. Both are compatible with a public repository.
- **Choice: rusty_h264.** It is vendored at `third_party/rusty_h264/{decoder,common}` with its LICENSE and a README that lists the changes: the manifests are trimmed, the SIMD and allocator crates are dropped, and the decoder runs scalar `no_std`. rust_h264 was rejected: it was 30% slower on the board, peaked at 9.7 MB of PSRAM at 144p, and ran out of memory (panic) at 240p.

Board measurements, one core at 360 MHz, 300 frames of real YouTube streams served from the Pi (`yt bench`):

| Stream | Decode | fps (one core) | cycles/MB | YUV→RGB565 | Peak heap |
|---|---|---|---|---|---|
| 144p (itag 160) | 14.3 ms/frame (max 91 ms on an IDR) | 70 | ~35.7k | 4.5 ms/frame | SRAM +223 KiB, PSRAM +1.1 MiB |
| 240p (itag 133) | 45.8 ms/frame (max 109 ms) | 21 | ~40.7k | 16.3 ms/frame* | SRAM +81 KiB, PSRAM +3.3 MiB |

\* Measured before a 2-pixel chroma optimisation, which cut 144p from 5.5 ms to 4.5 ms.

- Frame CRCs on the board equal the host decode of the same bytes: 144p `c967f03b`, 240p `91dc3279`, over 300 frames each.
- **Decision: 144p is the default.** 240p cannot sustain 25–30 fps on one core, so it is offered as an option that drops frames.
- **Opus (opus-rs 0.1.40):** 2.73 ms per 20 ms packet on the board, which is 13.7% of one core when decoding alone (3,000 packets).
- **Opus accuracy against ffmpeg/libopus on the Pi:** 41.8 dB SNR over 0.5–10 s, with a median per-packet SNR of 74 dB. The largest differences fall in the first ~0.3 s. Board and host PCM are not bit-identical, because opus-rs has NEON paths on aarch64. `yt bench-opus URL REF_URL` now computes the SNR against an ffmpeg PCM on the board itself; it has not been run yet.

## What was built

- **`ytplay/`**:
  - InnerTube client table: built in, or read from `/sd/p4os/youtube-clients.json` with validation and a fallback.
  - `/player` lookup with visitorData bootstrap and retry, and format selection (Opus 250/249/251; H.264 160/133).
  - Paced HTTP Range fetches: up to 384 KB per range, issued only while less than 20 s of video or 30 s of audio is buffered. Retries with backoff. URLs are refreshed before they expire and on 403/410.
  - Demuxers: zero-copy JSON reader, fMP4 (`sidx`, `moof`, `trun`, converted to Annex-B in place), WebM (Cues, all lacing types).
  - Lock-free SPSC queues and a PSRAM sample ring.
  - Decoder threads: Opus on worker `ytaudio` (128 KB stack); H.264 on worker `ytvideo` (64 KB stack), with presentation-order reordering, dropping of frames more than 60 ms late, and a skip to the next keyframe when 1.5 s behind.
  - The audio clock is the master clock, with a wall-clock fallback when there is no audio.
  - Player screen: portrait letterbox, landscape fullscreen with a tap overlay, progress drag to seek, ±10 s, play/pause, 144p/240p/Audio switch, status line, error box with Retry. Storyboard slideshow in audio mode.
  - Error codes: `YT-NET`, `YT-HTTPnnn`, `YT-UNPLAYABLE`, `YT-CLIENT`, `YT-NOFORMAT`, `YT-LIVE`, `YT-EXPIRED`, `YT-PARSE`, `YT-DECODE`, `YT-CONFIG`, `YT-SYSTEM`.
- **Seam for the browsing app:** `p4os_ytplay::app::play(sys, "ID or URL")`. It posts a command, and the desktop opens the `ytplayer` app. `PlayerView` can also be embedded directly.
- **Shared contracts changed:**
  - SDK: `HttpRequest` plus `Network::request` (a required method); `System::spawn_worker` and the `Worker` trait (with a default implementation); `App::fullscreen` (with a default implementation).
  - net: `Url::request_with` (rejects header injection) and `complete_len`. Responses now finish at Content-Length or the last chunk, because googlevideo keeps connections open despite `Connection: close`. Chunked bodies are decoded.
  - kernel: `worker.rs` (spawn_worker with a free-slot check), `yt.rs` shell commands, `net.rs` request support, `gui.rs` spawn_worker.
  - desktop: fullscreen apps and auto-opening the player for queued commands.
  - sim: request, workers and `yt` script commands.
- **Shell:** `yt play ID|URL [144|240|audio]`, `yt pause`, `yt seek ±N`, `yt stop`, `yt stat`, `yt bench URL [N]`, `yt bench-opus URL [REF]`.
- **Docs:** Vietnamese section "YouTube trên board" in `docs/getting-started.md` (how it works, limits, how to update the client JSON, error codes), plus `docs/README.md` rows.
- **Tests:** 39 in ytplay, plus net and remote tests. They use tiny synthetic fixtures generated by `ytplay/tests/fixtures/make-fixtures.sh`, not YouTube media:
  - `pattern.mp4`: 29 KB, H.264 Main with CABAC and B-frames, decoded bit-exact against ffmpeg frame CRCs.
  - `tone.webm`: 11 KB Opus; SNR above 25 dB against the source tones.

## Board session (first build) and the fixes it led to

Run: `yt play dQw4w9WgXcQ`. TLS handshake per range: 230–430 ms. Lookup through visitorData worked, and the seek to +60 s worked.

| Observed | Cause | Fix (host-verified) |
|---|---|---|
| fps 1–4; decode avg 130 ms (14 ms in the benchmark); CPU 86%/99% | GUI thread (prio 3) busy about 75 ms per frame (45 ms render + 30 ms present) at a 15 ms tick, pre-empting the decoder pinned to its core | Redraw only when the next picture is due (tick 4–40 ms); each pixel written once; decoder no longer pinned |
| SRAM 409/414 KB used, largest free block 280 B | Every demuxed packet and access unit, and every 20 ms PCM chunk, was a small heap block, and small blocks come from SRAM | Packets and access units are now views into the fetched chunk (PSRAM). PCM goes through one 375 KiB ring (PSRAM) |
| Stack overflow in `ytbench` / the audio path | opus-rs `no_std` needs about 72 KB of stack to build its CELT mode and about 28 KB per decode frame. Found from the ELF prologues | Opus decodes on worker `ytaudio` (128 KB stack); the audio thread only copies PCM |
| Stack overflow in `gui` in audio mode | Storyboard JPEG decode (28 KB frame) on the 48 KB GUI stack | JPEG decodes on a short-lived `ytsheet` worker (96 KB stack) |

Stack use measured on the board: `ytvideo` 25 KB of 64 KB, `ytaudio` 46 KB of 128 KB, `net` 15 KB.

## Memory budget (expected after the fixes; not re-measured on the board)

- **PSRAM, about 3 MB at 144p:**
  - decoder DPB and transients: ~1.1 MB;
  - 6 RGB frame buffers: 74 KB each at 144p, 204 KB at 240p;
  - fetched chunks: up to 384 KB each, two streams, alive while queued;
  - PCM ring: 375 KB;
  - Opus decoder: 175 KB;
  - worker stacks: 128 + 64 KB;
  - player JSON: transient, ≤200 KB.
- **SRAM:**
  - rusty_h264 per-frame planes at 144p: transient, up to about 220 KB peak in the benchmark;
  - queues of small descriptors: tens of KB;
  - YUV tables: 10 KB.

## Verification

- `cargo test -p p4os-apps -p p4os-gui -p p4os-net -p p4os-sdk -p p4os-ytplay --target aarch64-unknown-linux-gnu`: all pass (108 + 1 + 10 + 10 + 39).
- The kernel release build and the simulator build are clean, with zero warnings.
- Simulator, live, video 5-EBS0tyJyc:
  - 144p: 28.9 fps;
  - 240p: 29.7 fps;
  - audio + storyboard: works; seek works;
  - pictures shown are drawn from the stream (scratch screenshots only, not committed).

## Next steps once the board is back

1. Flash the current branch and run `yt play 5-EBS0tyJyc audio`: check the audio on the speaker, `yt stat` underruns, and `mem`.
2. Run `yt play 5-EBS0tyJyc`: read fps, frames dropped, CPU per core, the GUI frame time, and SRAM/PSRAM from `yt stat`. Take a camera shot.
3. Run `yt bench-opus` against the reference PCM.

If the GUI still cannot reach 25 fps (its full-screen repaint plus present costs ~75 ms per frame), the next step is a kernel video fast path. That would be a direct blit of the video rectangle into the panel buffer, or the PPA scaler. It needs a decision from the kernel owner.

## Unresolved questions

1. **HLS fallback:** not implemented. YouTube's HLS variants carry only AAC audio, and there is no `no_std` AAC decoder, so HLS could give silent video only. Is silent video worth a TS demuxer?
2. **Lock screen:** while the screen is locked, the desktop does not call the player, so playback starves. Should audio-only keep playing under the lock screen? That needs a desktop hook.
3. **API overlap:** `Network::request` is a required trait method. If the browsing-app worker added its own POST API, the two need reconciling at merge.
4. **Leftover fetches:** fetches still running when the player closes are collected only the next time the player opens. Until then the kernel keeps their bodies, up to ~768 KB.
