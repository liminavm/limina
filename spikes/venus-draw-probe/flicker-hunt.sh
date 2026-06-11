#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Hunt for a "flickery session" of the transition-flicker bug (RESULTS.md round 20).
#
# Theory under test: whether a session exhibits the window-content-missing flicker is
# decided per session/boot (one boot reproduced it repeatedly; the next boot was clean
# for 25+ min on the same workloads, probe-verified). This script boot-cycles the
# seated-KK guest with the LIMINA_RED_PROBE oracle armed and the 2s-transition glmark2
# loop running, counts bleed frames objectively, archives each session's worker log,
# and stops when it catches a flickery session — which can then be diffed against the
# archived clean session (evidence/worker-clean-session-2026-06-11.log.gz):
# format/modifier negotiation, context lineup, scanout config are all in the log.
#
# Needs: a TEMPLATE disk prepared with the solid-red wallpaper + ydotool installed
# (default /tmp/seated-kk-hunt-template.raw — clone of a provisioned seated-kk run).
# The probe needs the workload window over the red desktop, so each cycle escapes the
# login overview with ydotool before starting the loop.
#
# Usage: ./flicker-hunt.sh [max_cycles]   (default 12; ~6 min per cycle)
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
TEMPLATE=${TEMPLATE:-/tmp/seated-kk-hunt-template.raw}
OUT=${OUT:-/tmp/flicker-hunt}
MAX=${1:-12}
SSH="ssh -p 2222 -o ConnectTimeout=2 -o StrictHostKeyChecking=no claude@127.0.0.1"
mkdir -p "$OUT"

[ -f "$TEMPLATE" ] || { echo "no template disk $TEMPLATE"; exit 1; }

for cycle in $(seq 1 "$MAX"); do
  echo "=== cycle $cycle/$MAX $(date +%H:%M:%S) ==="
  kill $(pgrep limina-vmm) 2>/dev/null; sleep 2; pkill -9 -f limina-vmm 2>/dev/null
  pkill -f gvproxy 2>/dev/null; sleep 1
  rm -f /tmp/seated-kk-hunt.raw
  cp -c "$TEMPLATE" /tmp/seated-kk-hunt.raw || { echo CLONE_FAIL; exit 1; }

  LIMINA_DISK=/tmp/seated-kk-hunt.raw LIMINA_RED_PROBE=1 LIMINA_KK_STATS=1 \
    ./spikes/venus-draw-probe/boot-seated-kk.sh >"$OUT/boot-$cycle.log" 2>&1 &

  for i in $(seq 1 60); do
    $SSH 'test -S /run/user/1000/wayland-0' 2>/dev/null && break
    sleep 5
  done
  $SSH 'test -S /run/user/1000/wayland-0' 2>/dev/null || { echo "cycle $cycle: BOOT TIMEOUT"; continue; }

  # Escape the login overview (probe is blind there: bleed shows the dimmed overview
  # backdrop, not the red wallpaper). Harmless if already on the desktop.
  $SSH 'sudo systemd-run --unit=ydotoold --quiet ydotoold 2>/dev/null; sleep 2; sudo ydotool key 1:1 1:0' 2>/dev/null
  sleep 2

  $SSH 'systemd-run --user --unit=flicker-test --setenv=WAYLAND_DISPLAY=wayland-0 glmark2-es2-wayland -b texture:duration=2 -b shading:duration=2 --run-forever' || { echo "cycle $cycle: LAUNCH FAIL"; continue; }

  sleep 75 # window map + baseline climb settle
  H0=$(grep -c REDPROBE /tmp/seated-kk-worker.log)
  sleep 180
  H1=$(grep -c REDPROBE /tmp/seated-kk-worker.log)
  DELTA=$((H1 - H0))
  echo "cycle $cycle: probe delta=$DELTA over 3min (~90 transitions)"
  gzip -c /tmp/seated-kk-worker.log > "$OUT/worker-cycle$cycle-delta$DELTA.log.gz"

  if [ "$DELTA" -gt 5 ]; then
    echo "*** FLICKERY SESSION CAUGHT (cycle $cycle, delta=$DELTA) — VM left running for inspection ***"
    exit 0
  fi
done
echo "no flickery session in $MAX cycles — per-session theory needs revisiting"
