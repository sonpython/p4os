#!/usr/bin/env bash
# Halt the CPU over the built-in USB-JTAG, dump display-related peripheral
# registers, and resume. Usage: regdump.sh OUT_FILE
set -euo pipefail
OOCD="$HOME/tools/openocd-esp32"
OUT="$1"
CMDS="init; halt;"
dump() { CMDS="$CMDS echo \"== $1 $2 [read_memory $2 32 $3]\";"; }
dump dsi_host_a 0x500A0000 28      # 0x00..0x6C (skip 0x70 payload FIFO pop)
dump dsi_host_b 0x500A0074 75      # 0x74..0x19C
dump dsi_bridge 0x500A0800 40
dump clkrst 0x500E6000 64
dump clkrst_rst 0x500E60C0 4
dump pmu_ldo 0x50115180 24
dump iomux_23_27 0x500E1060 6
dump gpio_out_en 0x500E0004 8
dump gpio_pin_23_27 0x500E00D0 5
dump gpio_outsel_23_27 0x500E05B4 5
dump dmac 0x50081000 24
dump dmac_ch0 0x50081100 48
dump usb_wrap 0x50080000 2
CMDS="$CMDS resume; shutdown"
"$OOCD/bin/openocd" -s "$OOCD/share/openocd/scripts" -f board/esp32p4-builtin.cfg -c "$CMDS" 2>&1 \
  | grep -E "^==" > "$OUT"
wc -l "$OUT"
