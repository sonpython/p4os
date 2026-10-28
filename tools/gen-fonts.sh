#!/usr/bin/env bash
# Regenerate all bundled bitmap fonts from the system DejaVu fonts.
set -euo pipefail
cd "$(dirname "$0")/.."
D=/usr/share/fonts/truetype/dejavu
for s in 14 18 24 32; do python3 tools/gen-font.py $D/DejaVuSans.ttf $s gui/fonts/sans-$s.bin full; done
python3 tools/gen-font.py $D/DejaVuSans-Bold.ttf 24 gui/fonts/sans-bold-24.bin full
python3 tools/gen-font.py $D/DejaVuSans.ttf 56 gui/fonts/sans-56.bin ascii
python3 tools/gen-font.py $D/DejaVuSansMono.ttf 16 gui/fonts/mono-16.bin full
