#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Score the virglrs 828a05b -> c76b2c7 bump as one A/B, libkrun held at 60746d3e on both arms.
#   b  828a05b + 60746d3e   the previous virglrs pin, the baseline
#   n  c76b2c7 + 60746d3e   the new virglrs pin
#
# Order is b0 n0 b1 n1 b2 -- alternating, with the baseline measured at BOTH ENDS, because the
# host's renderer speed drifts for tens of minutes and a one-ended sweep books that drift as the
# treatment's effect (perf/virglrs-vrend-2026-09-17/legs.sh).
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/virglrs-2026-09-26/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/virglrs-2026-09-26
BASE_V=828a05bd51111f8ed972d52445dcef3ac2688285
BASE_L=60746d3e7b176f4d0bedf1527677f073d1309eb6
NEW_V=c76b2c79def106bd38426f80d0b017e7eb0a0f1d
NEW_L=$BASE_L
POINTS=(
  "b0-828a05b $BASE_V $BASE_L"
  "n0-c76b2c7 $NEW_V $NEW_L"
  "b1-828a05b $BASE_V $BASE_L"
  "n1-c76b2c7 $NEW_V $NEW_L"
  "b2-828a05b $BASE_V $BASE_L"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label vrev lrev <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
# point.sh leaves both trees detached at the LAST leg, which is the baseline -- so a later
# `cargo xtask build` would quietly build the old pair. Put them back on their branches.
git -C third_party/virglrs checkout -q main && echo "virglrs restored to $(git -C third_party/virglrs rev-parse --short HEAD)"
git -C third_party/libkrun checkout -q limina && echo "libkrun restored to $(git -C third_party/libkrun rev-parse --short HEAD)"
echo "######## legs done $(date +%H:%M:%S)"
