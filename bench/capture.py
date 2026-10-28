#!/usr/bin/env python3
"""Reset the board, optionally send a shell command, and print console
output until a marker line appears (or a timeout expires).

Usage: capture.py [--send CMD] [--boot-wait S] [--until TEXT] [--timeout S]
Exits non-zero when the marker never shows up.
"""
import argparse
import sys
import time

import serial

ap = argparse.ArgumentParser()
ap.add_argument("--port", default="/dev/ttyACM0")
ap.add_argument("--send", help="shell command to send after --boot-wait")
ap.add_argument("--boot-wait", type=float, default=6.0)
ap.add_argument("--until", default="bench: done")
ap.add_argument("--timeout", type=float, default=60.0)
args = ap.parse_args()

s = serial.Serial(args.port, 115200, timeout=0.1)
# Pulse RTS with DTR low: USB-Serial-JTAG treats this as a chip reset.
s.dtr = False
s.rts = True
time.sleep(0.1)
s.rts = False

buf = b""
start = time.time()
sent = args.send is None
while time.time() - start < args.timeout:
    buf += s.read(4096)
    if not sent and time.time() - start >= args.boot_wait:
        s.write((args.send + "\r").encode())
        sent = True
    if args.until.encode() in buf:
        buf += s.read(4096)
        break

text = buf.decode(errors="replace")
sys.stdout.write(text)
sys.exit(0 if args.until in text else 1)
