#!/usr/bin/env python3
"""Capture the p4os screen from the board's own framebuffer over serial.

Usage:
  screenshot.py [-o out.png] [--mode fast|raw]
  screenshot.py -o now.png --expect ref.png [--tolerance N] [--max-diff PCT]
  screenshot.py --input a.png --expect b.png [--tolerance N] [--max-diff PCT]

Sends the shell command `screenshot fast|raw`, decodes the framed reply
(format in media/src/shot.rs), checks its CRC and writes a PNG. Retries
once, exits non-zero on failure. The port is opened without touching
DTR/RTS: changing either line alone resets the chip.

Takes the board lock (scripts/board-lock.sh) unless P4OS_BOARD_LOCKED=1
says the caller already holds it.

--expect compares with a reference image: a pixel differs when any channel
is off by more than --tolerance (0-255); the check fails when more than
--max-diff percent of pixels differ.
"""
import argparse
import os
import struct
import sys
import time
import zlib
from pathlib import Path

import numpy as np

START = b"\x1b[P4SHOT"
END = b"\x1b[P4END]"
HEADER = struct.Struct("<HHBBHII")  # width, height, format, encoding, reserved, payload_len, pixels_len
FORMAT_RGB565 = 1
RAW, RLE = 0, 1
ROOT = Path(__file__).resolve().parent


class FrameError(Exception):
    """The frame is corrupt or incomplete; worth another attempt."""


class Refused(Exception):
    """The board declined to capture (e.g. a passcode is being typed)."""


def rle_decode(data: bytes, width: int, count: int) -> np.ndarray:
    """Decode the RLE pixel stream (see media/src/shot.rs) into uint16."""
    out = np.empty(count, dtype=np.uint16)
    n = at = 0
    size = len(data)
    while at < size:
        t = data[at]
        at += 1
        length = (t & 63) + 1
        if length == 64:
            if at + 2 > size:
                raise FrameError("truncated length")
            length = data[at] | data[at + 1] << 8
            at += 2
        if n + length > count:
            raise FrameError("stream decodes past the image size")
        op = t >> 6
        if op == 0:
            end = at + 2 * length
            if end > size:
                raise FrameError("truncated literal")
            out[n:n + length] = np.frombuffer(data[at:end], dtype="<u2")
            at = end
        elif op == 1:
            if at + 2 > size:
                raise FrameError("truncated run")
            out[n:n + length] = data[at] | data[at + 1] << 8
            at += 2
        elif op == 2:
            if n < width:
                raise FrameError("'up' token in the first row")
            # Copies may overlap their source when length > width.
            for i in range(0, length, width):
                k = min(width, length - i)
                out[n + i:n + i + k] = out[n + i - width:n + i - width + k]
        else:
            raise FrameError("bad token")
        n += length
    if n != count:
        raise FrameError(f"stream ended after {n} of {count} pixels")
    return out


def rgb565_to_rgb888(pixels: np.ndarray, width: int, height: int) -> np.ndarray:
    """Expand with bit replication, matching the device's PNG writer."""
    p = pixels.astype(np.uint32).reshape(height, width)
    r, g, b = (p >> 11) & 0x1F, (p >> 5) & 0x3F, p & 0x1F
    rgb = np.stack([(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)], axis=-1)
    return rgb.astype(np.uint8)


def parse_frame(buf: bytes):
    """Find and decode a frame in `buf`, which may hold log lines around it.

    Returns None while the frame is still incomplete, otherwise
    (rgb array of shape (h, w, 3), header dict).
    """
    start = buf.find(START)
    if start < 0:
        return None
    at = start + len(START)
    if len(buf) < at + HEADER.size:
        return None
    hb = buf[at:at + HEADER.size]
    width, height, fmt, enc, _, payload_len, pixels_len = HEADER.unpack(hb)
    if fmt != FORMAT_RGB565 or enc not in (RAW, RLE) or not width or not height \
            or pixels_len != width * height * 2 or (enc == RAW and payload_len != pixels_len) \
            or payload_len > 64 << 20:
        raise FrameError("bad header")
    at += HEADER.size
    end = at + payload_len + 4 + len(END)
    if len(buf) < end:
        return None
    payload = buf[at:at + payload_len]
    crc, = struct.unpack("<I", buf[at + payload_len:at + payload_len + 4])
    if buf[end - len(END):end] != END:
        raise FrameError("end marker missing (interleaved output?)")
    if zlib.crc32(hb + payload) != crc:
        raise FrameError("CRC mismatch")
    if enc == RAW:
        pixels = np.frombuffer(payload, dtype="<u2").astype(np.uint16)
    else:
        pixels = rle_decode(payload, width, width * height)
    info = {"width": width, "height": height, "encoding": "raw" if enc == RAW else "rle",
            "payload": payload_len, "tail": buf[end:]}
    return rgb565_to_rgb888(pixels, width, height), info


def capture_once(port: str, mode: str, timeout: float):
    import serial

    # Opening asserts DTR and RTS together, which the chip treats as "run".
    s = serial.Serial(port, 115200, timeout=0.05)
    try:
        s.reset_input_buffer()
        started = time.time()
        s.write(f"screenshot {mode}\r".encode())
        buf = b""
        deadline = started + timeout
        while time.time() < deadline:
            chunk = s.read(65536)
            if chunk:
                buf += chunk
                frame = parse_frame(buf)
                if frame is not None:
                    rgb, info = frame
                    elapsed = time.time() - started
                    # Pick up the board's timing line that follows the frame.
                    tail = info.pop("tail")
                    line_end = time.time() + 0.5
                    while b"screenshot:" not in tail or b"\n" not in tail.split(b"screenshot:", 1)[1]:
                        if time.time() > line_end:
                            break
                        tail += s.read(4096)
                    for line in tail.decode(errors="replace").splitlines():
                        if line.startswith("screenshot:"):
                            info["board"] = line[len("screenshot:"):].strip()
                    info["seconds"] = elapsed
                    return rgb, info
            elif START not in buf:
                text = buf.decode(errors="replace")
                for line in text.splitlines():
                    if line.startswith("screenshot:"):
                        msg = line[len("screenshot:"):].strip()
                        if "refused" in msg:
                            raise Refused(msg)
                        raise FrameError(msg)
        raise FrameError("timed out" + (" mid-frame" if START in buf else " (no frame seen)"))
    finally:
        s.close()


def capture(port: str, mode: str, timeout: float, attempts: int = 2):
    last = None
    for attempt in range(attempts):
        try:
            return capture_once(port, mode, timeout)
        except FrameError as e:
            last = e
            print(f"screenshot: attempt {attempt + 1} failed: {e}", file=sys.stderr)
            time.sleep(0.5)
    raise last


def load_rgb(path) -> np.ndarray:
    from PIL import Image
    return np.asarray(Image.open(path).convert("RGB"))


def diff_percent(a: np.ndarray, b: np.ndarray, tolerance: int) -> float:
    """Percentage of pixels where any channel differs by more than `tolerance`."""
    if a.shape != b.shape:
        raise ValueError(f"size differs: {a.shape[1]}x{a.shape[0]} vs {b.shape[1]}x{b.shape[0]}")
    delta = np.abs(a.astype(np.int16) - b.astype(np.int16)).max(axis=-1)
    return 100.0 * np.count_nonzero(delta > tolerance) / delta.size


def ensure_board_lock() -> None:
    if os.environ.get("P4OS_BOARD_LOCKED") == "1":
        return
    lock = ROOT / "board-lock.sh"
    os.execv(str(lock), [str(lock), sys.executable, str(Path(__file__).resolve())] + sys.argv[1:])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("-o", "--output", default="screenshot.png")
    ap.add_argument("--mode", choices=["fast", "raw"], default="fast")
    ap.add_argument("--port", default=os.environ.get("PORT", "/dev/ttyACM0"))
    ap.add_argument("--timeout", type=float, default=20.0, help="seconds per attempt")
    ap.add_argument("--input", help="compare this image instead of capturing")
    ap.add_argument("--expect", help="reference image to compare against")
    ap.add_argument("--tolerance", type=int, default=0, help="per-channel difference allowed (0-255)")
    ap.add_argument("--max-diff", type=float, default=0.0, help="percent of differing pixels allowed")
    args = ap.parse_args()

    if args.input:
        rgb = load_rgb(args.input)
    else:
        ensure_board_lock()
        try:
            rgb, info = capture(args.port, args.mode, args.timeout)
        except Refused as e:
            print(f"screenshot: board refused: {e}", file=sys.stderr)
            return 1
        except Exception as e:  # serial errors included: report, do not trace
            print(f"screenshot: failed: {e}", file=sys.stderr)
            return 1
        from PIL import Image
        Image.fromarray(rgb, "RGB").save(args.output)
        print(f"{args.output}: {info['width']}x{info['height']} {info['encoding']} {info['payload']} bytes "
              f"in {info['seconds']:.2f} s (board: {info.get('board', 'n/a')})")

    if args.expect:
        try:
            pct = diff_percent(rgb, load_rgb(args.expect), args.tolerance)
        except (OSError, ValueError) as e:
            print(f"compare: {e}", file=sys.stderr)
            return 1
        ok = pct <= args.max_diff
        print(f"compare: {pct:.3f}% of pixels differ (tolerance {args.tolerance}, "
              f"allowed {args.max_diff}%) -> {'OK' if ok else 'MISMATCH'}")
        return 0 if ok else 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
