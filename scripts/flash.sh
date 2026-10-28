#!/usr/bin/env bash
# Flash the kernel image into the factory app partition (0x10000).
# The ESP-IDF bootloader and partition table already on the board are kept.
set -euo pipefail
cd "$(dirname "$0")/.."
PORT="${PORT:-/dev/ttyACM0}"
IMAGE="$(scripts/build-image.sh)"
esptool --chip esp32p4 --port "$PORT" -b 921600 write-flash 0x10000 "$IMAGE"
