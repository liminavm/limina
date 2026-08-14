#!/usr/bin/env python3
"""Measure the per-row horizontal drift (the shear slope) in a captured frame.

The vrend/KK stride bug displaces row y of a surface by a constant `d` pixels relative to
row y-1, because writer and reader disagree on the row pitch by a fixed number of bytes:

    d_px = (reader_pitch_B - writer_pitch_B) / 4        for a 4-byte pixel

So measuring `d` from pixels tells us the effective pitch delta, which is the number the
mechanism has to explain. The arithmetic at the KK emission site predicts a delta of
align_up(w*4,16) - w*4, i.e. 4/8/12 bytes = 1/2/3 px. The 2026-08-14 field capture *looked*
like ~7 px/row, and that gap is the open question this script exists to settle.

Method: per-row intensity centroid. Under a pure horizontal shear the centroid of row y sits
at centroid0 + d*y, so the MEDIAN of successive centroid differences is d. The median (not the
mean) is the point: the shear wraps around the surface width, and a wrap injects a single huge
negative jump that would wreck a mean or a least-squares fit while leaving the median untouched.

Rows are ignored unless they carry enough bright pixels to have a meaningful centroid -- on a
mostly-black scene an empty row's "centroid" is pure noise.

Usage: measure-shear.py <png> <x0> <y0> <x1> <y1>
"""
import sys
from PIL import Image

if len(sys.argv) != 6:
    sys.exit(__doc__)

path = sys.argv[1]
x0, y0, x1, y1 = (int(a) for a in sys.argv[2:6])

img = Image.open(path).convert("L").crop((x0, y0, x1, y1))
w, h = img.size
px = img.tobytes()

THRESH = 24  # above the black background, below the faint desktop bleed at the edges
MIN_BRIGHT = 8  # a row needs this many lit pixels before its centroid means anything

centroids = []
for y in range(h):
    row = px[y * w:(y + 1) * w]
    tot = 0
    acc = 0
    n = 0
    for x in range(w):
        v = row[x]
        if v > THRESH:
            acc += v * x
            tot += v
            n += 1
    centroids.append(acc / tot if (tot and n >= MIN_BRIGHT) else None)

pairs = [
    (y, centroids[y] - centroids[y - 1])
    for y in range(1, h)
    if centroids[y] is not None and centroids[y - 1] is not None
]
diffs = sorted(d for _, d in pairs)
if not diffs:
    sys.exit("no usable rows -- check the crop and THRESH")

median = diffs[len(diffs) // 2]
# Robust spread: the interquartile range. A clean shear has a tight IQR; a wide one means the
# centroid is tracking scene content rather than the displacement, and the number is not a slope.
q1 = diffs[len(diffs) // 4]
q3 = diffs[(3 * len(diffs)) // 4]

print(f"crop            {w}x{h} from {path}")
print(f"usable rows     {len(pairs)} of {h}")
print(f"drift per row   {median:+.3f} px   (IQR {q1:+.3f} .. {q3:+.3f})")
print(f"implied pitch delta {median * 4:+.1f} bytes")
