#!/usr/bin/env python3
"""Save the Wi-Fi network stored in ~/.config/p4os/wifi (ssid=..., password=...)
on the board with `wifi save`, so p4os also rejoins it by itself at every boot,
and wait for the first connection attempt. Usage: wifi-connect.py [seconds].

The password is never printed: the shell echoes input, so all output is
redacted before display. The serial port is opened the same way as
board-ui.py, which does not reset the board.
"""
import sys
import time
from pathlib import Path

import serial

cfg = dict(line.split("=", 1) for line in (Path.home() / ".config/p4os/wifi").read_text().splitlines() if "=" in line)
ssid, password = cfg["ssid"].strip(), cfg["password"].strip()


def redact(data: bytes) -> str:
    text = data.decode(errors="replace")
    return text.replace(password, "[redacted]") if password else text


s = serial.Serial("/dev/ttyACM0", 115200, timeout=0.2)
time.sleep(0.3)
s.read(65536)
s.write(f"wifi save {ssid} {password}\r".encode())
out = b""
end = time.time() + float(sys.argv[1] if len(sys.argv) > 1 else 60)

def finished(data: bytes) -> bool:
    # The shell prints its prompt again once the command has finished.
    _, sep, after = data.partition(b"wifi save")
    return bool(sep) and b"p4os> " in after


while time.time() < end and not finished(out):
    out += s.read(4096)
text = redact(out)
print(text)
sys.exit(0 if "wifi: up" in text else 1)
