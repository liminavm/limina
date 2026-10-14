#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Print the timestamps of a CRTC's next vblanks, as the kernel reports them.

Usage: vblankgrid.py <card> <crtc index> <count>

Waits for `count` consecutive vblanks with DRM_IOCTL_WAIT_VBLANK (which needs no DRM master, so it
runs beside a seated compositor) and prints one `seq=<n> t_ns=<ns> mono_ns=<ns> raw_ns=<ns>` line
each: the vblank's CLOCK_MONOTONIC timestamp (the one every flip-done event of it carries), and
CLOCK_MONOTONIC and CLOCK_MONOTONIC_RAW read together just after. NTP slews CLOCK_MONOTONIC but not
_RAW, which runs at the counter's fixed rate, so the pair places the timestamp on the counter.
"""

import fcntl
import struct
import sys
import time

DRM_IOCTL_WAIT_VBLANK = 0xC018643A  # _IOWR('d', 0x3a, union drm_wait_vblank), 24 bytes
DRM_VBLANK_RELATIVE = 0x1
DRM_VBLANK_HIGH_CRTC_SHIFT = 1


def main():
    card, crtc, count = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    kind = DRM_VBLANK_RELATIVE | (crtc << DRM_VBLANK_HIGH_CRTC_SHIFT if crtc > 1 else 0)
    if crtc == 1:
        kind |= 0x20000000  # DRM_VBLANK_SECONDARY
    with open(card, "rb", buffering=0) as f:
        for _ in range(count):
            buf = bytearray(struct.pack("=IIQ8x", kind, 1, 0))
            fcntl.ioctl(f, DRM_IOCTL_WAIT_VBLANK, buf)
            mono = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
            raw = time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)
            _, seq, sec, usec = struct.unpack("=IIqq", buf)
            print(
                f"seq={seq} t_ns={sec * 1_000_000_000 + usec * 1000} mono_ns={mono} raw_ns={raw}",
                flush=True,
            )


if __name__ == "__main__":
    main()
