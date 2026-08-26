#!/usr/bin/env python3
"""Score saved banner PNGs by whether the card's TITLE row rendered.

The header-strip detector in calib.sh measures one row, on the standing assumption that the title
dies with the header. An arm that makes the header come back while the title stays missing breaks
that assumption and reports a clean sweep -- which is how KK_LIMINA_FORCE_LOAD first read as a
cure. Measuring the title independently, from the PNGs each run already saves, keeps an arm from
being scored against an assumption it is busy invalidating.

Band calibrated against a titled and an untitled capture of the same card: rows 83-96 of the crop
carry 41-113 lit pixels when the title renders and exactly 0 when it does not.

  score-title.py <caldir>...
"""
import sys, os, glob
from PIL import Image

TITLE_ROWS = range(83, 97)
TITLE_COLS = range(10, 420)
THRESH = 150      # luminance of lit text against the card's grey
INK_MIN = 15      # a titled row band clears this by 3x; an untitled one reads 0


def title_ink(path):
    px = Image.open(path).convert('L').load()
    return sum(1 for y in TITLE_ROWS for x in TITLE_COLS if px[x, y] > THRESH)


def main():
    for d in sys.argv[1:]:
        # Discard the same samples calib.sh discarded: no banner on screen, nothing to judge.
        skip = set()
        tsv = os.path.join(d, 'hdr.tsv')
        if os.path.exists(tsv):
            for line in open(tsv):
                f = line.split('\t')
                if len(f) > 1 and f[1] == 'NOBANNER':
                    skip.add(f[0])
        dmg = tot = 0
        for p in sorted(glob.glob(os.path.join(d, 'full-*.png'))):
            n = os.path.basename(p)[5:8]
            if n in skip:
                continue
            tot += 1
            if title_ink(p) < INK_MIN:
                dmg += 1
        if tot:
            print(f"{os.path.basename(d):16s} title missing {dmg:2d}/{tot:2d}  ({100*dmg/tot:3.0f}%)")


if __name__ == '__main__':
    main()
