#!/usr/bin/env python3
"""Log the three POSIX clocks once a second.

The point of the spike: a guest whose vCPUs are stopped OUTSIDE the
timekeeping-suspended window sees that wall time land in CLOCK_MONOTONIC,
because sleeptime injection deliberately moves only REALTIME and BOOTTIME
(kernel/time/timekeeping.c __timekeeping_inject_sleeptime). This logger makes
the split observable: freeze the process along with the rest of userspace and
read where the gap went.
"""

import time
import sys

CLOCKS = (
    ("real", time.CLOCK_REALTIME),
    ("mono", time.CLOCK_MONOTONIC),
    ("boot", time.CLOCK_BOOTTIME),
)

out = open(sys.argv[1] if len(sys.argv) > 1 else "/var/log/clocklog.txt", "a", buffering=1)
while True:
    out.write(" ".join(f"{name}={time.clock_gettime(clk):.6f}" for name, clk in CLOCKS) + "\n")
    time.sleep(1)
