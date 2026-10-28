#!/usr/bin/env python3
"""Watch the ESP32-P4 screen through the Imou camera pointed at it.

Commands:
  grab OUT.png            save one full camera frame
  calibrate               find the screen by flashing it white/black and
                          store its bounding box in scripts/cam-roi.json
  check RRGGBB            fill the screen with a colour and verify the
                          camera sees that colour inside the stored box
  shot OUT.png            save the screen region, upright, with the backlight
                          dimmed during capture so the camera does not clip

The RTSP password (Imou "safety code") is read from ~/.config/p4os/imou and
never printed. Camera IP: $P4OS_CAM (default 192.168.88.33).
"""
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import numpy as np
import serial
from PIL import Image

ROOT = Path(__file__).resolve().parent
ROI_FILE = ROOT / "cam-roi.json"
PORT = "/dev/ttyACM0"


def rtsp_url() -> str:
    code = (Path.home() / ".config/p4os/imou").read_text().strip()
    ip = os.environ.get("P4OS_CAM", "192.168.88.33")
    return f"rtsp://admin:{code}@{ip}:554/cam/realmonitor?channel=1&subtype=0"


def grab() -> Image.Image:
    with tempfile.NamedTemporaryFile(suffix=".png") as f:
        # Drop a few frames so the image reflects the current screen state.
        cmd = ["ffmpeg", "-loglevel", "error", "-rtsp_transport", "tcp", "-y",
               "-i", rtsp_url(), "-vf", "select=gte(n\\,5)", "-frames:v", "1", f.name]
        subprocess.run(cmd, check=True, timeout=30)
        return Image.open(f.name).convert("RGB").copy()


def shell(cmd: str, settle: float = 1.5) -> None:
    """Send a p4os shell command without resetting the board.

    Opening the port leaves DTR/RTS both asserted, which the USB-Serial-JTAG
    treats as "run". Changing either line alone triggers a chip reset.
    """
    s = serial.Serial(PORT, 115200, timeout=0.2)
    s.write((cmd + "\r").encode())
    time.sleep(0.3)
    s.read(4096)
    s.close()
    time.sleep(settle)


def calibrate() -> dict:
    # Mid grey rather than white: the camera's auto exposure clips a full
    # white backlit panel and blooms it into the surroundings.
    shell("fill 606060")
    white = np.asarray(grab(), dtype=np.int16)
    shell("fill 000000")
    black = np.asarray(grab(), dtype=np.int16)
    diff = (white - black).sum(axis=2)
    mask = diff > max(60, diff.max() * 0.35)
    ys, xs = np.nonzero(mask)
    if len(xs) < 500:
        raise SystemExit("screen not found: white/black frames look the same")
    # Robust box: ignore stray pixels by taking percentiles horizontally,
    # then keep only the first solid run of rows so the panel's reflection
    # on a glossy table below it is not included.
    x0, x1 = np.percentile(xs, [1, 99]).astype(int)
    rows = mask[:, x0:x1].mean(axis=1) > 0.6
    y0 = int(np.argmax(rows))
    y1 = y0
    while y1 < len(rows) and rows[y1]:
        y1 += 1
    roi = {"x0": int(x0), "y0": int(y0), "x1": int(x1), "y1": int(y1),
           "frame": [int(diff.shape[1]), int(diff.shape[0])]}
    ROI_FILE.write_text(json.dumps(roi, indent=2) + "\n")
    shell("fill off")
    return roi


def crop(img: Image.Image) -> Image.Image:
    r = json.loads(ROI_FILE.read_text())
    return img.crop((r["x0"], r["y0"], r["x1"], r["y1"]))


# Backlight percentage that keeps the panel within the camera's range.
SHOT_BACKLIGHT = 3
# The board stands in landscape with the panel's top edge on the camera's
# left, so captures are rotated clockwise to read upright.
ROTATE_DEGREES = -90


def shot() -> Image.Image:
    shell(f"bl {SHOT_BACKLIGHT}", settle=2.0)
    try:
        img = crop(grab())
    finally:
        shell("bl 100", settle=0.2)
    return img.rotate(ROTATE_DEGREES, expand=True)


def check(hexrgb: str) -> bool:
    shell(f"fill {hexrgb}")
    region = np.asarray(crop(grab()), dtype=np.float32)
    # Use the central half to avoid bezel and reflections at the edges.
    h, w, _ = region.shape
    mean = region[h // 4: 3 * h // 4, w // 4: 3 * w // 4].reshape(-1, 3).mean(axis=0)
    target = np.array([int(hexrgb[i:i + 2], 16) for i in (0, 2, 4)], dtype=np.float32)
    # Compare chromaticity, since camera exposure scales brightness.
    norm = lambda v: v / max(v.sum(), 1.0)
    # The camera over-exposes the panel, so judge by the dominant channel(s):
    # every channel that is "on" in the target must clearly lead the others.
    on = target > 0
    if on.all() or not on.any():
        ok = mean.max() > 40 and (mean.max() - mean.min()) < 0.35 * mean.max()
    else:
        ok = mean[on].min() > mean[~on].max() * 1.25
    print(f"expected {hexrgb} seen mean RGB {mean.round(0).tolist()} -> {'OK' if ok else 'MISMATCH'}")
    return bool(ok)


def main() -> None:
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "grab":
        grab().save(sys.argv[2])
    elif cmd == "calibrate":
        print(calibrate())
    elif cmd == "check":
        sys.exit(0 if check(sys.argv[2]) else 1)
    elif cmd == "shot":
        shot().save(sys.argv[2])
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main()
