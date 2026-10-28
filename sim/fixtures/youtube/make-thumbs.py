#!/usr/bin/env python3
"""Generate the placeholder video thumbnails (320x180 JPEG, like YouTube's
mqdefault) used by the simulator screenshots. Synthetic art only: no real
YouTube content. Re-run after adding video IDs to the JSON fixtures."""

import colorsys
import json
import os
import zlib

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(HERE, "..", "..", "..", "apps", "src", "youtube", "fixtures")
OUT = os.path.join(HERE, "thumbs")
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"


def video_ids():
    ids = []
    for name in ("trending.json", "search.json", "videos.json"):
        with open(os.path.join(FIXTURES, name)) as f:
            for item in json.load(f)["items"]:
                vid = item["id"]["videoId"] if isinstance(item["id"], dict) else item["id"]
                if vid not in ids:
                    ids.append(vid)
    # Channel avatars in the subscriptions fixture.
    ids += ["p4Os_ChanAAA", "p4Os_ChanCCC", "p4Os_ChanGGG"]
    return ids


def thumb(vid):
    seed = zlib.crc32(vid.encode())
    hue = (seed % 360) / 360.0
    top = tuple(int(c * 255) for c in colorsys.hsv_to_rgb(hue, 0.65, 0.85))
    bottom = tuple(int(c * 255) for c in colorsys.hsv_to_rgb((hue + 0.12) % 1, 0.8, 0.35))
    img = Image.new("RGB", (320, 180))
    d = ImageDraw.Draw(img)
    for y in range(180):
        t = y / 179
        d.line([(0, y), (320, y)], fill=tuple(int(a + (b - a) * t) for a, b in zip(top, bottom)))
    # A few circles so thumbnails look different from each other.
    for i in range(3):
        r = 30 + (seed >> (i * 5)) % 60
        x = (seed >> (i * 7)) % 320
        y = (seed >> (i * 3 + 2)) % 180
        d.ellipse([x - r, y - r, x + r, y + r], outline=(255, 255, 255), width=3)
    font = ImageFont.truetype(FONT, 26)
    label = vid.replace("p4Os_", "")
    w = d.textlength(label, font=font)
    d.rounded_rectangle([160 - w / 2 - 12, 66, 160 + w / 2 + 12, 112], 10, fill=(0, 0, 0))
    d.text((160 - w / 2, 74), label, font=font, fill=(255, 255, 255))
    img.save(os.path.join(OUT, vid + ".jpg"), quality=70, optimize=True)


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    for vid in video_ids():
        thumb(vid)
