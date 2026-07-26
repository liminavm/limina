#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Crop + upscale a region of a captured scanout PNG so a small on-screen counter is legible.

Exists because the WebGL-aquarium fps number is drawn ~14 px tall in the top-left of a 1280x800
scanout. Reading the full frame works but wastes a lot of image budget on fish; cropping to the
counter and upscaling makes the number unambiguous.

macOS `sips` is NOT usable for this: it accepts `--cropOffset` but silently CENTER-crops anyway
(verified 2026-07-26), which yields a picture of the middle of the scene and no counter at all.
Host python3 has PIL, so use that.

Usage:
    ./crop-fps.py IN.png OUT.png [--box LEFT,TOP,WIDTH,HEIGHT] [--scale N]
Defaults are tuned for the aquarium counter at 1280x800: box 0,0,300,130 upscaled 4x with nearest
neighbour (nearest keeps the text edges crisp; bicubic smears small glyphs).
"""

import argparse
import sys

from PIL import Image


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src")
    ap.add_argument("dst")
    ap.add_argument("--box", default="0,0,300,130",
                    help="LEFT,TOP,WIDTH,HEIGHT of the region to crop (default the aquarium counter)")
    ap.add_argument("--scale", type=int, default=4, help="integer upscale factor (default 4)")
    ap.add_argument("--require-content", action="store_true",
                    help="exit 3 if the crop looks blank (no bright text pixels) — lets a caller "
                         "retry instead of banking a frame captured before the page painted")
    args = ap.parse_args()

    try:
        left, top, w, h = (int(v) for v in args.box.split(","))
    except ValueError:
        print(f"bad --box {args.box!r}; expected LEFT,TOP,WIDTH,HEIGHT", file=sys.stderr)
        return 2

    im = Image.open(args.src).convert("RGB")
    # Clamp to the image so a smaller-than-expected capture crops to what exists rather than
    # throwing — a truncated counter is still readable, an exception ends the whole sweep.
    right, bottom = min(left + w, im.width), min(top + h, im.height)
    if left >= im.width or top >= im.height:
        print(f"crop origin ({left},{top}) is outside {im.width}x{im.height}", file=sys.stderr)
        return 1
    crop = im.crop((left, top, right, bottom))

    # Blank-frame guard. The counter is white-on-dark, so a frame captured before the page painted
    # (Firefox cold start is well over the settle time on the FIRST launch) has essentially no bright
    # pixels. Measuring that fraction is a cheap, OCR-free "did anything render yet".
    px = crop.convert("L").getdata()
    bright = sum(1 for v in px if v > 200) / max(1, len(px))
    if args.require_content and bright < 0.005:
        print(f"crop looks blank (bright_frac={bright:.4f}) — page probably had not painted yet",
              file=sys.stderr)
        return 3

    crop = crop.resize((crop.width * args.scale, crop.height * args.scale), Image.NEAREST)
    crop.save(args.dst)
    print(f"{args.dst}: {crop.width}x{crop.height} (from {args.src} {im.width}x{im.height} "
          f"box {left},{top},{right - left},{bottom - top}) bright_frac={bright:.4f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
