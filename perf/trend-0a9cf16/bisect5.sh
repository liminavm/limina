#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Fifth round: narrow Basemark WebGL 2.0's step inside p05..p06 (feef548..4bfdefe). p05 read 4674
# on its one boot, p06 4271 and 3813, 39b0c68 4185-4260 on three. WebGL 2.0 spreads by hundreds of
# points on one tree, so every tree named here is run until it has three scored runs.
#
# Usage: perf/trend-0a9cf16/bisect5.sh <rev>...   (short virglrs revs; attempts already in
# evidence/ count, so a re-run resumes). The Linux port changed virglrs's Rust API partway through
# this range and only libkrun 0dba6b9c follows it, so each tree builds against 7c4ada0 first and
# 0dba6b9c when that fails to build (the failed build keeps its own evidence directory). Trees in
# 4a10f9b..17214e8 build against neither; run those with LIBKRUN_PATCH=perf/trend-0a9cf16/libkrun-
# 7c4ada0-capset-bridge.patch, which point.sh applies to 7c4ada0. Every tree is capped at 2*MAX point
# runs in all, so a tree that keeps failing before Basemark (build, patch, boot) cannot loop forever.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
EV=perf/trend-0a9cf16/evidence
OLD=$(git -C third_party/libkrun rev-parse 7c4ada05)
NEW=$(git -C third_party/libkrun rev-parse 0dba6b9c)
MAX=${MAX_ATTEMPTS:-6}
scored() { grep -l 'scores (run 2)' "$EV"/w*-"$1"/basemark.txt 2>/dev/null | wc -l | tr -d ' '; }
tried() { ls "$EV"/w*-"$1"/basemark.txt 2>/dev/null | wc -l | tr -d ' '; }
runs() { ls -d "$EV"/w*-"$1" 2>/dev/null | wc -l | tr -d ' '; }
next() { local n=1; while [ -e "$EV/w$n-$1" ]; do n=$((n + 1)); done; echo "w$n-$1"; }
point() { # <label> <rev> <libkrun>
  echo "######## $1 (libkrun ${3:0:8})"; STOCK_ONLY=1 perf/trend-0a9cf16/point.sh "$1" "$2" "$3"
}
attempt() { # <rev>; returns 2 when the tree builds against neither libkrun
  point "$(next "$1")" "$1" "$OLD"; local rc=$?
  [ "$rc" = 2 ] && { point "$(next "$1")" "$1" "$NEW"; rc=$?; }
  [ "$rc" = 0 ] || echo "######## $1 ABORTED rc=$rc"
  [ "$rc" = 2 ] && return 2 || return 0
}
revs=("$@"); more=1
while [ "$more" = 1 ]; do
  more=0
  for i in "${!revs[@]}"; do
    rev=${revs[$i]}
    if [ "$(scored "$rev")" -lt 3 ] && [ "$(tried "$rev")" -lt "$MAX" ] && [ "$(runs "$rev")" -lt $((2 * MAX)) ]; then
      if attempt "$rev"; then more=1; else echo "######## $rev builds against neither libkrun; dropped"; unset "revs[$i]"; fi
    fi
  done
done
for rev in "${revs[@]}"; do echo "######## $rev: $(scored "$rev") scored of $(tried "$rev")"; done
echo "######## bisect5 done"
