# SDK docs, README refresh and example apps

Status: done. Branch merged with main at 2c12d55 (YouTube app, CLIC-threshold locking, HTTP requests with body, `Network::cancel`, `config_remove`).

## Delivered

- `docs/README.md`: English, public-facing, rewritten for current main: hardware, features (SMP, CLIC-threshold locking and task notifications, TLSF heap, optimised memcpy, PMP-isolated user apps on core 1, lock screen, YouTube, HTTP/HTTPS, SD, settings), FreeRTOS benchmark highlights with a note on which report rows predate later changes, screenshots, quick start, layout table, Writing apps section, limitations, roadmap.
- `docs/app-development.md`: guide for both app models, with a Vietnamese summary at the top; frame loop, layout, widgets, input, every `System` service, registration, host testing, simulator (incl. `mock`), build/flash; user-mode ABI, syscalls, limits, build/flash/run, isolation guarantees and the core-1 hang; best practices; API quick reference from the trait definitions; coming soon (gestures/lifecycle, home pages and app groups, power management). Linked from `getting-started.md`.
- Examples behind one **Examples** launcher entry (`apps/src/examples/`): Hello Counter, Lap Timer (laps in settings), Weather (Open-Meteo over HTTPS, cancel, error states, JSON field reader). A folder app was chosen over a cargo feature so the examples always build and run their tests, while the launcher gets one icon and the registration is one line in `APPS`.
- `apps/src/examples/testkit.rs`: host harness driving `update` with taps and an in-memory `System`.
- `userapps/ticker`: user-mode example using every syscall; registered in `userapps/Cargo.toml` and `scripts/build-userapps.sh`.
- Screenshots `docs/screenshots/example-*.png` from `docs/screenshots/examples.sim`, weather answered by fixtures in `sim/fixtures/examples/`.
- `.cargo/config.toml`: corrected the `cargo sim` usage comment. `cargo sim -- "..."` (as the old README said) passes a literal `--` to the simulator and panics; the alias already ends in `--`.

## Verification

- `cargo test -p p4os-apps -p p4os-gui -p p4os-net -p p4os-sdk --target aarch64-unknown-linux-gnu`: 185 + 1 + 12 + 10 passed, no warnings; `p4os-tlsf`, `p4os-mem`, `p4os-abi` also pass.
- Kernel (`RUSTFLAGS="-C link-arg=-Tlink.x"`), simulator and `scripts/build-userapps.sh` build without warnings.
- Board: `ticker` flashed to slot 13 and run with `run 13`; output matches the guide (5 ticks, +0 ms, EBADF path, exit 0).
- Every Rust/TOML line in the docs' code blocks exists verbatim in the sources; all relative links and anchors resolve (script check).

## Notes for merge

- The gesture branch adds `Desktop::register_group`/`register_app`. Examples are registered with the single line `examples::INFO,` in `APPS` (`apps/src/lib.rs`), since that API is not on main. At merge time that line can become a group registration; the folder app and the "Coming soon" note in the guide would then be updated.
- Flash slot 13 on the board now holds `ticker`.

## Unresolved questions

- None blocking. If the gesture branch changes the `App` trait, the guide's App section and API table need a refresh.
