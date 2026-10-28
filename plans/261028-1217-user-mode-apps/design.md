# User-mode apps: syscall ABI and image format

Status: implemented (first step). Code: `kernel/src/usermode.rs`, `pmp.rs`,
`syscall.rs`, `apploader.rs`, `abi/`, `userapps/`, `scripts/mkapp.py`.

## Privilege model

- Kernel threads stay in M-mode. A user app is a scheduler thread that drops
  to U-mode (`mstatus.MPP`/`mcause.MPP` = 0 in its saved frame).
- While a thread runs in U-mode, `mscratch` holds its kernel stack top. The
  trap entry swaps it in, so a trap from U-mode never stores through the
  user `sp`; the user frame always lives at `ktop - 144`. In M-mode
  `mscratch` is 0. The trap entry also reloads the kernel `gp`.
- Syscalls and faults are not served in trap context. The trap handler
  builds an M-mode frame just below the user frame that runs the syscall (or
  the kill path) in the app's own thread, on its kernel stack, with
  interrupts on. Syscalls may block or sleep. Kernel call `RETURN_TO_USER`
  (M-mode ecall, a7 = 0x101) resumes the user frame.
- A fault in U-mode logs one line and ends only that thread:
  `*** app 'crash' (thread 4) killed: store access fault at pc=… (app+0x74), mtval=0x4ff00000`.

## PMP

Before every `mret` to U-mode the hart's PMP is reprogrammed for the
current app: entry `f` (OFF) holds the base, `f+1` TOR R+X to the end of
code, `f+2` TOR R+W to the end of the stack. Everything else is denied to
U-mode; M-mode is unaffected (no kernel entry is locked).

The ESP-IDF v5.5 bootloader leaves entries 0, 1, 2 and 15 **locked**
(they bind every mode and survive until reset):

| entry | range | grants | for apps |
|---|---|---|---|
| 0 | 0x20000000 + 256 MiB (CPU subsystem: CLIC, debug) | RW | reachable |
| 1 | 0x3FF00000 + 128 KiB (CPU peripherals: trace, bus monitor, cache controller) | RW | reachable |
| 2 | 0x4FC00000 + 128 KiB (mask ROM) | RX | reachable |
| 15 | 0x50000000 + 1 MiB (HP peripherals) | RW | fenced by an unlocked no-access copy at a lower index |

Static priority means entries 0..2 always win, so `f` = 3. **Known hole:** an
app can read/write the CLIC and the cache controller and run ROM code, so it
can still disturb the kernel (e.g. mask interrupts, drop dirty cache lines).
Kernel SRAM, PSRAM, flash, TCM and all HP/LP peripherals fault. Closing the
hole needs a boot path that does not lock those entries (own 2nd-stage
loader), or running apps on core 1, whose PMP the bootloader never touches
(to verify once SMP lands).

## Syscall ABI

`ecall` with a7 = number, a0..a5 = arguments, result in a0. Negative
results are errors (`p4os_abi::err`). Only a0 is changed; FP registers are
preserved, but the user wrapper declares the C ABI clobbers anyway.

| a7 | call | args | result |
|---|---|---|---|
| 0 | `exit` | code | does not return |
| 1 | `write` | fd (1 = console), ptr, len | bytes written, `EBADF`, `EFAULT` |
| 2 | `sleep_ms` | ms | 0 |
| 3 | `uptime_ms` | – | ms since boot (wraps) |
| 4 | `yield` | – | 0 |
| 5 | `getpid` | – | thread id |
| other | | | `ENOSYS` (-38) |

Every user pointer is checked against the caller's own region before the
kernel touches it (`UserApp::user_bytes`); otherwise `EFAULT` (-14).

## App image (`.p4app`)

Little-endian, built by `scripts/build-userapps.sh` → `scripts/mkapp.py`
from a static-PIE ELF linked at 0 with `userapps/p4os-user/link.x`.

| offset | field | notes |
|---|---|---|
| 0 | magic `"P4AP"` | |
| 4 | version u16 = 1, header size u16 = 64 | |
| 8 | name `[u8; 16]` | NUL padded |
| 24 | `file_size` | bytes of load image after the header |
| 28 | `rx_size` | end of code + rodata, multiple of 128 |
| 32 | `mem_size` | image + bss, multiple of 128 |
| 36 | `stack_size` | multiple of 128, ≥ 1 KiB |
| 40 | `entry` | offset from load base |
| 44 | `reloc_count` | u32 offsets after the image |
| 48 | `crc32` | IEEE CRC of load image + relocation table |
| 52 | reserved | zero to 64 |

Loading: allocate `mem_size + stack_size` (≤ 256 KiB), 128-byte aligned and
zeroed; copy the image; add the base to the word at each relocation offset
(only `R_RISCV_RELATIVE` exists in the ELF; the packer stores addends in
place); write back the D-cache and invalidate the I-cache; start at
`base + entry` with sp at the top and the `run` argument in a0. The packer
rejects writable sections below `rx_size`, executable ones above it, and any
other relocation type. The kernel re-validates everything (`p4os_abi::Image::parse`).

## Storage

Flash "storage" partition, slots of 64 KiB at `0x720000 + N * 0x10000`,
N = 0..13 (this feature owns 0x720000..0x7FFFFF). The kernel maps them
read-only through flash MMU entries 0x3F0.. (vaddr 0x43F00000). Write with
`scripts/flash-app.sh N IMAGE` (takes the board lock).

## Not yet

Framebuffer/UI syscalls, app arguments beyond one u32, loading from SD,
per-app heap (`sbrk`), killing a running app from the shell.
