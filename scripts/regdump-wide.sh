#!/usr/bin/env bash
# Wider JTAG register dump for comparing display/backlight setup between
# firmwares: all GPIO output selects and IO MUX pads, LEDC, the full DSI
# bridge, and D-PHY internal registers read through the PHY test port.
set -euo pipefail
OOCD="$HOME/tools/openocd-esp32"
OUT="$1"
CMDS="init; halt;"
dump() { CMDS="$CMDS echo \"== $1 $2 [read_memory $2 32 $3]\";"; }
dump gpio_outsel 0x500E0558 57
dump iomux 0x500E1004 57
dump gpio_pin 0x500E0074 57
dump gpio_en 0x500E0020 4
dump ledc 0x500D3000 64
dump dsi_bridge 0x500A0800 64
dump dsi_host_c 0x500A0090 12
# PHY test-port reads: TST_CTRL1 = testen|addr, clock it in, read testdout [15:8].
for a in 0x10 0x11 0x12 0x17 0x18 0x19 0x22 0x44 0x60 0x64 0x65 0x70 0x71 0x72 0x74 0x75; do
  CMDS="$CMDS mww 0x500A00B4 0; mww 0x500A00B8 [expr 0x10000|$a]; mww 0x500A00B4 2; mww 0x500A00B4 0; mww 0x500A00B8 0; echo \"== phy_$a 0x0 [read_memory 0x500A00B8 32 1]\";"
done
CMDS="$CMDS resume; shutdown"
"$OOCD/bin/openocd" -s "$OOCD/share/openocd/scripts" -f board/esp32p4-builtin.cfg -c "$CMDS" 2>&1 \
  | grep -E "^==" > "$OUT"
wc -l "$OUT"
