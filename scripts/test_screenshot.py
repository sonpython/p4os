#!/usr/bin/env python3
"""Host tests for the screenshot frame protocol and codec.

Run: python3 -m unittest discover -s scripts -p 'test_*.py'
The sample vector is shared with media/src/shot.rs, so both sides of the
serial link are held to the same format.
"""
import struct
import sys
import unittest
import zlib
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import screenshot as ss  # noqa: E402

SAMPLE = [0xF800, 0xF800, 0xF800, 0x07E0,
          0xF800, 0xF800, 0xF800, 0x001F,
          0x1234, 0x5678, 0x5678, 0x5678]
SAMPLE_RLE = bytes([0x42, 0x00, 0xF8, 0x00, 0xE0, 0x07, 0x82,
                    0x01, 0x1F, 0x00, 0x34, 0x12, 0x42, 0x78, 0x56])


def frame(width, height, enc, payload, crc=None):
    header = ss.HEADER.pack(width, height, ss.FORMAT_RGB565, enc, 0, len(payload), width * height * 2)
    if crc is None:
        crc = zlib.crc32(header + payload)
    return ss.START + header + payload + struct.pack("<I", crc) + ss.END


def rle_encode(pixels, width):
    """Reference encoder (same greedy rules as the Rust one) for round trips."""
    out = bytearray()
    n, i, lit = len(pixels), 0, 0

    def token(op, length):
        if length <= 63:
            out.append(op << 6 | (length - 1))
        else:
            out.append(op << 6 | 63)
            out.extend(struct.pack("<H", length))

    def flush(a, b):
        if b > a:
            token(0, b - a)
            for p in pixels[a:b]:
                out.extend(struct.pack("<H", p))

    while i < n:
        limit = min(n - i, 65535)
        v = pixels[i]
        run = 0
        while run < limit and pixels[i + run] == v:
            run += 1
        up = 0
        if i >= width:
            while up < limit and pixels[i + up] == pixels[i + up - width]:
                up += 1
        if up >= 2 and up >= run:
            flush(lit, i)
            token(2, up)
            i += up
            lit = i
        elif run >= 2:
            flush(lit, i)
            token(1, run)
            out.extend(struct.pack("<H", v))
            i += run
            lit = i
        else:
            i += 1
            if i - lit == 65535:
                flush(lit, i)
                lit = i
    flush(lit, n)
    return bytes(out)


class Codec(unittest.TestCase):
    def test_shared_vector_decodes(self):
        self.assertEqual(ss.rle_decode(SAMPLE_RLE, 4, 12).tolist(), SAMPLE)

    def test_reference_encoder_matches_vector(self):
        self.assertEqual(rle_encode(SAMPLE, 4), SAMPLE_RLE)

    def test_round_trip_ui_like_image(self):
        rng = np.random.default_rng(1)
        w, h = 120, 90
        img = np.zeros((h, w), dtype=np.uint16)
        img[10:40, 20:80] = 0x39E7
        img[50:, :] = 0x0841
        noise = rng.random((h, w)) < 0.02
        img[noise] = rng.integers(0, 65536, noise.sum())
        flat = img.ravel().tolist()
        enc = rle_encode(flat, w)
        self.assertLess(len(enc), len(flat))
        self.assertEqual(ss.rle_decode(enc, w, w * h).tolist(), flat)

    def test_up_longer_than_a_row_overlaps(self):
        # Two identical rows after the first: one 'up' token spanning 2 rows.
        enc = bytes([0x01, 1, 0, 2, 0, 0x83])
        self.assertEqual(ss.rle_decode(enc, 2, 6).tolist(), [1, 2, 1, 2, 1, 2])

    def test_long_length_form(self):
        enc = bytes([0x7F]) + struct.pack("<H", 1000) + b"\x34\x12"
        self.assertEqual(ss.rle_decode(enc, 10, 1000).tolist(), [0x1234] * 1000)

    def test_corrupt_streams_rejected(self):
        for data, count in [(b"\x80", 4), (b"\xC0", 4), (b"\x41\x00", 4), (b"\x44\x00\x00", 4), (b"\x42\x00\x00", 4)]:
            with self.assertRaises(ss.FrameError, msg=data.hex()):
                ss.rle_decode(data, 4, count)

    def test_rgb_expansion(self):
        rgb = ss.rgb565_to_rgb888(np.array([0xFFFF, 0x0000, 0xF800, 0x07E0], dtype=np.uint16), 4, 1)
        self.assertEqual(rgb.tolist(), [[[255, 255, 255], [0, 0, 0], [255, 0, 0], [0, 255, 0]]])


class Frames(unittest.TestCase):
    def test_frame_between_log_lines(self):
        buf = b"p4os> screenshot\r\nnet: dhcp ok\r\n\n" + frame(4, 3, ss.RLE, SAMPLE_RLE) + b"\nscreenshot: 4x3 rle\r\n"
        rgb, info = ss.parse_frame(buf)
        self.assertEqual(rgb.shape, (3, 4, 3))
        self.assertEqual(rgb[0, 0].tolist(), [255, 0, 0])
        self.assertEqual(info["encoding"], "rle")
        self.assertTrue(info["tail"].startswith(b"\nscreenshot:"))

    def test_raw_frame(self):
        payload = struct.pack("<12H", *SAMPLE)
        rgb, info = ss.parse_frame(frame(4, 3, ss.RAW, payload))
        self.assertEqual(info["encoding"], "raw")
        self.assertEqual(rgb[2, 0].tolist(), ss.rgb565_to_rgb888(np.array([0x1234], dtype=np.uint16), 1, 1)[0, 0].tolist())

    def test_incomplete_frame_waits(self):
        full = frame(4, 3, ss.RLE, SAMPLE_RLE)
        for cut in (0, 4, 10, len(full) - 1):
            self.assertIsNone(ss.parse_frame(b"log\n" + full[:cut]))

    def test_crc_mismatch(self):
        with self.assertRaises(ss.FrameError):
            ss.parse_frame(frame(4, 3, ss.RLE, SAMPLE_RLE, crc=0))

    def test_interleaved_byte_breaks_frame(self):
        full = bytearray(frame(4, 3, ss.RLE, SAMPLE_RLE))
        full[30:30] = b"X"  # a log byte landing inside the payload
        with self.assertRaises(ss.FrameError):
            ss.parse_frame(bytes(full))

    def test_bad_header(self):
        bad = ss.START + ss.HEADER.pack(4, 3, 9, ss.RLE, 0, 1, 24) + b"\0" * 20
        with self.assertRaises(ss.FrameError):
            ss.parse_frame(bad)


class Compare(unittest.TestCase):
    def test_diff_percent(self):
        a = np.zeros((10, 10, 3), dtype=np.uint8)
        b = a.copy()
        b[0, :5] = 10
        b[1, 0] = 2
        self.assertAlmostEqual(ss.diff_percent(a, b, 0), 6.0)
        self.assertAlmostEqual(ss.diff_percent(a, b, 5), 5.0)
        self.assertAlmostEqual(ss.diff_percent(a, b, 10), 0.0)

    def test_size_mismatch(self):
        with self.assertRaises(ValueError):
            ss.diff_percent(np.zeros((2, 2, 3), np.uint8), np.zeros((2, 3, 3), np.uint8), 0)


if __name__ == "__main__":
    unittest.main()
