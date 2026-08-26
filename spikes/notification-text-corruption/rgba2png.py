#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Turn vrend-replay's REPLAY_DUMP_DIR raw dumps into viewable PNGs. The dumps are BGRA (virgl
# format 67 is B8G8R8A8), and the card offscreens are premultiplied text on transparency, so a
# straight alpha-composite onto white is what makes the glyphs legible to a human eye.
import glob, os, re, struct, sys, zlib

def png(path, w, h, rgb):
    raw = b"".join(b"\x00" + rgb[y * w * 3:(y + 1) * w * 3] for y in range(h))
    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xffffffff)
    open(path, "wb").write(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b""))

for src in sorted(sys.argv[1:]):
    m = re.search(r"res(\d+)_(\d+)x(\d+)\.rgba$", src)
    if not m:
        continue
    _, w, h = int(m.group(1)), int(m.group(2)), int(m.group(3))
    data = open(src, "rb").read()
    out = bytearray()
    for i in range(0, w * h * 4, 4):
        b, g, r, a = data[i], data[i + 1], data[i + 2], data[i + 3]
        # composite over white so glyphs on a transparent offscreen are actually visible
        inv = 255 - a
        out += bytes((min(255, r + inv), min(255, g + inv), min(255, b + inv)))
    dst = os.path.splitext(src)[0] + ".png"
    png(dst, w, h, bytes(out))
    print(dst)
