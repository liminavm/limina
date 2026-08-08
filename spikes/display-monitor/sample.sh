#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# One timestamped sample of the whole display chain, for an interactive EDID/monitor
# repro session. Four links, one oracle each — so when the user demonstrates a problem
# the timeline says WHICH link broke:
#
#   1. host   — NSScreen geometry the supervisor would see (system_profiler is too slow;
#               we take the window frame + screen from osascript) and the remembered
#               window state file (state.toml, whose fullscreen_display IS the EDID
#               identity hash).
#   2. push   — `display-control: pushed ...` lines in the supervisor log (the exact wire
#               command, EDID and all, that limina handed the guest).
#   3. guest kernel — /sys/class/drm connector status/modes + the EDID blob (base64; the
#               binary must never go through `cat` over a console/ssh pipe).
#   4. mutter — GetCurrentState over the SESSION bus, plus ~/.config/monitors.xml, which
#               GNOME keys on the EDID identity we generate.
#
# Usage: bash sample.sh <ssh-port> <out-dir> [label]
set -u
PORT="${1:?usage: sample.sh <ssh-port> <out-dir> [label]}"
OUT="${2:?usage: sample.sh <ssh-port> <out-dir> [label]}"
LABEL="${3:-sample}"
STAMP=$(date +%Y%m%dT%H%M%S)
F="$OUT/$STAMP-$LABEL.txt"
mkdir -p "$OUT"

SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
     -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=10 claude@127.0.0.1)

{
  echo "=== sample $STAMP ($LABEL) ==="

  echo
  echo "--- [1a] host: window + screen (osascript) ---"
  osascript -e 'tell application "System Events" to tell (first process whose name is "limina") to get {position, size} of window 1' 2>&1
  osascript -e 'tell application "Finder" to get bounds of window of desktop' 2>&1

  echo
  echo "--- [1b] host: remembered window state (state.toml) ---"
  cat "$(dirname "$0")/edid-repro.state.toml" 2>&1

  echo
  echo "--- [2] push: display-control lines (last 15) ---"
  grep -a "display-control\|display update\|EDID\|edid" /tmp/enhanced-efi-kk-worker.log 2>/dev/null | tail -15

  echo
  echo "--- [3] guest kernel: drm connectors ---"
  "${SSH[@]}" 'for c in /sys/class/drm/card*-*/; do
      n=$(basename "$c")
      printf "%s status=%s enabled=%s\n" "$n" "$(cat "$c/status" 2>/dev/null)" "$(cat "$c/enabled" 2>/dev/null)"
      echo "  modes: $(tr "\n" " " < "$c/modes" 2>/dev/null)"
      # sysfs binary attributes can report size 0 while reading fine — never gate on -s.
      b=$(base64 -w0 < "$c/edid" 2>/dev/null)
      echo "  edid.b64: ${b:-(none)}"
    done' 2>&1

  echo
  echo "--- [4a] mutter: GetCurrentState ---"
  "${SSH[@]}" 'export XDG_RUNTIME_DIR=/run/user/$(id -u);
    export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus;
    gdbus call --session -d org.gnome.Mutter.DisplayConfig \
      -o /org/gnome/Mutter/DisplayConfig \
      -m org.gnome.Mutter.DisplayConfig.GetCurrentState' 2>&1

  echo
  echo "--- [4b] mutter: monitors.xml ---"
  "${SSH[@]}" 'ls -l ~/.config/monitors.xml 2>&1; cat ~/.config/monitors.xml 2>&1' 2>&1

  echo
  echo "--- [4c] guest: recent gnome-shell/mutter display journal ---"
  "${SSH[@]}" 'export XDG_RUNTIME_DIR=/run/user/$(id -u);
    journalctl --user -b -n 25 --no-pager 2>&1 | tail -25;
    echo "-- kernel drm --";
    journalctl -k -b --no-pager 2>&1 | grep -iE "drm|edid|virtio_gpu" | tail -15' 2>&1
} >"$F" 2>&1

echo "$F"
