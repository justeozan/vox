#!/usr/bin/env python3
"""Generate the Vox app icon (1024x1024 PNG) with only the Python stdlib.

Dark rounded square + capsule waveform bars fading green -> violet.
Output: scripts/icon-src.png (feed to `npx tauri icon`).
"""
import math
import os
import struct
import zlib

W = H = 1024
px = bytearray(W * H * 4)  # RGBA, starts fully transparent

R = 232          # container corner radius
BG = (14, 14, 18)
BORDER = (255, 255, 255, 26)


def blend(i, r, g, b, a):
    """Source-over blend onto the buffer at byte index i."""
    sa = a / 255.0
    da = px[i + 3] / 255.0
    oa = sa + da * (1 - sa)
    if oa <= 0:
        return
    px[i] = round((r * sa + px[i] * da * (1 - sa)) / oa)
    px[i + 1] = round((g * sa + px[i + 1] * da * (1 - sa)) / oa)
    px[i + 2] = round((b * sa + px[i + 2] * da * (1 - sa)) / oa)
    px[i + 3] = round(oa * 255)


def rr_dist(x, y):
    """Signed distance to the rounded-rect edge (negative = inside)."""
    cx = min(max(x, R), W - 1 - R)
    cy = min(max(y, R), H - 1 - R)
    return math.hypot(x - cx, y - cy) - R


# Background with 2px anti-aliased edge
for y in range(H):
    for x in range(W):
        d = rr_dist(x, y)
        if d < 1.5:
            a = 255 if d < -0.5 else round(255 * (1.5 - d) / 2.0)
            i = (y * W + x) * 4
            blend(i, *BG, max(0, min(255, a)))

# Waveform: capsule bars, symmetric arch, green -> violet gradient
bars = [0.30, 0.52, 0.78, 1.00, 0.66, 0.88, 0.48, 0.62, 0.34]
n = len(bars)
bw = 52.0                      # bar width
gap = 34.0
total = n * bw + (n - 1) * gap
x0 = (W - total) / 2
MAXH = 500.0
C1 = (74, 222, 128)            # #4ade80
C2 = (167, 139, 250)           # #a78bfa

for bi, f in enumerate(bars):
    t = bi / (n - 1)
    col = tuple(round(C1[k] + (C2[k] - C1[k]) * t) for k in range(3))
    bh = MAXH * f
    cx = x0 + bi * (bw + gap) + bw / 2
    half = max(0.0, bh / 2 - bw / 2)
    rad = bw / 2
    ylo = int(H / 2 - bh / 2) - 2
    yhi = int(H / 2 + bh / 2) + 3
    xlo = int(cx - rad) - 2
    xhi = int(cx + rad) + 3
    for y in range(ylo, yhi):
        dy = max(0.0, abs(y - H / 2) - half)
        for x in range(xlo, xhi):
            dx = x - cx
            d = math.hypot(dx, dy) - rad
            if d < 1.0:
                a = 255 if d < -1.0 else round(255 * (1.0 - d) / 2.0)
                blend((y * W + x) * 4, *col, max(0, min(255, a)))


def chunk(tag, data):
    return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data))


raw = b''.join(b'\x00' + bytes(px[y * W * 4:(y + 1) * W * 4]) for y in range(H))
png = (b'\x89PNG\r\n\x1a\n'
       + chunk(b'IHDR', struct.pack('>IIBBBBB', W, H, 8, 6, 0, 0, 0))
       + chunk(b'IDAT', zlib.compress(raw, 9))
       + chunk(b'IEND', b''))

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'icon-src.png')
with open(out, 'wb') as fh:
    fh.write(png)
print(f'wrote {out} ({len(png)} bytes)')
