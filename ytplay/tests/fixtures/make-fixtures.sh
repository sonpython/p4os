#!/usr/bin/env bash
# Regenerate the tiny synthetic test streams (no downloaded media).
# pattern.mp4: 2 s of testsrc2 at 128x72, H.264 Main (CABAC, B-frames,
#   weighted prediction) in DASH fMP4 with a sidx, like YouTube's itags 160/133.
# tone.webm: 1 s of 440 Hz (left) / 660 Hz (right) sine in Opus/WebM, like
#   YouTube's itags 249/250.
# After regenerating, update the frame CRCs in ytplay/src/video.rs with:
#   ffmpeg -v error -i pattern.mp4 -f rawvideo -pix_fmt yuv420p - | python3 -c \
#     'import sys,zlib; d=sys.stdin.buffer.read(); n=128*72*3//2; print([hex(zlib.crc32(d[i:i+n])) for i in range(0,len(d),n)])'
set -euo pipefail
cd "$(dirname "$0")"
ffmpeg -v error -y -f lavfi -i "testsrc2=size=128x72:rate=25" -t 2 -c:v libx264 -profile:v main -preset medium \
  -x264-params "keyint=25:min-keyint=25:bframes=2:weightp=2:ref=3" -pix_fmt yuv420p \
  -movflags +dash+global_sidx -f mp4 pattern.mp4
ffmpeg -v error -y -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" \
  -f lavfi -i "sine=frequency=660:sample_rate=48000:duration=1" \
  -filter_complex "[0][1]amerge=inputs=2,volume=0.5" -c:a libopus -b:a 64k -f webm tone.webm
