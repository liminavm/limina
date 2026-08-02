#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Control experiment: push the GNOME hot corner from *inside* the guest, with limina's
host-side edge resistance entirely out of the picture.

The question this settles, before any host-side theory is worth having: given a plain relative
pointer device shoving into the top-left corner, does mutter's pressure barrier fire at all in
this guest? If it doesn't, no amount of host-side threshold tuning was ever going to help, and
the forwarded-pressure model is wrong. If it does, the pressure figure it needed is the number
the host side has to clear.

Creates its own uinput mouse, slams the pointer into the corner, then pushes in measured steps,
reporting GNOME's own OverviewActive after each batch.

    sudo python3 guest-corner-control.py [step_px] [batches]
"""
import subprocess
import sys
import time

from evdev import UInput, ecodes as e

STEP = int(sys.argv[1]) if len(sys.argv) > 1 else 10
BATCHES = int(sys.argv[2]) if len(sys.argv) > 2 else 30


def overview():
    out = subprocess.run(
        ["sudo", "-u", "claude", "env", "XDG_RUNTIME_DIR=/run/user/1000", "busctl", "--user",
         "get-property", "org.gnome.Shell", "/org/gnome/Shell", "org.gnome.Shell",
         "OverviewActive"],
        capture_output=True, text=True)
    return out.stdout.strip() or out.stderr.strip()


ui = UInput({e.EV_REL: [e.REL_X, e.REL_Y], e.EV_KEY: [e.BTN_LEFT]}, name="edge-probe-mouse")
time.sleep(1.5)  # let mutter notice the new device


def move(dx, dy):
    ui.write(e.EV_REL, e.REL_X, dx)
    ui.write(e.EV_REL, e.REL_Y, dy)
    ui.syn()


print("overview before:", overview())
# Park in the corner first. The barrier only accumulates pressure for motion that pushes
# *against* it, so the pointer has to already be there.
for _ in range(25):
    move(-200, -200)
    time.sleep(0.005)
time.sleep(0.4)
print("overview after parking:", overview())

delivered = 0
for i in range(BATCHES):
    move(-STEP, -STEP)
    delivered += STEP
    time.sleep(0.012)
    if (i + 1) % 5 == 0:
        print(f"  pushed {delivered} px -> {overview()}")

time.sleep(0.5)
print("overview after:", overview())
ui.close()
