#!/usr/bin/env bash
# Write a user app image into a flash slot of the "storage" partition.
#   scripts/flash-app.sh SLOT IMAGE     e.g. scripts/flash-app.sh 0 out/userapps/hello.p4app
# Slot N lives at 0x720000 + N * 0x10000 (N = 0..13); nothing outside
# 0x720000..0x7FFFFF is ever written. Takes the board lock unless the caller
# already holds it (scripts/board-lock.sh exports P4OS_BOARD_LOCKED=1).
set -euo pipefail
cd "$(dirname "$0")/.."

SLOT_BASE=$((0x720000))
SLOT_SIZE=$((0x10000))
SLOTS=14

if [ $# -ne 2 ]; then
  echo "usage: $0 SLOT IMAGE" >&2
  exit 2
fi
slot=$1
image=$2
if ! [[ "$slot" =~ ^[0-9]+$ ]] || [ "$slot" -ge "$SLOTS" ]; then
  echo "flash-app: slot must be 0..$((SLOTS - 1))" >&2
  exit 2
fi
if [ ! -f "$image" ]; then
  echo "flash-app: no such image: $image" >&2
  exit 2
fi
if [ "$(head -c 4 "$image")" != "P4AP" ]; then
  echo "flash-app: $image is not a p4os app image (build it with scripts/build-userapps.sh)" >&2
  exit 2
fi
size=$(stat -c %s "$image")
if [ "$size" -gt "$SLOT_SIZE" ]; then
  echo "flash-app: image is $size bytes, a slot holds $SLOT_SIZE" >&2
  exit 2
fi

if [ "${P4OS_BOARD_LOCKED:-}" != 1 ]; then
  exec scripts/board-lock.sh "$0" "$@"
fi

addr=$(printf '0x%x' $((SLOT_BASE + slot * SLOT_SIZE)))
PORT="${PORT:-/dev/ttyACM0}"
echo "flash-app: writing $image ($size bytes) to slot $slot at $addr" >&2
esptool --chip esp32p4 --port "$PORT" -b 921600 write-flash "$addr" "$image"
