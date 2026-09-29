#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Log every raw evdev event a guest input device receives, stamped in wallclock microseconds.

The guest clock is anchored to the host's (PL031 + TimeSync), so these stamps line up with the
host's `LIMINA_POINTER_WIRE_TRACE` lines (`[WIRE] t=<us> dev=touchpad ...`): what the host wrote
and what the guest kernel delivered can be matched event for event. Unlike `libinput
debug-events`, this shows the device's raw stream, not libinput's reading of it.

Usage (in the guest, as root): guest-evdev-log.py "limina Virtual Touchpad" > /tmp/evdev.log
Output: `t=<us> type=<n> code=<n> value=<n>`, one line per event, flushed per SYN_REPORT.
"""

import fcntl
import glob
import struct
import sys
import time

EVIOCSCLOCKID = 0x400445A0  # _IOW('E', 0xa0, int)
CLOCK_REALTIME = 0
EVENT = struct.Struct("llHHi")  # struct input_event on 64-bit: timeval, type, code, value


def find(name):
    for path in sorted(glob.glob("/sys/class/input/event*/device/name")):
        with open(path) as f:
            if f.read().strip() == name:
                return "/dev/input/" + path.split("/")[4]
    sys.exit(f"no input device named {name!r}")


def main():
    dev = find(sys.argv[1] if len(sys.argv) > 1 else "limina Virtual Touchpad")
    print(f"# {dev} opened at t={int(time.time() * 1e6)}", flush=True)
    with open(dev, "rb", buffering=0) as f:
        fcntl.ioctl(f, EVIOCSCLOCKID, struct.pack("i", CLOCK_REALTIME))
        while True:
            data = f.read(EVENT.size * 64)
            for off in range(0, len(data), EVENT.size):
                sec, usec, type_, code, value = EVENT.unpack_from(data, off)
                sys.stdout.write(f"t={sec * 1000000 + usec} type={type_} code={code} value={value}\n")
                if type_ == 0:
                    sys.stdout.flush()


if __name__ == "__main__":
    main()
