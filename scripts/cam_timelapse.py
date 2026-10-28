#!/usr/bin/env python3
"""Record a camera timelapse of the board while work happens, then encode it.

  cam_timelapse.py record [INTERVAL_S]   grab a full frame every interval into
                                         out/timelapse-live/ (Ctrl-C to stop)
  cam_timelapse.py encode OUT.mp4        stamp each frame with its capture time
                                         and encode at 8 fps

The camera does not keep its own recordings reachable over RTSP, so the
timelapse only covers the time this recorder was running.
"""
import datetime as dt
import subprocess
import sys
import time
from pathlib import Path

from PIL import ImageDraw, ImageFont

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cam  # noqa: E402

DIR = Path(__file__).resolve().parent.parent / "out" / "timelapse-live"
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"


def record(interval: float) -> None:
    DIR.mkdir(parents=True, exist_ok=True)
    while True:
        stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
        try:
            cam.grab().save(DIR / f"{stamp}.jpg", quality=85)
        except Exception as e:  # keep recording through camera hiccups
            print(f"{stamp}: grab failed: {e}", file=sys.stderr)
        time.sleep(interval)


def encode(out: str) -> None:
    frames = sorted(DIR.glob("*.jpg"))
    if not frames:
        raise SystemExit("no frames recorded")
    tmp = DIR / "stamped"
    tmp.mkdir(exist_ok=True)
    font = ImageFont.truetype(FONT, 36)
    for i, f in enumerate(frames):
        img = cam.Image.open(f).convert("RGB")
        t = dt.datetime.strptime(f.stem, "%Y%m%d-%H%M%S").strftime("%H:%M:%S")
        ImageDraw.Draw(img).text((30, 30), t, font=font, fill=(255, 220, 0))
        img.save(tmp / f"{i:05d}.jpg", quality=85)
    subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-framerate", "8", "-i", str(tmp / "%05d.jpg"),
                    "-c:v", "libx264", "-pix_fmt", "yuv420p", out], check=True)
    print(out)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "record":
        record(float(sys.argv[2]) if len(sys.argv) > 2 else 20)
    elif len(sys.argv) > 2 and sys.argv[1] == "encode":
        encode(sys.argv[2])
    else:
        print(__doc__)
