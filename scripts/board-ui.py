#!/usr/bin/env python3
"""Drive the p4os GUI on the board from the shell and capture the result
with the camera. Each argument is a shell command (e.g. "tap 80 132") or
"shot NAME" to save docs-free captures under out/board/NAME.png.

Example: board-ui.py "tap 80 132" "shot calculator"
"""
import subprocess
import sys
import time
from pathlib import Path

import serial

ROOT = Path(__file__).resolve().parent
OUT = ROOT.parent / "out" / "board"


def send(cmd: str) -> None:
    s = serial.Serial("/dev/ttyACM0", 115200, timeout=0.2)
    s.write((cmd + "\r").encode())
    time.sleep(0.4)
    s.close()


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for arg in sys.argv[1:]:
        if arg.startswith("shot "):
            path = OUT / f"{arg[5:].strip()}.png"
            subprocess.run([sys.executable, str(ROOT / "cam.py"), "shot", str(path)], check=True)
            print(path)
        elif arg.startswith("wait "):
            time.sleep(float(arg[5:]))
        else:
            send(arg)
            time.sleep(1.0)


if __name__ == "__main__":
    main()
