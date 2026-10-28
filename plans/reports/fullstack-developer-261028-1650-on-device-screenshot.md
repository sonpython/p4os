# On-device screenshot: implementation report

Status: DONE_WITH_CONCERNS. On-device capture works on the real board in portrait, after a tap and in landscape. The passcode-refusal check was not run on hardware: the board dropped off USB (host over-current fault) before that session could flash.

## What was built

- `media/src/shot.rs` (no_std, host-tested). It contains:
  - the frame protocol: `ESC[P4SHOT`, a 16-byte header, the payload, a CRC-32 and `ESC[P4END]`;
  - a lossless RLE codec for RGB565 with three token types (literal, run, and "copy the row above");
  - a table-driven CRC-32;
  - a store-only zlib PNG writer.
- `kernel/src/display.rs`:
  - `present_with(fb, flags)` records `FRAME_LANDSCAPE` and `FRAME_SECURE` for each frame;
  - a `FLIP_GATE` mutex is held across a flip;
  - `with_front()` reads the front buffer under that gate, so a capture never tears. The GUI waits at most the time of one copy (about 23 ms).
- `kernel/src/gui.rs` changes one line: it calls `present_with` with flags taken from the orientation and from `Desktop::passcode_typed()`.
- `apps/src/lock/mod.rs`: new `Lock::has_typed_digits()`, with a unit test. `apps/src/desktop.rs`: new `passcode_typed()`.
- `kernel/src/console.rs`: `binary_transfer()` holds the output mutex, so other threads' logs wait. It writes bytes untranslated and abandons the transfer after the host stalls for 1 s.
- `kernel/src/screenshot.rs` adds the commands `screenshot [fast|raw]` and `screenshot sd [NAME]`:
  - `sd` saves a PNG to `/sd/Pictures/Screenshots/`.
  - Landscape frames are rotated back upright.
  - Capture is refused while passcode digits are typed.
  - Streaming is refused from the Terminal app, because its output is captured into text.
  - Optional extra: the BOOT button (GPIO35) saves a capture to SD and flashes the screen grey for 120 ms.
- `scripts/screenshot.py`:
  - It opens the port without toggling DTR/RTS.
  - It takes the board lock itself unless `P4OS_BOARD_LOCKED=1` is set.
  - It skips log lines around the frame, checks the CRC, retries once, and exits 1 on failure.
  - It offers `--expect ref.png --tolerance N --max-diff PCT`, and `--input` to compare two files without the board.
- `scripts/board-ui.py`: its `shot NAME` step now tries on-device capture first, falls back to `cam.py shot`, and prints which source it used.
- `scripts/test_screenshot.py`: 15 tests covering the codec, the frame parser (log lines around the frame, CRC errors, interleaved bytes, truncation, bad headers) and the diff. The 4x3 sample vector is shared with the Rust tests.
- `docs/getting-started.md` (Vietnamese): new section "Chụp màn hình ngay trên board", the shell command list, and the test commands.

## Board measurements (merged main build)

| Capture | Size | Board time (capture + encode + send) | Host round trip |
|---|---|---|---|
| Home, fast (RLE) | 28.8 KB | 23 + 18 + 85 ms | 0.30 s |
| Home, raw | 768 KB | 23 + 54 + 2317 ms | 2.51 s |
| Calculator after a tap, fast | 15.0 KB | 23 + 16 + 44 ms | 0.19 s |
| Landscape Settings, fast (800x480) | 22.4 KB | 23 + 17 + 66 ms | 0.22 s |
| `screenshot sd` PNG | 1.15 MB | 23 + 111 ms encode, 3623 ms SD write | n/a |

- The raw and fast captures of the same screen were identical: 0.000 % of pixels differ at tolerance 0.
- Raw transfer runs at about 330 KB/s over USB-Serial-JTAG. RLE is about 27 to 50 times smaller, so `fast` is the default and it is still pixel-exact.
- The hardware JPEG encoder was not used. Its driver is complex and JPEG loses detail, while RLE already brings a capture under 0.3 s.
- Camera cross-check of landscape: the camera crop (blurry, and the stored ROI only covers part of the screen) shows the same layout as the on-device capture: "Display" highlighted in the sidebar, the blue "Landscape" button and the brightness knob.
- I checked the PNG writer on the host by decoding its output with PIL; it matched the pixels exactly.

## Tests

- The kernel builds with zero warnings, using `RUSTFLAGS="-C link-arg=-Tlink.x"`.
- `cargo test -p p4os-apps -p p4os-gui -p p4os-net -p p4os-sdk -p p4os-media --target aarch64-unknown-linux-gnu` passes (169 + 1 + 11 + 12 + 10 tests).
- `python3 -m unittest discover -s scripts -p 'test_*.py'` passes (15 tests).

## Still needs hardware verification

1. Passcode refusal on the board. The script is ready at `scratchpad/lock-session.sh`. It sets passcode 1111 in Settings, runs `lock now`, taps one digit, expects `screenshot: refused while a passcode is being typed`, presses backspace, expects a capture to succeed, then runs `lock reset CLEAR-PASSCODE`. The logic is unit-tested (`typed_digits_are_reported_until_submitted`).
2. The BOOT-button capture and its flash. This needs a physical button press. It shares its code with `screenshot sd`, which was verified on the board.

## Notes

- The passcode entry in Settings > Security (setting or changing a passcode) is not covered by the refusal; only the lock-screen keypad is. That page lives in `settings.rs`, which other workers are editing. Its dots do not reveal the digits anyway.
- Interrupt-context log lines can still land inside a frame. The CRC catches them and the host retries.
- An existing UI issue, not caused by this change: in the landscape status bar the clock overlaps "CPU" ("17:52PU").
- Writing the store-only PNG to SD takes about 3.6 s. A deflate encoder would shrink it if that ever matters.
