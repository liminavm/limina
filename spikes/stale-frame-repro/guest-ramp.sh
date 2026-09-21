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
# The fix is to make the content itself a counter. Each tick burns a different amount of CPU, so no
# two ticks in a graph's window look alike, the graph becomes a rising staircase whose steps are
# ordered, and the burner's own row in the process table carries the same step a second time. A
# frame that matches an earlier one then cannot be a coincidence, and how far the picture fell back
# is readable off the step.
#
# The log line per tick is the ground truth to correlate against -- the signal is only as good as
# the record of when each step was asked for.
set -u
PERIOD_MS="${RAMP_PERIOD_MS:-1000}"
STEPS="${RAMP_STEPS:-57}"   # coprime-ish with a 60-sample graph so the window rarely aligns

burn() {  # busy-loop for $1 milliseconds
  local end=$(( $(date +%s%N) / 1000000 + $1 ))
  while [ "$(( $(date +%s%N) / 1000000 ))" -lt "$end" ]; do :; done
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
