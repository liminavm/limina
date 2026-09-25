#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Score the 2026-09-24 pin against the one before it, as one A/B.
#   b  b900066 + 32cc3776   the previous pin, the baseline
#   n  828a05b + 26450d76   the new pin
#
# Order is b0 n0 b1 n1 b2 -- alternating, with the baseline measured at BOTH ENDS, because the
# host's renderer speed drifts for tens of minutes and a one-ended sweep books that drift as the
# treatment's effect (perf/virglrs-vrend-2026-09-17/legs.sh).
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/pin-2026-09-24/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/pin-2026-09-24
BASE_V=b900066b08385a3cb265d21f20fe68dc6c81ca7f
BASE_L=32cc37762187c4e9ebb8c0ad5d46af17c2e2b27b
NEW_V=828a05bd51111f8ed972d52445dcef3ac2688285
NEW_L=26450d76bc810c6ee0ca7c4ed6ea887bc4910823
POINTS=(
  "b0-b900066 $BASE_V $BASE_L"
  "n0-828a05b $NEW_V $NEW_L"
  "b1-b900066 $BASE_V $BASE_L"
  "n1-828a05b $NEW_V $NEW_L"
  "b2-b900066 $BASE_V $BASE_L"
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
