#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Pin the guest's display to an exact mode + scale, in-guest, over Mutter's DisplayConfig D-Bus API.

WHY THIS EXISTS. Since `match-host` display mode shipped (2026-07-03, memory limina-display-modes)
a windowed boot drives the guest to the host screen's resolution and GNOME picks a *fractional*
scale for it — on the M1 Max that lands on 2560x1440 @ 2.5, which mutter hands to wayland clients as
`buffer_scale 3`. Two consequences that silently invalidate benchmark numbers:

  1. A client asking for a WxH window gets a 3W x 3H *buffer* — ~9x the pixels. Any fps/score
     compared against a pre-2026-07-03 number is then comparing different workloads.
  2. glmark2-es2-wayland's default 512x512 is not a multiple of 3, so the compositor rejects the
     buffer outright ("Buffer size (512x512) must be an integer multiple of the buffer_scale (3)")
     and the run reports a garbage score (we saw 97 and 274 on back-to-back runs of the same build).

So: before measuring anything, pin the display. `perf/ledger.csv` rows are only comparable when the
mode+scale match, and from 2026-07-26 the ledger runner pins 1280x800 @ 1.0 explicitly.

Usage (in the guest, inside the seated session's bus):
    ./set-guest-display.py --show                      # print current mode/scale, exit 0
    ./set-guest-display.py --verify 1280x800 1.0       # exit 0 only if that is ALREADY current
    ./set-guest-display.py --write-config 1280x800 1.0 # write monitors.xml; applies on next boot
    ./set-guest-display.py 1280x800 1.0                # live D-Bus apply — PROMPTS, see below

THE DIALOG TRAP (learned 2026-07-26, the hard way). The live apply path pops GNOME's
"Keep these display settings?" confirmation, which **reverts after ~20 s if nobody clicks**. That is
fatal for an unattended benchmark run: it blocks the script, and worse, it can revert the geometry
*mid-measurement* so the numbers silently describe a different display than the one you pinned. It
bit us twice in one session, the second time unnoticed.

So the supported order for automation is:
  1. `--write-config` once, then reboot the guest (or restart the session). monitors.xml is applied
     at compositor startup with **no dialog at all**.
  2. `--verify` at the top of the run, which is read-only and fails loudly rather than prompting.
Pair it with the supervisor's own fixed-resolution mode (`limina --display-resolution WIDTHxHEIGHT`)
so the virtual output offers exactly the mode you are pinning, instead of `match-host` driving it to
the host screen. Reserve the live apply for interactive poking with a human at the keyboard.

Exits nonzero (and changes nothing) if the requested mode does not support the requested scale, so a
caller can never silently benchmark a display it did not ask for.
"""

import sys

import gi

gi.require_version("Gio", "2.0")
from gi.repository import GLib, Gio  # noqa: E402

BUS_NAME = "org.gnome.Mutter.DisplayConfig"
OBJ_PATH = "/org/gnome/Mutter/DisplayConfig"
IFACE = "org.gnome.Mutter.DisplayConfig"

# ApplyMonitorsConfig `method`: 0 = verify only, 1 = temporary (reverts if not confirmed),
# 2 = persistent. We use 2 — a benchmark run must not have the display revert underneath it.
METHOD_PERSISTENT = 2


def proxy():
    return Gio.DBusProxy.new_for_bus_sync(
        Gio.BusType.SESSION, Gio.DBusProxyFlags.NONE, None,
        BUS_NAME, OBJ_PATH, IFACE, None,
    )


def current_state(p):
    """(serial, monitors, logical_monitors, properties) — see the DisplayConfig XML for the shapes."""
    return p.call_sync("GetCurrentState", None, Gio.DBusCallFlags.NONE, -1, None).unpack()


def describe(state):
    _serial, monitors, logicals, _props = state
    out = []
    for (connector, _vendor, _product, _mserial), modes, _mprops in monitors:
        cur = next((m for m in modes if m[6].get("is-current")), None)
        scale = next((lm[2] for lm in logicals if any(c[0] == connector for c in lm[5])), None)
        if cur:
            out.append(f"{connector}: {cur[1]}x{cur[2]}@{cur[3]:.3f} scale={scale} "
                       f"supported_scales={[round(s, 4) for s in cur[5]]}")
    return "\n".join(out)


def find_mode(monitors, want_w, want_h, want_scale):
    """-> (monitorspec, mode, matched_scale) or raises SystemExit with a loud message."""
    for spec, modes, _mprops in monitors:
        for mode in modes:
            _mode_id, w, h, _refresh, _pref, supported_scales, _mp = mode
            if (w, h) != (want_w, want_h):
                continue
            # Float compare against the compositor's own advertised list — mutter rejects a scale it
            # did not advertise, and a near-miss (1.0 vs 0.9999) fails opaquely at ApplyMonitorsConfig.
            match = next((s for s in supported_scales if abs(s - want_scale) < 1e-3), None)
            if match is None:
                print(f"mode {w}x{h} does not support scale {want_scale}; "
                      f"supported: {[round(s, 4) for s in supported_scales]}", file=sys.stderr)
                raise SystemExit(1)
            return spec, mode, match
    have = sorted({f"{m[1]}x{m[2]}" for _mi, modes, _mp in monitors for m in modes})
    print(f"no mode {want_w}x{want_h}; available: {' '.join(have)}", file=sys.stderr)
    raise SystemExit(1)


def write_config(state, want_w, want_h, want_scale):
    """Write ~/.config/monitors.xml — the DIALOG-FREE pin. Applied at next compositor startup."""
    import os
    from xml.sax.saxutils import escape

    _serial, monitors, _logicals, _props = state
    spec, mode, scale = find_mode(monitors, want_w, want_h, want_scale)
    connector, vendor, product, mserial = spec
    _mode_id, w, h, refresh, _pref, _ss, _mp = mode

    # `scale` must serialize as mutter parses it back; an integral scale written as "1.0" is fine,
    # but keep the full float for fractional values so a 1.3333 round-trips.
    scale_s = f"{scale:g}"
    xml = f"""<monitors version="2">
  <configuration>
    <logicalmonitor>
      <x>0</x>
      <y>0</y>
      <scale>{scale_s}</scale>
      <primary>yes</primary>
      <monitor>
        <monitorspec>
          <connector>{escape(connector)}</connector>
          <vendor>{escape(vendor)}</vendor>
          <product>{escape(product)}</product>
          <serial>{escape(mserial)}</serial>
        </monitorspec>
        <mode>
          <width>{w}</width>
          <height>{h}</height>
          <rate>{refresh}</rate>
        </mode>
      </monitor>
    </logicalmonitor>
  </configuration>
</monitors>
"""
    path = os.path.join(
        os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")), "monitors.xml"
    )
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(xml)
    print(f"wrote {path}: {connector} {w}x{h} scale={scale_s} (applies at next session start)")
    return 0


def main():
    argv = sys.argv[1:]
    p = proxy()

    if argv and argv[0] == "--show":
        print(describe(current_state(p)))
        return 0

    if argv and argv[0] in ("--verify", "--write-config"):
        cmd = argv[0]
        want = argv[1] if len(argv) > 1 else "1280x800"
        want_scale = float(argv[2]) if len(argv) > 2 else 1.0
        want_w, want_h = (int(v) for v in want.lower().split("x"))
        state = current_state(p)
        if cmd == "--write-config":
            return write_config(state, want_w, want_h, want_scale)
        # --verify: read-only assertion, never prompts. This is what benchmark runners call.
        _serial, monitors, logicals, _props = state
        for spec, modes, _mprops in monitors:
            cur = next((m for m in modes if m[6].get("is-current")), None)
            scale = next((lm[2] for lm in logicals if any(c[0] == spec[0] for c in lm[5])), None)
            if cur and (cur[1], cur[2]) == (want_w, want_h) and abs(scale - want_scale) < 1e-3:
                print(f"verify OK: {spec[0]} {cur[1]}x{cur[2]} scale={scale}")
                return 0
        print(f"verify FAILED: wanted {want_w}x{want_h}@{want_scale}, have:\n{describe(state)}",
              file=sys.stderr)
        return 1

    want = argv[0] if argv else "1280x800"
    want_scale = float(argv[1]) if len(argv) > 1 else 1.0
    try:
        want_w, want_h = (int(v) for v in want.lower().split("x"))
    except ValueError:
        print(f"bad geometry {want!r}; expected WIDTHxHEIGHT", file=sys.stderr)
        return 2

    serial, monitors, _logicals, _props = current_state(p)
    spec, mode, match = find_mode(monitors, want_w, want_h, want_scale)
    connector = spec[0]
    mode_id, w, h = mode[0], mode[1], mode[2]

    logical = GLib.Variant(
        "(uua(iiduba(ssa{sv}))a{sv})",
        (serial, METHOD_PERSISTENT, [(0, 0, match, 0, True, [(connector, mode_id, {})])], {}),
    )
    p.call_sync("ApplyMonitorsConfig", logical, Gio.DBusCallFlags.NONE, -1, None)
    print(f"applied {connector} {w}x{h} scale={match} "
          f"(NOTE: GNOME is now showing a 'Keep changes?' dialog — it reverts in ~20 s if nobody "
          f"confirms; use --write-config + reboot for unattended runs)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
