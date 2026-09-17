#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Score virglrs's 2026-09-17 vrend commits (af70f7c..9041fa2, 6 commits: a typed transfer
# direction and compressed-copy rule, texture views taken only of a driver-witnessed immutable
# texture, a texture's storage folded into one field, a blit's views released from one scope, and
# the classic replay span refusing a journal fed outside it) against the pin, as one A/B.
#   b  af70f7c + 02498445   the pin, the baseline
#   n  9041fa2 + 217ba058   the six vrend commits
#
# One fork commit covers six: only 130d7a1 is visible to rutabaga, because it typed the signature
# rutabaga calls. 217ba058 follows it and is parented directly on 02498445.
#
# WHICH INSTRUMENTS CAN SEE THIS: every commit is vrend. On the enhanced guest glmark2, gl-replay
# and vk-replay all run zink->venus and never enter vrend, so their flatness is arithmetic, not
# evidence -- they are here to catch collateral damage, not to score the change. The instruments
# that actually exercise vrend are Basemark on the STOCK guest and the WebGL aquarium. Read those.
#
# Order is b0 n0 b1 n1 b2 -- alternating, and the baseline is measured at BOTH ENDS. The host's
# renderer speed drifts for tens of minutes at a time, and on 2026-09-17 Geometry Stress fell
# 1730 -> 1613 across a 2.5 h sweep with both low points on the BASELINE; a one-ended sweep books
# that drift as the treatment's regression. Every n has a b on either side of it.
#
# An aborted point is logged and the run moves on; read the log for ABORTED, WEDGED and WATCHDOG.
# Usage: perf/virglrs-vrend-2026-09-17/legs.sh [first-label]   (resume from a label)
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/virglrs-vrend-2026-09-17
BASE_V=af70f7c4f11db24c26bc86ca268960211e1ba521
BASE_L=024984459a7b005c1dd2dd92ada0239a9f367c3a
NEW_V=9041fa2f3f5e0a861e91d089ad278bbf71943839
NEW_L=217ba0586ebe4d883f52e9beb7547ad19bce3875
POINTS=(
  "b0-af70f7c $BASE_V $BASE_L"
  "n0-9041fa2 $NEW_V $NEW_L"
  "b1-af70f7c $BASE_V $BASE_L"
  "n1-9041fa2 $NEW_V $NEW_L"
  "b2-af70f7c $BASE_V $BASE_L"
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
# working with before anything else builds. Both DETACH: the fork rev under test lives on
# limina-transfer-direction, which is checked out in another worktree, and git refuses to check
# out a branch twice -- so `checkout limina` here would silently restore the WRONG libkrun.
git -C third_party/virglrs checkout -q --detach "$NEW_V" && echo "virglrs restored to $(git -C third_party/virglrs rev-parse --short HEAD)"
git -C third_party/libkrun checkout -q --detach "$NEW_L" && echo "libkrun restored to $(git -C third_party/libkrun rev-parse --short HEAD)"
echo "######## legs done $(date +%H:%M:%S)"
