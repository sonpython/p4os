#!/usr/bin/env bash
# Run the ESP-IDF/FreeRTOS benchmark image(s) and then the p4os `bench`
# command on the board, saving the raw console logs under bench/results/.
#
# Only the factory app partition (0x10000) is written; the bootloader,
# partition table and storage partition are left alone. p4os is flashed
# back at the end, also when a step fails. Hold the board lock:
#   scripts/board-lock.sh bench/run-on-board.sh freertos=/path/to/p4bench.bin
#
# Arguments   LABEL=IMAGE pairs, one per ESP-IDF build to run
# P4OS_IMAGE  p4os app image; built with scripts/build-image.sh when unset
set -euo pipefail
cd "$(dirname "$0")/.."
[ $# -gt 0 ] || { echo "usage: $0 LABEL=IDF_IMAGE..." >&2; exit 2; }
PORT="${PORT:-/dev/ttyACM0}"
P4OS_IMAGE="${P4OS_IMAGE:-$(scripts/build-image.sh)}"
STAMP="$(date +%y%m%d-%H%M%S)"
mkdir -p bench/results

flash_app() {
  esptool --chip esp32p4 --port "$PORT" -b 921600 write-flash 0x10000 "$1" >/dev/null
}
restore_p4os() { flash_app "$P4OS_IMAGE"; }
trap restore_p4os EXIT

for pair in "$@"; do
  label="${pair%%=*}"
  image="${pair#*=}"
  [ -f "$image" ] || { echo "missing image: $image" >&2; exit 1; }
  flash_app "$image"
  python3 bench/capture.py --port "$PORT" --timeout 60 >"bench/results/$label-$STAMP.log"
  echo "bench/results/$label-$STAMP.log"
done

restore_p4os
trap - EXIT
python3 bench/capture.py --port "$PORT" --send bench --boot-wait 6 --timeout 60 >"bench/results/p4os-$STAMP.log"
echo "bench/results/p4os-$STAMP.log"
