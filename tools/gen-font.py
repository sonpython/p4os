#!/usr/bin/env python3
"""Rasterize a TrueType font into the p4os bitmap font format (P4F1).

Layout (little endian):
  header : b"P4F1", u16 size_px, u16 line_height, i16 ascent, u16 glyph_count
  glyphs : glyph_count x 16 bytes, sorted by codepoint:
           u32 codepoint, u16 advance, u8 width, u8 height,
           i8 x_offset, i8 y_offset (top of bitmap relative to baseline, down positive),
           u32 bitmap_offset, u16 reserved
  bitmap : 4-bit alpha, row-major, each row padded to whole bytes, high nibble first

Usage: gen-font.py FONT.ttf SIZE OUT.bin [ascii|full]
"""
import struct
import sys

from PIL import Image, ImageDraw, ImageFont

VIETNAMESE = [0x102, 0x103, 0x110, 0x111, 0x128, 0x129, 0x168, 0x169,
              0x1A0, 0x1A1, 0x1AF, 0x1B0] + list(range(0x1EA0, 0x1EFA))
SYMBOLS = [0x2022, 0x2026, 0x2190, 0x2191, 0x2192, 0x2193, 0x00D7, 0x00F7,
           0x221A, 0x2212, 0x25B6, 0x25A0, 0x25CF, 0x2713, 0x2715, 0x00B0,
           0x232B, 0x23CE, 0x21BB, 0x2630, 0x266A, 0x23F8, 0x23F9, 0x25C0,
           0x2302, 0x2699, 0x2605, 0x2606, 0x25B2, 0x25BC]


def charset(kind):
    cps = list(range(0x20, 0x7F))
    if kind == "full":
        cps += list(range(0xA0, 0x100)) + VIETNAMESE + SYMBOLS
    return sorted(set(cps))


def main():
    path, size, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    kind = sys.argv[4] if len(sys.argv) > 4 else "full"
    font = ImageFont.truetype(path, size)
    ascent, descent = font.getmetrics()
    line_height = ascent + descent

    glyphs = []
    bitmap = bytearray()
    for cp in charset(kind):
        ch = chr(cp)
        # Skip codepoints the font does not cover (rendered as .notdef box).
        if cp != 0x20 and font.getmask(ch).getbbox() is None:
            continue
        advance = round(font.getlength(ch))
        bbox = font.getbbox(ch)  # relative to the drawing origin (top of ascent)
        if bbox is None or bbox[2] <= bbox[0] or bbox[3] <= bbox[1]:
            glyphs.append((cp, advance, 0, 0, 0, 0, len(bitmap)))
            continue
        x0, y0, x1, y1 = bbox
        w, h = x1 - x0, y1 - y0
        img = Image.new("L", (w, h), 0)
        ImageDraw.Draw(img).text((-x0, -y0), ch, font=font, fill=255)
        offset = len(bitmap)
        px = img.load()
        for y in range(h):
            row = bytearray((w + 1) // 2)
            for x in range(w):
                a = (px[x, y] + 8) // 17  # 0..15
                if x % 2 == 0:
                    row[x // 2] |= a << 4
                else:
                    row[x // 2] |= a
            bitmap += row
        # y offset relative to the baseline (ascent from the top).
        glyphs.append((cp, advance, w, h, x0, y0 - ascent, offset))

    with open(out, "wb") as f:
        f.write(b"P4F1")
        f.write(struct.pack("<HHhH", size, line_height, ascent, len(glyphs)))
        for cp, adv, w, h, xo, yo, off in glyphs:
            f.write(struct.pack("<IHBBbbIH", cp, adv, w, h, xo, yo, off, 0))
        f.write(bitmap)
    print(f"{out}: {len(glyphs)} glyphs, {len(bitmap)} bitmap bytes")


if __name__ == "__main__":
    main()
