#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Count frames whose client content went backwards, in a strip from filmstrip.sh.

    spikes/stale-frame-repro/regressions.py spikes/stale-frame-repro/strip-09-48-03

Hashing one region and calling a match a regression does not work: a panel reading `0.3%` repeats
whenever the value recurs, so any single region reports a rate made of nothing. The way past that
floor is agreement -- several panels showing unrelated quantities all matching the *same* earlier
frame at once. CPU load, network throughput and a process table do not re-occur together by chance.

The periodicity control is printed alongside and is not optional. btm's graphs scroll one column
per tick, so a flat dotted stretch matches itself at a fixed distance for purely cosmetic reasons,
and that distance is uniform -- which reads exactly like a mechanism. If a panel matches `i-d` on
most frames, the distance-`d` hits mean nothing; the finding is only real when the matches are rare
and the panels agree.
"""

import hashlib
import pathlib
import sys

from PIL import Image

# btm's panels in a 2560x1440 scanout, plus the whole window.
PANELS = {
    "cpu": (262, 228, 1965, 512),
    "net": (262, 986, 1261, 1248),
    "procs": (1267, 985, 2280, 1245),
    "temps": (1420, 540, 2280, 760),
    "disks": (1420, 760, 2280, 960),
}
WINDOW = (246, 111, 2286, 1287)
# A panel that never changes matches every frame and can only dilute the agreement.
MIN_UNIQUE = 2
# How many panels must agree on the same earlier frame.
MIN_AGREE = 3


def main(strip: pathlib.Path) -> int:
    frames = sorted(strip.glob("[0-9]*.png"))
    if not frames:
        print(f"no frames in {strip}")
        return 1
    ims = [Image.open(f).convert("RGB") for f in frames]
    n = len(ims)

    def hashes(box):
        return [hashlib.sha256(im.crop(box).tobytes()).hexdigest()[:10] for im in ims]

    h = {k: hashes(b) for k, b in PANELS.items()}
    live = [k for k in PANELS if len(set(h[k])) >= MIN_UNIQUE]
    window = hashes(WINDOW)

    print(f"{strip.name}: {n} frames, panels carrying signal: {', '.join(live)}")
    for k in live:
        best = max(
            ((d, sum(1 for i in range(d, n) if h[k][i] == h[k][i - d])) for d in range(1, 8)),
            key=lambda x: x[1],
        )
        print(
            f"  {k:6s} {len(set(h[k])):3d} unique; most repetitive distance {best[0]} "
            f"matches {best[1]}/{n - best[0]} frames"
        )

    events = []
    for i in range(n):
        for j in range(i):
            agree = [k for k in live if h[k][i] == h[k][j]]
            if len(agree) >= MIN_AGREE:
                events.append((i, j, agree, window[i] == window[j]))

    print(f"\n{len(events)} regressions ({MIN_AGREE}+ panels agreeing on one earlier frame):")
    for i, j, agree, whole in events:
        print(
            f"  frame {i + 1:03d} == frame {j + 1:03d}  distance {i - j}  "
            f"{'+'.join(agree)}  {'whole window too' if whole else 'window chrome differs'}"
        )
    if events:
        ds = [i - j for i, j, _, _ in events]
        print(f"\ndistances: {sorted(set(ds))}   rate: {len(events)}/{n} frames")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(pathlib.Path(sys.argv[1])))
