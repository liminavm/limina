#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Suspend/resume timing cycles on a flat --disk run, with no clicks: each cycle launches the
# boot vehicle (which auto-resumes a pending snapshot), waits for a real ssh login, suspends
# through the supervisor (SIGTSTP — the menu path), waits for the window to park, then quits
# the parked supervisor so the next launch restores again.
#
#   spikes/suspend-perf/cycle.sh <disk.raw> <outdir> [cycles] [settle-secs]
#
# The disk must already have a live session worth measuring (boot it once and load it up); the
# first cycle cold-boots if no snapshot is pending. Timing lines from every cycle's worker log
# are summarised at the end. Uses the release binaries: debug timings run ~5x slow.
set -u
cd "$(dirname "$0")/../.."
DISK=$(cd "$(dirname "$1")" && pwd)/$(basename "$1"); OUT=$2; CYCLES=${3:-3}; SETTLE=${4:-20}
mkdir -p "$OUT"; OUT=$(cd "$OUT" && pwd)
SNAP="$DISK.limina-suspend.bin"

for c in $(seq 1 "$CYCLES"); do
  LOG="$OUT/cycle$c.log"
  [ -e "$SNAP" ] && echo "cycle $c: resuming $(du -h "$SNAP" | cut -f1) snapshot" \
                 || echo "cycle $c: no snapshot pending, cold boot"
  RUST_LOG=warn,limina=info,limina_vmm=info,krun_vmm=info,krun_devices=info \
    LIMINA_WINDOW_CAPTURE="$OUT/window.png" LIMINA_BOOT_LOG="$LOG" \
    LIMINA_BIN=target/release/limina LIMINA_VMM_BIN=target/release/limina-vmm \
    LIMINA_DISK="$DISK" LIMINA_RAM_MIB=${LIMINA_RAM_MIB:-10240} \
    LIMINA_EXTRA_ARGS="--ssh-port ${SSH_PORT:-2299}" \
    spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$OUT/boot$c.out" 2>&1 &
  until SUP=$(grep -oE 'limina pid=[0-9]+' "$OUT/boot$c.out" 2>/dev/null | cut -d= -f2) && [ -n "$SUP" ]; do
    sleep 0.2
  done
  if ! scripts/wait-guest-ssh.sh "$LOG" 300 "$SUP" > /dev/null; then
    echo "cycle $c: guest never came up; see $LOG"; kill -TERM "$SUP"; exit 1
  fi
  sleep "$SETTLE"
  N=$(wc -l < "$LOG" | tr -d ' ')
  kill -TSTP "$SUP"
  until tail -n +"$N" "$LOG" | grep -q "window parked"; do
    kill -0 "$SUP" 2>/dev/null || { echo "cycle $c: supervisor died during suspend"; exit 1; }
    sleep 0.1
  done
  kill -TERM "$SUP"
  while kill -0 "$SUP" 2>/dev/null; do sleep 0.2; done
  [ -e "$SNAP" ] || { echo "cycle $c: no snapshot after suspend"; exit 1; }
done

for c in $(seq 1 "$CYCLES"); do
  echo "== cycle $c"
  grep -hE "restore: (read|applied)|released DRIVER_OK|restore: first frame|suspend: (all|the guest|asking)|bracket: (SIGTSTP|guest quiesced)|GPU re-creation|snapshot written|splash save" \
    "$OUT/cycle$c.log" | sed -E 's/^\[([^]]*)\] */\1 /; s/ -> \/.*//' | cut -c1-200
done
