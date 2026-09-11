#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Third round: where does Basemark's scored-run stall end? It hit p03, p04 and p06 (roughly half the
# boots) and none of p07-p10 on one boot each. First two more stock runs at p07 to check that clean
# result is not luck; only if both score, two at 1fd19c2 ("a fence covers every queue its work could
# be on"), the fence change inside p06..p07, to see which side of the window it falls on.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
EV=perf/trend-0a9cf16/evidence
run() { echo "######## $1"; STOCK_ONLY=1 perf/trend-0a9cf16/point.sh "$1" "$2" "$3" || echo "######## $1 ABORTED rc=$?"; }
scored() { grep -q 'basemark-shader\|Shader Pipeline Test' "$EV/$1/basemark.txt" 2>/dev/null && ! grep -q 'REFUSING' "$EV/$1/basemark.txt"; }
run s8-p07 4654a345faab18d908736f962a1aa1ac8b655a36 bae5de4ab86ebd4087468fdefe4ee105099ad397
run s9-p07 4654a345faab18d908736f962a1aa1ac8b655a36 bae5de4ab86ebd4087468fdefe4ee105099ad397
if scored s8-p07 && scored s9-p07; then
  run s10-1fd19c2 1fd19c2 0dba6b9c4029bd75e5f668ffb33942c39061e95d
  run s11-1fd19c2 1fd19c2 0dba6b9c4029bd75e5f668ffb33942c39061e95d
else
  echo "######## p07 did not score on both repeats; skipping the 1fd19c2 probe"
fi
echo "######## bisect3 done"
