# Lock screen with passcode: implementation report

Status: DONE_WITH_CONCERNS. The lock screen works on the board. One check takes 305 ms at the required 10k PBKDF2 rounds, about twice the ~150 ms target.

## What was built

- `apps/src/lock/`: the new module.
  - `pin.rs`: zeroing `PinEntry` buffer; `PinHash` record `v1$<iter>$<salt hex>$<hash hex>` (PBKDF2-HMAC-SHA256, 16-byte salt, 10k rounds, constant-time compare via `subtle`). Unknown versions and round counts outside 10k..1M are rejected.
  - `throttle.rs`: 5 free attempts, then a 30 s lockout that doubles every further 5 failures, capped at 15 min. `lock.fails` and `lock.until` are persisted. The end time is stored as Unix seconds when the wall clock is known, otherwise as `?`, in which case the full lockout restarts after a reboot. A clock set backwards cannot extend a lockout past what the failure count earns.
  - `mod.rs`: `Env` trait (blanket impl for every `System`, mock in tests), `AutoLock` (screen-off default, 1m, 5m, never), and the `Lock` state machine. It locks at boot, on `lock_now`, `on_screen_off` and `on_wake`, and on inactivity. It swallows the waking touch, fails closed on a corrupt record, and releases the lock if the passcode is cleared over serial.
  - `screen.rs`: lock screen UI with large clock and date, a Wi-Fi indicator, dots, keypad, shake/red feedback, "N tries left", the lockout countdown, a "Checking..." frame before the hash runs, and both orientations.
  - `security.rs`: Settings > Security page and its flow logic: toggle; set/change/remove; passcode typed twice; current passcode required to change, remove or turn off; settings guesses count towards the same limit and lock the screen on lockout.
- `apps/src/desktop.rs`: `is_locked()`, `lock_now()`, `on_screen_off()` and `on_wake()`. The lock check runs first in `frame` and returns early, so nothing else is drawn and no input reaches apps. There is a padlock button in the status bar when the lock is configured, and `SysProxy` intercepts `System::lock_now`.
- `apps/src/settings.rs`: a small, separate edit (Security section, field, dispatch, reset).
- `sdk`: `System::fill_random` (default false) and `System::lock_now` (default no-op); `config::LOCK_*` keys; `is_masked()` so `config list` shows `lock.pin = [set]`. `lock.pin` stays readable through `config_get` because the desktop must verify against it.
- Kernel (tiny): `gui.rs` gets `fill_random` from the HW RNG and `request_lock()`. I also removed the per-touch `touch down raw` log, because on the keypad it would spell out the passcode on serial. In `shell.rs`: `lock`, `lock now`, `lock bench`, and `lock reset` / `lock reset CLEAR-PASSCODE`. The reset is serial-only and refused from the Terminal app.
- Simulator: `/dev/urandom` random source, plus `config KEY VALUE` and `lock` script commands.
- Docs: `docs/getting-started.md` has a new Vietnamese section "Màn hình khoá" covering usage, storage, rate limit, recovery and the physical-access limits, plus updates to the shell command and config key lists. `docs/README.md` GUI row updated. Screenshots are `docs/screenshots/lock-*.png` (portrait, landscape, wrong, lockout, settings portrait/landscape, passcode entry).

## Verification

- `cargo test -p p4os-apps -p p4os-gui -p p4os-net -p p4os-sdk --target aarch64-unknown-linux-gnu` passes (apps 108, including 39 lock tests: hashing and a known vector, versioning, wrong PIN, rate-limit schedule, reboot persistence with and without a wall clock, settings flows, enable without a PIN leading to setting one).
- Kernel release build and simulator build: zero warnings.
- Board (inside `scripts/board-lock.sh`):
  - `lock bench`: hash 305 ms, verify 304 ms.
  - Through injected taps, I set passcode 2580 in Settings, ran `lock now` (camera showed the lock screen), and entered the right passcode, which unlocked back to Settings.
  - The wrong-passcode state was not caught by the camera, which was too slow for the 2.5 s feedback window; it is covered in the simulator and host tests.
  - Cleanup ran `lock reset CLEAR-PASSCODE`; the board was left with the lock off and no passcode.

## Concerns / decisions for the orchestrator

1. **Timing vs iteration floor.** At 10k rounds a check takes 305 ms, because software SHA-256 costs about 5.5k cycles per block and the core has no Zbb rotate instruction. I kept 10k, the hard floor in the spec, and show "Checking..." during the check. Options:
   - Accept 305 ms.
   - Drop to about 5k rounds, which breaks the ≥10k floor.
   - Add a hardware SHA accelerator driver (a separate kernel task; likely tens of ms). The record stores its round count, so any change needs no migration.
2. **Merge points.** `kernel/src/gui.rs` has three small hunks: the lock request check in the loop, `request_lock`/`LOCK_REQUEST`, and `fill_random`, plus the removed touch log line. `settings.rs` changes the `SECTIONS` array length to 6. The power and gesture workers touch both files.
3. **Power integration.** The power worker should call `desktop.on_screen_off(&mut sys)` / `desktop.on_wake(&mut sys)`. Until those events arrive, the "screen off" policy locks after 60 s without input.
4. **Extra requirement I added.** Switching the lock off needs the current passcode; otherwise "Remove needs the passcode" would be pointless.
5. **No battery indicator.** The lock screen has no battery status because no battery API exists.
