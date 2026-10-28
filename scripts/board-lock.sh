#!/usr/bin/env bash
# Run a command with exclusive use of the board (serial port, JTAG, camera).
# Parallel workers must wrap every flash/test session in this:
#   scripts/board-lock.sh bash -c 'scripts/flash.sh && python3 scripts/monitor.py 10'
set -euo pipefail
# Lets nested scripts (e.g. flash-app.sh) know the lock is already held.
export P4OS_BOARD_LOCKED=1
exec flock --timeout 1800 /tmp/p4os-board.lock "$@"
