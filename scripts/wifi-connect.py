#!/usr/bin/env python3
"""Join the Wi-Fi network stored in ~/.config/p4os/wifi (ssid=..., password=...)
by sending `wifi connect` to the p4os shell. The password is never printed:
the shell echoes input, so all output is redacted before display.
"""
import sys
import time
from pathlib import Path

import serial

cfg = dict(line.split("=", 1) for line in (Path.home() / ".config/p4os/wifi").read_text().splitlines() if "=" in line)
ssid, password = cfg["ssid"].strip(), cfg["password"].strip()

s = serial.Serial("/dev/ttyACM0", 115200, timeout=0.2)
time.sleep(0.3)
s.read(65536)
s.write(f"wifi connect {ssid} {password}\r".encode())
out = b""
end = time.time() + float(sys.argv[1] if len(sys.argv) > 1 else 30)
while time.time() < end:
    out += s.read(4096)
    if b"wifi: up" in out or b"wifi: " in out.split(b"\n")[-2:][0] and b"failed" in out:
        pass
text = out.decode(errors="replace").replace(password, "[redacted]")
print(text)
sys.exit(0 if "wifi: up" in text else 1)
