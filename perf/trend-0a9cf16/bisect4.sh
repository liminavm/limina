#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Fourth round: where does Basemark WebGL 2.0 step down? Every boot through p05 read 4255-4713 and
# every boot from p07 on 3920-4149; p06 (4271) and 1fd19c2 (3930, 4267) straddle. Stock runs at p06
# (4bfdefe) and at 39b0c68 ("a fence with no work of its own rides the queue"), the first of the two
# fence commits in 4bfdefe..1fd19c2, alternated so host drift lands on both. The Basemark site drops
# some result pages, so each tree is run until it has three scored runs, at most five attempts.
# Safe to re-run: attempts already in evidence/ count.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
EV=perf/trend-0a9cf16/evidence
LK=0dba6b9c4029bd75e5f668ffb33942c39061e95d   # the libkrun both trees were pinned with
run() { echo "######## $1"; STOCK_ONLY=1 perf/trend-0a9cf16/point.sh "$1" "$2" "$LK" || echo "######## $1 ABORTED rc=$?"; }
scored() { grep -l 'scores (run 2)' "$EV"/w*-"$1"/basemark.txt 2>/dev/null | wc -l | tr -d ' '; }
tried() { ls -d "$EV"/w*-"$1" 2>/dev/null | wc -l | tr -d ' '; }
more=1
while [ "$more" = 1 ]; do
  more=0
  for rev in 4bfdefe 39b0c68; do
    if [ "$(scored $rev)" -lt 3 ] && [ "$(tried $rev)" -lt 5 ]; then
      run "w$(( $(tried $rev) + 1 ))-$rev" "$rev"; more=1
    fi
  done
done
for rev in 4bfdefe 39b0c68; do echo "######## $rev: $(scored $rev) scored of $(tried $rev)"; done
echo "######## bisect4 done"
