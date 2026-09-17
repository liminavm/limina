#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Score virglrs's 2026-09-17 robustness commits (d833259..af70f7c, 8 commits: unified journal and
# replay entry points, a typed renderer witness, Current::switch_to, stream depth on Handlers,
# PlaneLayouts) against the pin, as one A/B. Each point is a virglrs + libkrun pair; libkrun
# 02498445 follows the unified journal/replay API and does not compile against the old virglrs,
# so the two move together and the bump will be two pins.
#   b  d833259 + 3fa4cd9b   the pin, the baseline
#   n  af70f7c + 02498445   the eight robustness commits
#
# Order is b0 n0 b1 n1 b2 -- alternating, and the baseline is measured at BOTH ENDS. The host's
# renderer speed drifts for tens of minutes at a time (measured 2026-09-10) and Basemark WebGL 2.0
# fell 4516 -> 3786 across a 5 h sweep with no step (2026-09-16); a one-ended sweep books that drift
# as the treatment's regression. Every n has a b on either side of it.
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/virglrs-robustness-2026-09-17/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/virglrs-robustness-2026-09-17
BASE_V=d833259135fad1d93cc515be4da6e663049e5bd3
BASE_L=3fa4cd9bfeb532d1273237079d6ad2f1bcdfe97e
NEW_V=af70f7c4f11db24c26bc86ca268960211e1ba521
NEW_L=024984459a7b005c1dd2dd92ada0239a9f367c3a
POINTS=(
  "b0-d833259 $BASE_V $BASE_L"
  "n0-af70f7c $NEW_V $NEW_L"
  "b1-d833259 $BASE_V $BASE_L"
  "n1-af70f7c $NEW_V $NEW_L"
  "b2-d833259 $BASE_V $BASE_L"
)
started=${1:+0}; started=${started:-1}
for p in "${POINTS[@]}"; do
  read -r label vrev lrev <<<"$p"
  [ "$started" = 1 ] || { [ "$label" = "$1" ] && started=1 || continue; }
  echo "######## $label $(date +%H:%M:%S)"
  $D/point.sh "$label" "$vrev" "$lrev" || echo "######## $label ABORTED rc=$?"
done
# point.sh leaves both trees detached at the LAST leg, which is the baseline -- so a later
# `cargo xtask build` would quietly build the old pair. Put them back on what the session is
# working with before anything else builds.
git -C third_party/virglrs checkout -q --detach "$NEW_V" && echo "virglrs restored to $(git -C third_party/virglrs rev-parse --short HEAD)"
git -C third_party/libkrun checkout -q limina && echo "libkrun restored to $(git -C third_party/libkrun rev-parse --short HEAD) (limina)"
echo "######## legs done $(date +%H:%M:%S)"
