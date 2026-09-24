#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Flush latency with video playing, synchronous against asynchronous decode, alternated across
# boots so host drift lands on both arms. `s` is virglrs 1dd26d3 (phase 1: decode inside
# END_FRAME, with the delay knob) + libkrun f393db7f, built into target/lat-sync; `a` is the
# current tree (decode on its own thread) in target/lat-async. Each runs at real decode speed and
# with every decode made 15 ms late, which stays inside a 30 fps clip's 33 ms frame budget.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
for round in 1 2; do
  for p in "s0 target/lat-sync 0" "a0 target/lat-async 0" "s15 target/lat-sync 15" "a15 target/lat-async 15"; do
    read -r arm dir delay <<<"$p"
    echo "######## $arm-r$round $(date +%H:%M:%S)"
    spikes/flush-latency/point.sh "$arm-r$round" "$dir" "$delay" video || echo "######## $arm-r$round ABORTED rc=$?"
  done
done
echo "######## done $(date +%H:%M:%S)"
