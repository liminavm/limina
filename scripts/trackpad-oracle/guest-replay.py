#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Replay a host-side trackpad replay into uinput clones of limina's guest devices, and judge
it with the real libinput.

Run in the guest as root: guest-replay.py <replay.txt> <width> <height> [fuzz] [--verbose]
The replay (from the `dump_replay` test) is `t_us dev type code value` lines, dev being
`touchpad` or `pointer`, under a `# intended right=N left=M` header. Two uinput devices are
created — a clickpad with the touchpad's capabilities, axes and resolution, and a mouse
cloning the tablet limina clicks through — the events are written at their recorded times, and
`libinput debug-events --enable-tap --set-click-method=clickfinger` (GNOME's defaults) counts
the button presses each device produced. Prints a verdict line and exits 0 on a pass: every
intended click reached the guest once, through the pointer, and the touchpad produced none.

It also measures the touchpad's two-finger scroll, as a client would see it: for each scroll
(libinput's finger scroll events up to its stop), the event count and span, the velocity GTK's
kinetic scrolling would compute at the stop (`scroll_history_finish` in GTK 3 and 4: the deltas
of the last 150 ms over their time span, the stop included), the deltas against the scroll's
direction (a wobble), and the steps more than 1.7 times as fast as both neighbours (a double step). `fuzz`
sets the clone's MT position fuzz (0.01 mm units); `--verbose` runs libinput verbosely, which
logs its gesture state machine.
"""

import fcntl
import os
import re
import struct
import subprocess
import sys
import tempfile
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


def device(name, product, keys, rels=(), abses=(), props=(), fuzz=None):
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
            f = (fuzz or {}).get(code, 0)
            # struct uinput_abs_setup: u16 code, (pad), struct input_absinfo
            # {value, minimum, maximum, fuzz, flat, resolution}.
            fcntl.ioctl(fd, UI_ABS_SETUP, struct.pack("Hxxiiiiii", code, 0, lo, hi, f, 0, res))
    for p in props:
        fcntl.ioctl(fd, UI_SET_PROPBIT, p)
    setup = struct.pack("HHHH80sI", BUS_VIRTUAL, 0x4B47, product, 1, name.encode(), 0)
    fcntl.ioctl(fd, UI_DEV_SETUP, setup)
    fcntl.ioctl(fd, UI_DEV_CREATE)
    return fd


def write(fd, type_, code, value):
    os.write(fd, struct.pack("llHHi", 0, 0, type_, code, value))


# `-event7   POINTER_SCROLL_FINGER   +1.234s  vert 3.45/0.0* horiz 0.00/0.0 (finger)`, with a
# repeat count before the time on the lines after the first of a run.
SCROLL = re.compile(
    r"^[-\s]*(event\d+)\s+POINTER_SCROLL_FINGER\s+(?:\d+\s+)?\+([\d.]+)s\s+"
    r"vert (-?[\d.]+)/-?[\d.]+(\*?)\s+horiz (-?[\d.]+)/-?[\d.]+(\*?)",
    re.M,
)
GTK_WINDOW_MS = 150


def scrolls(out, node):
    """The touchpad's scrolls: each a list of (t_ms, delta) on its dominant axis, and the stop."""
    done, cur = [], []
    for ev, t, v, vs, h, hs in SCROLL.findall(out):
        if ev != node:
            continue
        t_ms = round(float(t) * 1000)
        v, h = float(v), float(h)
        if (vs and v == 0 and (not hs or h == 0)) or (hs and h == 0 and not vs):
            if cur:
                done.append((cur, t_ms))
            cur = []
            continue
        cur.append((t_ms, v, h))
    result = []
    for events, stop in done:
        vert = sum(abs(e[1]) for e in events) >= sum(abs(e[2]) for e in events)
        result.append(([(t, v if vert else h) for t, v, h in events], stop))
    return result


def measure(events, stop):
    deltas = [d for _, d in events]
    total = sum(deltas)
    sign = 1 if total >= 0 else -1
    wrong = [d for d in deltas if d * sign < 0]
    window = [(t, d) for t, d in events if t >= stop - GTK_WINDOW_MS] + [(stop, 0.0)]
    span = window[-1][0] - window[0][0]
    velocity = sum(d for _, d in window) * 1000 / span if span else 0.0
    # Speed, not step size: a late sample carrying twice the motion is the finger's real speed.
    speeds = [abs(d) / max(t - p, 1) for (p, _), (t, d) in zip(events, events[1:])]
    doubles = sum(
        1
        for i in range(1, len(speeds) - 1)
        if speeds[i] > 1.7 * speeds[i - 1] and speeds[i] > 1.7 * speeds[i + 1] and speeds[i - 1] > 0
    )
    return {
        "n": len(events),
        "span": events[-1][0] - events[0][0],
        "total": total,
        "velocity": velocity,
        "wrong": len(wrong),
        "wrong_sum": sum(wrong),
        "doubles": doubles,
    }


def report_scroll(out, node):
    rows = [measure(e, s) for e, s in scrolls(out, node)]
    with open("/tmp/trackpad-oracle-scroll.txt", "w") as f:
        f.write("n span_ms total gtk_velocity wrong wrong_sum doubles\n")
        for r in rows:
            f.write(
                f"{r['n']} {r['span']} {r['total']:.1f} {r['velocity']:.0f} "
                f"{r['wrong']} {r['wrong_sum']:.2f} {r['doubles']}\n"
            )
    short = [r for r in rows if r["n"] <= 4]
    still = sum(1 for r in rows if r["velocity"] == 0)
    print(
        f"SCROLL: {len(rows)} scrolls, {len(short)} of at most 4 events, {still} with no GTK "
        f"velocity; wrong-way deltas {sum(r['wrong'] for r in rows)} in "
        f"{sum(1 for r in rows if r['wrong'])} scrolls; double steps "
        f"{sum(r['doubles'] for r in rows)} of {sum(r['n'] for r in rows)} events"
    )


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    verbose = "--verbose" in sys.argv
    path, width, height = args[0], int(args[1]), int(args[2])
    fuzz = int(args[3]) if len(args) > 3 else 0
    lines = open(path).read().splitlines()
    intended = dict(re.findall(r"(\w+)=(\d+)", lines[0]))
    events = [line.split() for line in lines[1:] if line and not line.startswith("#")]

    launched = time.monotonic()
    # To a file, not a pipe: nothing reads a pipe until the replay ends, and once its buffer
    # fills libinput blocks and every later event is lost.
    capture = tempfile.TemporaryFile("w+")
    libinput = subprocess.Popen(
        [
            "stdbuf",
            "-oL",
            "libinput",
            "debug-events",
            "--enable-tap",
            "--set-click-method=clickfinger",
        ]
        + (["--verbose"] if verbose else []),
        stdout=capture,
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
        fuzz={ABS_X: fuzz, ABS_Y: fuzz, ABS_MT_POSITION_X: fuzz, ABS_MT_POSITION_Y: fuzz},
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
    # A frame written late lands closer to the next one than it was sent, which libinput can
    # read as a touch jump: the count of late frames says whether the timing held.
    late, worst = 0, 0.0
    for t_us, dev, type_, code, value in events:
        due = start + (int(t_us) - t0) / 1e6
        delay = due - time.monotonic()
        if delay > 0.002:
            time.sleep(delay - 0.002)
        while time.monotonic() < due:  # sleep overshoots by a millisecond or more
            pass
        delay = due - time.monotonic()
        if int(type_) == 0 and dev == "touchpad":
            worst = max(worst, -delay)
            late += -delay > 0.002
        write(pad if dev == "touchpad" else ptr, int(type_), int(code), int(value))
    time.sleep(1.0)
    for fd in (pad, ptr):
        fcntl.ioctl(fd, UI_DEV_DESTROY)
        os.close(fd)
    time.sleep(0.5)
    libinput.terminate()
    libinput.wait()
    capture.seek(0)
    out = capture.read()
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
    jumps = out.count("Touch jump detected")
    print(f"TIMING: {late} touchpad frames written over 2 ms late (worst {worst * 1000:.1f} ms); "
          f"libinput touch jumps {jumps} (it logs at most 5 a day)")
    pad_node = next((n for n, name in names.items() if name == "limina replay touchpad"), None)
    report_scroll(out, pad_node)
    print(
        f"{'PASS' if ok else 'FAIL'}: pointer right={right}/{intended['right']} "
        f"left={left}/{intended['left']}, touchpad clicks={pad_clicks} (want 0); all={counts}"
    )
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
