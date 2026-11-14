"""Draws kibotos.ico: a closed chest (kibotos = "ark, chest" in Greek).

Pure Python (no imaging library): each size is rendered with 4x4
supersampling and stored as PNG inside the .ico, which Windows Vista and
later read for every size.

    python make_icon.py            # writes kibotos.ico next to this file
"""

import os
import struct
import zlib

SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
SS = 4  # supersampling per axis

# Colors (R, G, B)
BODY_TOP = (37, 99, 235)      # blue 600
BODY_BOTTOM = (29, 78, 216)   # blue 700
LID_TOP = (96, 165, 250)      # blue 400
LID_BOTTOM = (59, 130, 246)   # blue 500
BAND = (30, 58, 138)          # blue 900
CLASP = (245, 158, 11)        # amber 500
CLASP_DARK = (180, 83, 9)     # amber 700
OUTLINE = (23, 37, 84)        # blue 950


def rounded_rect(x, y, x0, y0, x1, y1, r):
    """True if (x, y) lies inside the rounded rectangle."""
    if x < x0 or x > x1 or y < y0 or y > y1:
        return False
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def shade(x, y, size):
    """Color of a point in unit coordinates, or None if transparent."""
    # Thicker outline at small sizes keeps the shape readable.
    o = max(0.035, 1.2 / size)
    # Lid: y 0.16 .. 0.44, body: y 0.40 .. 0.88
    if rounded_rect(x, y, 0.47, 0.34, 0.53, 0.56, 0.02) or rounded_rect(x, y, 0.42, 0.36, 0.58, 0.54, 0.035):
        inner = rounded_rect(x, y, 0.42 + o, 0.36 + o, 0.58 - o, 0.54 - o, 0.02)
        return CLASP if inner else CLASP_DARK
    if rounded_rect(x, y, 0.08, 0.40, 0.92, 0.88, 0.07):
        if not rounded_rect(x, y, 0.08 + o, 0.40 + o, 0.92 - o, 0.88 - o, 0.06):
            return OUTLINE
        if y < 0.40 + o + 0.04:
            return BAND
        return lerp(BODY_TOP, BODY_BOTTOM, (y - 0.40) / 0.48)
    if rounded_rect(x, y, 0.10, 0.16, 0.90, 0.44, 0.10):
        if not rounded_rect(x, y, 0.10 + o, 0.16 + o, 0.90 - o, 0.44 - o, 0.09):
            return OUTLINE
        return lerp(LID_TOP, LID_BOTTOM, (y - 0.16) / 0.28)
    return None


def render(size):
    rows = []
    n = SS * SS
    for py in range(size):
        row = bytearray([0])  # PNG filter: none
        for px in range(size):
            r = g = b = a = 0
            for sy in range(SS):
                for sx in range(SS):
                    c = shade((px + (sx + 0.5) / SS) / size, (py + (sy + 0.5) / SS) / size, size)
                    if c is not None:
                        r += c[0]
                        g += c[1]
                        b += c[2]
                        a += 1
            if a:
                row += bytes((r // a, g // a, b // a, 255 * a // n))
            else:
                row += bytes(4)
        rows.append(bytes(row))
    return png(size, size, b"".join(rows))


def png(w, h, raw):
    def chunk(kind, data):
        c = kind + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def main():
    images = [render(s) for s in SIZES]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    for s, data in zip(SIZES, images):
        dim = 0 if s >= 256 else s
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "kibotos.ico")
    with open(out, "wb") as f:
        f.write(header + entries + b"".join(images))
    print(f"wrote {out} ({os.path.getsize(out)} bytes)")


if __name__ == "__main__":
    main()
