#!/usr/bin/env python3
"""Reset the board, wait for boot, send shell commands and print the replies.

Usage: shell-session.py [--boot-wait S] [--wait S] cmd1 cmd2 ...
"""
import argparse
import time

import serial

ap = argparse.ArgumentParser()
ap.add_argument("--port", default="/dev/ttyACM0")
ap.add_argument("--boot-wait", type=float, default=2.5)
ap.add_argument("--wait", type=float, default=0.8)
ap.add_argument("commands", nargs="*")
args = ap.parse_args()

s = serial.Serial(args.port, 115200, timeout=0.1)
s.dtr = False
s.rts = True
time.sleep(0.1)
s.rts = False


def read_for(seconds):
    buf = b""
    end = time.time() + seconds
    while time.time() < end:
        buf += s.read(4096)
    return buf.decode(errors="replace")


boot = read_for(args.boot_wait)
print(boot[boot.find("p4os 0"):] if "p4os 0" in boot else boot, end="")
for cmd in args.commands:
    s.write((cmd + "\r").encode())
    print(read_for(args.wait), end="")
print()
