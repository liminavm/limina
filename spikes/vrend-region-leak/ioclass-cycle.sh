#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Step 1 of the region-leak hunt: which IOKit KERNEL class is ratcheting?
#
# WHY THIS BEFORE vmmap. The per-Objective-C-class census proved the leak lives BELOW the ObjC
# object layer (no minted class exceeds ~1.2k live against +29 000 regions per cycle), so the next
# question is what an `IOAccelerator (graphics)` region actually IS. `ioclasscount` answers that
# from the kernel side and costs SECONDS per sample, against `vmmap`'s MINUTES once the region
# count is high — so it is the cheap instrument to run first. A kernel class ratcheting ~10k per
# cycle names the leaked object outright.
#
# ⚠ THE ONE TRAP: `ioclasscount` is SYSTEM-WIDE, not per-process. WindowServer, browsers and every
# other GPU client on the host contribute to the same counters. So this script always records a
# HOST-ONLY control (run it with `baseline` before booting any VM, and again after shutdown) and
# every stage snapshot is kept raw, so host drift can be subtracted rather than assumed away. A
# class that moves by tens while ours moves by thousands is noise; do not read small deltas.
#
# Usage:
#   spikes/vrend-region-leak/ioclass-cycle.sh baseline      # BEFORE booting the VM
#   ... boot the guest, seat the desktop ...
#   CYCLES=2 spikes/vrend-region-leak/ioclass-cycle.sh run
#   spikes/vrend-region-leak/ioclass-cycle.sh baseline-after # AFTER shutdown
#   spikes/vrend-region-leak/ioclass-cycle.sh report
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
RUN=${RUN:-/tmp/ioclass-run}
mkdir -p "$RUN"
MODE=${1:-run}
CYCLES=${CYCLES:-2}
SSH="ssh -p ${LIMINA_SSH_PORT:-2222} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1"

# Largest-RSS match, never `head -1`: several processes match this argv prefix and picking a small
# one reports a ~1.6 MB footprint with no IOAccelerator lines — a snapshot that reads like
# "nothing allocated" rather than like a mismeasurement.
worker_pid() {
  local pids
  pids=$(pgrep -f 'limina-vmm --cpus' | tr '\n' ',' | sed 's/,$//')
  [ -n "$pids" ] || { echo ""; return; }
  ps -o pid=,rss= -p "$pids" | sort -k2 -rn | head -1 | awk '{print $1}'
}

# One stage = the kernel class table + our own region anchor, so the two can be correlated after
# the fact. Both are cheap; the vmmap here is --summary (seconds), not -v (minutes).
snap() {
  local tag=$1
  ioclasscount > "$RUN/$tag.ioclass" 2>/dev/null
  local pid; pid=$(worker_pid)
  if [ -n "$pid" ]; then
    vmmap --summary "$pid" 2>/dev/null \
      | grep -E "^IOAccelerator \(graphics\)|^IOSurface|Physical footprint:" > "$RUN/$tag.vm"
  else
    : > "$RUN/$tag.vm"
  fi
  printf '%s  regions=%s\n' "$tag" \
    "$(awk '/^IOAccelerator \(graphics\)/{print $NF}' "$RUN/$tag.vm" 2>/dev/null | head -1)"
}

launch() {
  $SSH "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
        systemctl --user stop ff-bench 2>/dev/null; sleep 3
        systemd-run --user --unit=ff-bench --setenv=WAYLAND_DISPLAY=wayland-0 \
          --setenv=MOZ_ENABLE_WAYLAND=1 --setenv=MOZ_DISABLE_GPU_SANDBOX=1 \
          --setenv=XDG_RUNTIME_DIR=/run/user/1000 ${AQ_EXTRA_ENV:-} \
          /usr/bin/firefox --kiosk 'https://webglsamples.org/aquarium/aquarium.html?numFish=5000'" >/dev/null 2>&1
  sleep 40
}
stopff() {
  $SSH "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
        systemctl --user stop ff-bench 2>/dev/null; true" >/dev/null 2>&1
  sleep 30
}

# Diff two class tables. Prints only classes that MOVED, biggest mover first, so a ratchet of
# thousands cannot hide in 700 lines of flat counters.
diff_tables() {
  local a=$1 b=$2 label=$3
  echo "=== $label ==="
  join -j 1 -o 0,1.2,2.2 \
    <(sed 's/ = /|/' "$a" | tr '|' ' ' | sort -k1,1) \
    <(sed 's/ = /|/' "$b" | tr '|' ' ' | sort -k1,1) 2>/dev/null \
  | awk '{d=$3-$2; if (d!=0) printf "%+9d  %-46s %8d -> %8d\n", d, $1, $2, $3}' \
  | sort -k1,1 -rn | head -25
}

case "$MODE" in
  baseline)       snap "host-pre" ;;
  baseline-after) snap "host-post" ;;
  run)
    snap "c0-closed"
    for i in $(seq 1 "$CYCLES"); do
      launch;  snap "c$i-open"
      stopff;  snap "c$i-closed"
    done
    ;;
  report)
    # Closed-to-closed is the only honest comparison — within a cycle the close LOOKS like it
    # releases, which hides the ratchet completely.
    for i in $(seq 1 "$CYCLES"); do
      prev=$((i-1))
      [ -f "$RUN/c$prev-closed.ioclass" ] && [ -f "$RUN/c$i-closed.ioclass" ] || continue
      diff_tables "$RUN/c$prev-closed.ioclass" "$RUN/c$i-closed.ioclass" \
                  "closed c$prev -> closed c$i (THE RATCHET)"
    done
    if [ -f "$RUN/host-pre.ioclass" ] && [ -f "$RUN/host-post.ioclass" ]; then
      diff_tables "$RUN/host-pre.ioclass" "$RUN/host-post.ioclass" \
                  "HOST CONTROL: pre-boot -> post-shutdown (what did NOT come back)"
    fi
    echo "=== region anchor ==="
    for f in "$RUN"/*.vm; do
      [ -s "$f" ] || continue
      printf '%-14s %s\n' "$(basename "$f" .vm)" \
        "$(awk '/^IOAccelerator \(graphics\)/{printf "regions=%-8s size=%-8s", $NF, $(NF-1)}
                /Physical footprint:/{printf "footprint=%s", $3}' "$f")"
      echo
    done
    ;;
  *) echo "usage: $0 {baseline|run|baseline-after|report}"; exit 1 ;;
esac
