#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Score asynchronous hardware video decode (virglrs 9c5b3e2..d57be22, libkrun f393db7f..32cc3776)
# against the pin, as one A/B.
#   b  9c5b3e2 + f393db7f   the pin, the baseline
#   n  d57be22 + 32cc3776   decode on a thread per codec, settle barriers, settle_video on snapshot
#
# Order is b0 n0 b1 n1 b2 -- alternating, with the baseline measured at BOTH ENDS, because the
# host's renderer speed drifts for tens of minutes and a one-ended sweep books that drift as the
# treatment's effect (perf/virglrs-vrend-2026-09-17/legs.sh).
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/async-decode-2026-09-23/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/async-decode-2026-09-23
BASE_V=9c5b3e2bf9b1ec3543c50b19241905e56b8e1fa7
BASE_L=f393db7f5a702256d546c2c50ae18bd8f1d36b7f
NEW_V=d57be222ffb06d081d958a88145aee8f0199e1cc
NEW_L=32cc37762187c4e9ebb8c0ad5d46af17c2e2b27b
POINTS=(
  "b0-9c5b3e2 $BASE_V $BASE_L"
  "n0-d57be22 $NEW_V $NEW_L"
  "b1-9c5b3e2 $BASE_V $BASE_L"
  "n1-d57be22 $NEW_V $NEW_L"
  "b2-9c5b3e2 $BASE_V $BASE_L"
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
