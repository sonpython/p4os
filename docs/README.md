# p4os

A from-scratch operating system for the **ESP32-P4**, written entirely in Rust: no ESP-IDF, no FreeRTOS, no esp-hal. It targets silicon **revision v1.x**, which esp-hal does not support, on the **Guition JC4880P443C** board.

p4os has a preemptive scheduler running on both CPU cores, isolated user-mode apps, a touch desktop with a dozen apps, Wi-Fi with HTTPS, an SD card file system and settings that survive reboots. It is a learning and research project, under active development, and runs on real hardware.

| Home screen (camera capture of the board) | Browser over Wi-Fi | YouTube (simulator, sample data) | Lock screen |
|---|---|---|---|
| ![home](screenshots/board-home.png) | ![browser](screenshots/board-browser-example.png) | ![youtube](screenshots/youtube-feed.png) | ![lock](screenshots/lock-portrait.png) |

## Hardware

| Part | Details |
|---|---|
| SoC | ESP32-P4 rev v1.x: two RISC-V HP cores at 360 MHz, 768 KiB L2 SRAM |
| Memory | 32 MiB PSRAM at 200 MHz, 16 MB SPI flash |
| Display | 4.3" 480 x 800 ST7701S panel over MIPI-DSI (2 lanes), PWM backlight |
| Touch | GT911 over I2C |
| Wi-Fi | ESP32-C6 co-processor over SDIO, running ESP-Hosted v2 |
| Storage | microSD (SDMMC, 4-bit) |
| Audio | ES8311 codec over I2S, speaker amplifier |

The board is the Guition JC4880P443C, not the Espressif EV board; pins and init sequences come from the vendor package and register dumps of the factory firmware.

## Features on `main`

| Area | State |
|---|---|
| Boot | Rust kernel boots from the ESP-IDF 2nd-stage bootloader and raises the CPU to 360 MHz |
| Kernel | Preemptive priority scheduler on **both cores** (SMP, one shared run queue, thread pinning), lazy FP context switch, inter-core interrupts. Kernel critical sections **mask by CLIC threshold**, so fast interrupts (display, audio) still run while a lock is held; **task notifications** wake one thread with at most one scheduler-lock acquisition; uncontended mutex and semaphore need no scheduler lock |
| Memory | **TLSF heap** with constant-time alloc and free, one per memory (SRAM for small blocks, PSRAM for large), per-size thread stack pools; heap use from interrupt context is caught. **Optimised `memcpy`/`memset`/`memmove`/`memcmp`** (`mem/`) moving a cache line per iteration |
| User mode | Apps loaded from flash slots run in RISC-V U-mode on core 1, **isolated by PMP**; a faulting app is killed and the kernel keeps running. Syscalls for console, sleep, time, yield |
| Display | MIPI-DSI via DSI bridge and DW-GDMA, double-buffered page flip, portrait and landscape |
| Storage | SD card with FAT32 and long file names; **persistent settings** in SPI flash (two sectors, CRC, power-loss safe) |
| Network | Wi-Fi STA (WPA2) with autoconnect, DHCP, DNS, SNTP, TCP via smoltcp; **HTTP and HTTPS** with TLS 1.3/1.2 (rustls, certificates verified against the Mozilla roots); apps can send any method with headers and a body, and cancel requests |
| GUI | Immediate-mode toolkit, anti-aliased fonts with Vietnamese, on-screen keyboard; **lock screen** with a PBKDF2-hashed passcode and attempt limiting |
| Apps | Calculator, Files, Photos, Notes, Music, Browser, Terminal, Monitor, Settings, Clock, Paint, 2048, **YouTube** (Data API browsing, Google sign-in by device code; videos open on a phone through a QR code), and an Examples folder ([guide](app-development.md)) |
| Tooling | Host simulator with scripted input and PNG screenshots; host unit tests; serial shell with diagnostics and benchmarks |

## Benchmarks against FreeRTOS

The same micro-benchmarks ran on the same board under p4os and under ESP-IDF v5.5 / FreeRTOS (median of 5 runs, 360 MHz). Full method, numbers and caveats: [benchmark report](../plans/reports/benchmark-261028-p4os-vs-freertos.md) (in Vietnamese) and [`bench/`](../bench/README.md).

| Measurement | p4os | FreeRTOS | |
|---|---:|---:|---|
| Context switch (semaphore ping-pong) | 2264 ns | 5612 ns | p4os 2.5x faster |
| Yield between two threads | 1284 ns | 2316 ns | p4os 1.8x faster |
| Mutex lock + unlock | 698 ns | 964 ns | p4os 1.4x faster |
| Interrupt entry (busy CPU) | 330 ns | 570 ns | p4os 1.7x faster |
| ISR to thread wake-up, average | 2545 ns | 2319 ns | FreeRTOS 10% faster |
| Interrupt latency, worst case | 3250 ns | 813 ns | FreeRTOS better; p4os was drawing the GUI during the run |

- Pure CPU work (CRC32, an integer kernel) is within 7%, so the differences come from the OS design, not the compiler.
- Since the report, three of its findings were acted on. Kernel locking moved to the CLIC threshold and gained task notifications, the answer to FreeRTOS's faster ISR wake-ups; with it an uncontended mutex lock + unlock dropped to about 365 ns. The heap is now TLSF: the worst allocation with 1024 free gaps takes 1.15 µs, against 12.8 µs for the old allocator. `memcpy` was replaced: on the board, a 32 KiB SRAM copy went from 284 to 626 MB/s and a 1 MiB PSRAM copy from 47 to 56 MB/s. The report's mutex, heap and `memcpy` rows predate these changes.

## Quick start

Requires Rust stable with the `riscv32imafc-unknown-none-elf` target (pinned in `rust-toolchain.toml`), `esptool` 5 or newer and `pyserial`. The board is expected on `/dev/ttyACM0` (set `PORT` otherwise).

```sh
scripts/flash.sh                     # build the kernel and flash it to the factory partition
python3 scripts/monitor.py           # reset the board and watch the console (type `help`)

# Simulator: the desktop on Linux, screenshots as PNG (aarch64 host; see the guide for others)
cargo sim "launch calculator; shot /tmp/calc.png"

# Host tests
cargo test -p p4os-apps -p p4os-gui -p p4os-net -p p4os-sdk -p p4os-tlsf -p p4os-mem -p p4os-abi \
  --target aarch64-unknown-linux-gnu

# User-mode apps: build, write to flash slot 0, then `run 0` in the shell
scripts/build-userapps.sh
scripts/flash-app.sh 0 out/userapps/ticker.p4app
```

Board setup, shell commands, pitfalls and recovery are in [getting-started.md](getting-started.md) (in Vietnamese).

## Writing apps

[app-development.md](app-development.md) covers both app models with working examples:

- **Built-in apps** implement the `App` trait, draw with the immediate-mode toolkit, and use OS services through `System` (files, settings, HTTP/HTTPS, clock, audio, orientation). They are unit-tested on the host and screenshotted in the simulator. Examples: [`apps/src/examples/`](../apps/src/examples/).
- **User-mode apps** are separate binaries confined by PMP, talking to the kernel through six syscalls. Example: [`userapps/ticker/`](../userapps/ticker/src/main.rs).

| Hello Counter | Lap Timer | Weather (Open-Meteo over HTTPS) |
|---|---|---|
| ![](screenshots/example-hello.png) | ![](screenshots/example-laps.png) | ![](screenshots/example-weather.png) |

## Repository layout

| Path | Contents |
|---|---|
| `kernel/` | The OS: boot, traps, SMP scheduler, heap, drivers, network, settings, user-mode loader, shell (`no_std`) |
| `gui/` | Canvas, fonts, immediate-mode widgets, on-screen keyboard (`no_std` + `alloc`) |
| `sdk/` | The app interface: `App`, `System`, files, audio, network, settings format |
| `apps/` | Desktop shell, lock screen, built-in apps (YouTube included) and the examples |
| `media/` | JPEG/BMP and MP3/WAV decoding |
| `net/` | HTTP/1.0, SNTP and TLS client logic shared by the kernel and the simulator |
| `mem/` | `memcpy`, `memmove`, `memset`, `memcmp` for the kernel; host-tested against `std` |
| `tlsf/` | The constant-time TLSF allocator used by the kernel heap; host-tested |
| `abi/` | User-mode ABI: syscall numbers and the app image format |
| `userapps/` | User-mode apps (`hello`, `crash`, `ticker`) and their runtime `p4os-user`; a separate workspace |
| `sim/` | Host simulator: runs the desktop on Linux and writes PNG screenshots |
| `bench/` | The FreeRTOS counterpart of the `bench` command and raw results |
| `scripts/` | Build, flash, serial console, app packing, JTAG register dumps, camera-based test tools |
| `docs/` | This page, the [app guide](app-development.md), [getting-started.md](getting-started.md) and screenshots |
| `plans/` | Design notes and research reports |

## Status and limitations

Working on the board, but young. Known gaps:

- Still boots through the ESP-IDF 2nd-stage bootloader.
- The touch driver and coordinate mapping are in place, but the axis calibration is still to be verified with real touches; no speaker is fitted on the test board, so audio is verified electrically only.
- User-mode apps cannot draw, use files or the network yet, and cannot be stopped from the shell. A U-mode access to the CPU subsystem window hangs core 1 until reset (a hardware limit PMP cannot catch).
- HTTP has no keep-alive or TLS session resumption, request bodies are limited to 2 KiB and responses are read whole into RAM.
- No BLE, no PPA graphics acceleration, no power management yet; flash is not encrypted, so the lock screen protects against casual use, not against someone with the board and a cable.

## Roadmap

In progress on branches:

- Watchdogs and crash reports.
- Power management: screen off, wake, sleep.
- Gesture navigation and an app lifecycle (Back/Forward, background and resume), and home screen pages.
- On-device YouTube playback; today the app browses and hands videos to a phone ([research](../plans/reports/researcher-261028-1447-youtube-on-device-playback.md)).

## Licence

Dual-licensed under MIT or Apache-2.0, at your option.
