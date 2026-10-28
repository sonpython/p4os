#!/usr/bin/env python3
"""Reset the board over USB-Serial-JTAG and print console output.

Usage: monitor.py [seconds] [port]. Runs until Ctrl-C when seconds is 0.
"""
import sys
import time

import serial

duration = float(sys.argv[1]) if len(sys.argv) > 1 else 0
port = sys.argv[2] if len(sys.argv) > 2 else "/dev/ttyACM0"

s = serial.Serial(port, 115200, timeout=0.1)
s.dtr = False
s.rts = True
time.sleep(0.1)
s.rts = False

start = time.time()
try:
    while duration == 0 or time.time() - start < duration:
        data = s.read(4096)
        if data:
            sys.stdout.write(data.decode(errors="replace"))
            sys.stdout.flush()
except KeyboardInterrupt:
    pass
