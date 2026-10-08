"""Draws icons/source.png (1024x1024): a white anchor on a blue rounded
square. `cargo tauri icon icons/source.png -o icons` makes the platform set
from it. Standard library only, so the icon's origin is this file."""
import math, struct, zlib, pathlib

N = 1024
BG = (29, 78, 216)       # blue
FG = (255, 255, 255)

def inside_rounded(x, y, r=200, pad=40):
    lo, hi = pad, N - pad
    cx = min(max(x, lo + r), hi - r)
    cy = min(max(y, lo + r), hi - r)
    return lo <= x <= hi and lo <= y <= hi and (x - cx) ** 2 + (y - cy) ** 2 <= r * r

def seg_dist(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))

def anchor(x, y):
    w = 46  # stroke half-width
    # ring at the top
    if abs(math.hypot(x - 512, y - 250) - 70) <= w * 0.75:
        return True
    # shank
    if seg_dist(x, y, 512, 320, 512, 790) <= w:
        return True
    # stock (crossbar)
    if seg_dist(x, y, 380, 400, 644, 400) <= w * 0.85:
        return True
    # the arms: lower arc
    d = math.hypot(x - 512, y - 560)
    if abs(d - 250) <= w and y >= 560:
        return True
    # flukes: short barbs at the arc's ends
    for sx in (-1, 1):
        if seg_dist(x, y, 512 + sx * 250, 560, 512 + sx * 300, 620) <= w * 0.8:
            return True
        if seg_dist(x, y, 512 + sx * 250, 560, 512 + sx * 200, 500) <= w * 0.8:
            return True
    return False

rows = []
for y in range(N):
    row = bytearray([0])
    for x in range(N):
        if not inside_rounded(x, y):
            row += bytes((0, 0, 0, 0))
        elif anchor(x, y):
            row += bytes(FG + (255,))
        else:
            row += bytes(BG + (255,))
    rows.append(bytes(row))

def chunk(t, d):
    c = t + d
    return struct.pack(">I", len(d)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", N, N, 8, 6, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b"")
out = pathlib.Path(__file__).with_name("source.png")
out.write_bytes(png)
print(out, len(png), "bytes")
