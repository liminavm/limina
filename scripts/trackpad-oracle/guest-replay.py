#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Replay a host-side trackpad replay into uinput clones of limina's guest devices, and judge
it with the real libinput.

Run in the guest as root: guest-replay.py <replay.txt> <width> <height>
The replay (from the `dump_replay` test) is `t_us dev type code value` lines, dev being
`touchpad` or `pointer`, under a `# intended right=N left=M` header. Two uinput devices are
created — a clickpad with the touchpad's capabilities, axes and resolution, and a mouse
cloning the tablet limina clicks through — the events are written at their recorded times, and
`libinput debug-events --enable-tap --set-click-method=clickfinger` (GNOME's defaults) counts
the button presses each device produced. Prints a verdict line and exits 0 on a pass: every
intended click reached the guest once, through the pointer, and the touchpad produced none.
"""

import fcntl
import os
import re
import struct
import subprocess
import sys
import time

UI_SET_EVBIT, UI_SET_KEYBIT, UI_SET_RELBIT, UI_SET_ABSBIT = (
    0x40045564,
    0x40045565,
    0x40045566,
    0x40045567,
)
UI_SET_PROPBIT = 0x4004556E
UI_DEV_SETUP, UI_ABS_SETUP = 0x405C5503, 0x401C5504
UI_DEV_CREATE, UI_DEV_DESTROY = 0x5501, 0x5502
EV_KEY, EV_REL, EV_ABS = 1, 2, 3
BTN_LEFT, BTN_RIGHT, BTN_MIDDLE = 0x110, 0x111, 0x112
BTN_TOOL_FINGER, BTN_TOUCH, BTN_TOOL_DOUBLETAP, BTN_TOOL_TRIPLETAP = 0x145, 0x14A, 0x14D, 0x14E
ABS_X, ABS_Y = 0, 1
ABS_MT_SLOT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID = 0x2F, 0x35, 0x36, 0x39
REL_HWHEEL, REL_WHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES = 6, 8, 0x0B, 0x0C
INPUT_PROP_POINTER, INPUT_PROP_BUTTONPAD = 0, 2
BUS_VIRTUAL = 6


def device(name, product, keys, rels=(), abses=(), props=()):
    fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
    for ev, bits, ioc in ((EV_KEY, keys, UI_SET_KEYBIT), (EV_REL, rels, UI_SET_RELBIT)):
        if bits:
            fcntl.ioctl(fd, UI_SET_EVBIT, ev)
            for b in bits:
                fcntl.ioctl(fd, ioc, b)
    if abses:
        fcntl.ioctl(fd, UI_SET_EVBIT, EV_ABS)
        for code, lo, hi, res in abses:
            fcntl.ioctl(fd, UI_SET_ABSBIT, code)
            # struct uinput_abs_setup: u16 code, (pad), struct input_absinfo
            # {value, minimum, maximum, fuzz, flat, resolution}.
            fcntl.ioctl(fd, UI_ABS_SETUP, struct.pack("Hxxiiiiii", code, 0, lo, hi, 0, 0, res))
    for p in props:
        fcntl.ioctl(fd, UI_SET_PROPBIT, p)
    setup = struct.pack("HHHH80sI", BUS_VIRTUAL, 0x4B47, product, 1, name.encode(), 0)
    fcntl.ioctl(fd, UI_DEV_SETUP, setup)
    fcntl.ioctl(fd, UI_DEV_CREATE)
    return fd


def write(fd, type_, code, value):
    os.write(fd, struct.pack("llHHi", 0, 0, type_, code, value))


def main():
    path, width, height = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    lines = open(path).read().splitlines()
    intended = dict(re.findall(r"(\w+)=(\d+)", lines[0]))
    events = [line.split() for line in lines[1:] if line and not line.startswith("#")]

    launched = time.monotonic()
    libinput = subprocess.Popen(
        [
            "stdbuf",
            "-oL",
            "libinput",
            "debug-events",
            "--enable-tap",
            "--set-click-method=clickfinger",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    time.sleep(1.0)
    pad = device(
        "limina replay touchpad",
        0x7F04,
        [BTN_LEFT, BTN_TOOL_FINGER, BTN_TOUCH, BTN_TOOL_DOUBLETAP, BTN_TOOL_TRIPLETAP],
        abses=[
            (ABS_X, 0, width, 100),
            (ABS_Y, 0, height, 100),
            (ABS_MT_SLOT, 0, 2, 0),
            (ABS_MT_POSITION_X, 0, width, 100),
            (ABS_MT_POSITION_Y, 0, height, 100),
            (ABS_MT_TRACKING_ID, 0, 0xFFFF, 0),
        ],
        props=[INPUT_PROP_POINTER, INPUT_PROP_BUTTONPAD],
    )
    # The tablet as limina's worker advertises it (`PointerConfig`): absolute, 0..=32767.
    ptr = device(
        "limina replay pointer",
        0x7F02,
        [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
        rels=[REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES],
        abses=[(ABS_X, 0, 32767, 0), (ABS_Y, 0, 32767, 0)],
        props=[INPUT_PROP_POINTER],
    )
    time.sleep(1.5)  # let libinput add both devices before the first event

    start = time.monotonic()
    offset = start - launched
    t0 = int(events[0][0]) if events else 0
    for t_us, dev, type_, code, value in events:
        delay = start + (int(t_us) - t0) / 1e6 - time.monotonic()
        if delay > 0:
            time.sleep(delay)
        write(pad if dev == "touchpad" else ptr, int(type_), int(code), int(value))
    time.sleep(1.0)
    for fd in (pad, ptr):
        fcntl.ioctl(fd, UI_DEV_DESTROY)
        os.close(fd)
    time.sleep(0.5)
    libinput.terminate()
    out = libinput.communicate()[0]
    # Kept for a closer look: libinput's own times are seconds since it started, and the
    # replay's first event went out `offset` seconds after that.
    with open("/tmp/trackpad-oracle-libinput.log", "w") as f:
        f.write(f"# replay started {offset:.3f}s after libinput\n{out}")

    names = dict(re.findall(r"^-?(event\d+)\s+DEVICE_ADDED\s+(.+?)\s{2,}", out, re.M))
    counts = {}
    # debug-events marks a line with a leading `-` whenever the device changes.
    pressed = r"^[-\s]*(event\d+)\s+POINTER_BUTTON\s+.*?(BTN_\w+) \(\d+\) pressed"
    for node, button in re.findall(pressed, out, re.M):
        key = (names.get(node, node), button)
        counts[key] = counts.get(key, 0) + 1
    right = counts.get(("limina replay pointer", "BTN_RIGHT"), 0)
    left = counts.get(("limina replay pointer", "BTN_LEFT"), 0)
    pad_clicks = sum(v for (n, _), v in counts.items() if n == "limina replay touchpad")
    ok = right == int(intended["right"]) and left == int(intended["left"]) and pad_clicks == 0
    print(
        f"{'PASS' if ok else 'FAIL'}: pointer right={right}/{intended['right']} "
        f"left={left}/{intended['left']}, touchpad clicks={pad_clicks} (want 0); all={counts}"
    )
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
