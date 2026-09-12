#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Third round: where does Basemark's scored-run stall end? It hit p03, p04 and p06 (roughly half the
# boots) and none of p07-p10 on one boot each. First two more stock runs at p07 to check that clean
# result is not luck; only if both score, two at fd17672 ("a fence covers every queue its work could
# be on"), the fence change inside p06..p07, to see which side of the window it falls on.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
EV=perf/trend-d5889ef/evidence
run() { echo "######## $1"; STOCK_ONLY=1 perf/trend-d5889ef/point.sh "$1" "$2" "$3" || echo "######## $1 ABORTED rc=$?"; }
scored() { grep -q 'basemark-shader\|Shader Pipeline Test' "$EV/$1/basemark.txt" 2>/dev/null && ! grep -q 'REFUSING' "$EV/$1/basemark.txt"; }
run s8-p07 02d047be22d96737b087be49a12ab89c80e12e58 bae5de4ab86ebd4087468fdefe4ee105099ad397
run s9-p07 02d047be22d96737b087be49a12ab89c80e12e58 bae5de4ab86ebd4087468fdefe4ee105099ad397
if scored s8-p07 && scored s9-p07; then
  run s10-fd17672 fd17672 0dba6b9c4029bd75e5f668ffb33942c39061e95d
  run s11-fd17672 fd17672 0dba6b9c4029bd75e5f668ffb33942c39061e95d
else
  echo "######## p07 did not score on both repeats; skipping the fd17672 probe"
fi
echo "######## bisect3 done"
