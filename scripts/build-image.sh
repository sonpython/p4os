#!/usr/bin/env bash
# Build the kernel and wrap it in an ESP app image for the factory partition.
set -euo pipefail
cd "$(dirname "$0")/.."
. "$HOME/.cargo/env"
cargo build --release -p p4os-kernel
ELF=out/riscv32imafc-unknown-none-elf/release/p4os
esptool --chip esp32p4 elf2image --flash-mode dio --flash-freq 80m --flash-size 16MB \
  --min-rev-full 0 --max-rev-full 199 -o out/p4os.bin "$ELF" >/dev/null
echo "out/p4os.bin"
