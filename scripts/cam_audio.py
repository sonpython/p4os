#!/usr/bin/env python3
"""Check that the board's speaker plays a tone, using the Imou camera's
microphone. Records the RTSP audio track while `tone FREQ` plays and
compares the energy at FREQ against a silent recording.

Usage: cam_audio.py [FREQ]   (exit 0 if the tone is heard)
"""
import os
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import numpy as np
import serial

FREQ = int(sys.argv[1]) if len(sys.argv) > 1 else 1000
CODE = (Path.home() / ".config/p4os/imou").read_text().strip()
IP = os.environ.get("P4OS_CAM", "192.168.88.33")
URL = f"rtsp://admin:{CODE}@{IP}:554/cam/realmonitor?channel=1&subtype=0"
RATE = 16000


def record(seconds: float) -> np.ndarray:
    with tempfile.NamedTemporaryFile(suffix=".raw") as f:
        subprocess.run(["ffmpeg", "-loglevel", "error", "-rtsp_transport", "tcp", "-y", "-i", URL,
                        "-vn", "-t", str(seconds), "-ac", "1", "-ar", str(RATE), "-f", "s16le", f.name],
                       check=True, timeout=seconds + 30)
        return np.fromfile(f.name, dtype=np.int16).astype(np.float32)


def band_ratio(x: np.ndarray, freq: int) -> float:
    """Energy within +-30 Hz of freq relative to the 300-4000 Hz band."""
    spec = np.abs(np.fft.rfft(x * np.hanning(len(x)))) ** 2
    f = np.fft.rfftfreq(len(x), 1 / RATE)
    tone = spec[(f > freq - 30) & (f < freq + 30)].sum()
    band = spec[(f > 300) & (f < 4000)].sum()
    return float(tone / max(band, 1e-9))


def tone() -> None:
    time.sleep(1.5)
    s = serial.Serial("/dev/ttyACM0", 115200, timeout=0.2)
    s.write(f"tone {FREQ}\r".encode())
    s.close()


def main() -> None:
    quiet = band_ratio(record(3), FREQ)
    threading.Thread(target=tone).start()
    loud = band_ratio(record(5)[int(2 * RATE):], FREQ)
    heard = loud > 0.2 and loud > 5 * quiet
    print(f"{FREQ} Hz share of 300-4000 Hz energy: silent {quiet:.3f}, playing {loud:.3f} -> {'HEARD' if heard else 'not heard'}")
    sys.exit(0 if heard else 1)


if __name__ == "__main__":
    main()
