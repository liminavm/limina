#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Drive a monitor's graphs with a signal that never repeats, so any repeated frame is staleness.
#
# Run this in the guest next to a `btm`:
#
#   spikes/stale-frame-repro/guest-ramp.sh > ~/ramp.log
#
# Counting stale frames by hashing a monitor's own output does not work, because a monitor's
# output recurs: a panel reading `0.3%` matches every other frame reading `0.3%`, and a flat
# dotted graph scrolling one column per tick matches itself at a fixed distance. Both produce a
# rate out of nothing, and the fixed distance is the more dangerous of the two because it reads
# like a mechanism.
#
# The fix is to make the content itself a counter. Each tick burns a different amount of CPU on
# every vCPU, so no two ticks in a graph's window look alike, the graph becomes a rising staircase
# spanning its full height whose steps are ordered, and the burners' rows in the process table
# carry the same step a second time. A
# frame that matches an earlier one then cannot be a coincidence, and how far the picture fell back
# is readable off the step.
#
# The log line per tick is the ground truth to correlate against -- the signal is only as good as
# the record of when each step was asked for.
set -u
PERIOD_MS="${RAMP_PERIOD_MS:-1000}"
STEPS="${RAMP_STEPS:-57}"   # coprime-ish with a 60-sample graph so the window rarely aligns

# One burner per vCPU, each pinned. A single spinner can only reach 1/ncpu of the aggregate
# graph, which squashes the whole ramp into the bottom rows where the steps stop being
# distinguishable -- and an oracle whose steps are not distinguishable is back to matching on
# recurrence, which is the thing it exists to avoid.
NCPU=$(nproc)
burn() {  # busy-loop every vCPU for $1 milliseconds
  local ms="$1" cpu
  [ "$ms" -le 0 ] && return
  for cpu in $(seq 0 $(( NCPU - 1 ))); do
    taskset -c "$cpu" bash -c '
      end=$(( $(date +%s%N) / 1000000 + '"$ms"' ))
      while [ "$(( $(date +%s%N) / 1000000 ))" -lt "$end" ]; do :; done' &
  done
  wait
}

n=0
while true; do
  step=$(( n % STEPS ))
  # CPU: 0..~85% of the period, in a rising ramp that resets.
  ms=$(( step * PERIOD_MS * 85 / 100 / STEPS ))
  printf '%s tick=%d step=%d cpu_ms=%d\n' "$(date '+%Y-%m-%d %H:%M:%S.%3N')" "$n" "$step" "$ms"
  start=$(( $(date +%s%N) / 1000000 ))
  burn "$ms"
  now=$(( $(date +%s%N) / 1000000 ))
  rest=$(( PERIOD_MS - (now - start) ))
  [ "$rest" -gt 0 ] && sleep "$(printf '0.%03d' "$rest")"
  n=$(( n + 1 ))
done
