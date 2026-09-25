#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Does the pin's guest reboot cost vk-replay? The pin pass read vk-replay ~10% and glmark2 ~3% below
# the 2026-09-23 pass on both pins, and its guests were measured after a reboot (a relaunched
# worker) where 09-23's ran on a first boot. Hold the tree at the baseline pair and move only how
# the display pin is applied:
#   s  PIN=session  restart the seated session; the first boot's worker is the one measured
#   r  PIN=reboot   the pin pass's vehicle
# Order s0 r0 s1, the session arm at both ends. Enhanced guest only: every instrument in question
# runs there.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/pin-2026-09-24
BASE_V=b900066b08385a3cb265d21f20fe68dc6c81ca7f
BASE_L=32cc37762187c4e9ebb8c0ad5d46af17c2e2b27b
POINTS=("s0-b900066 session" "r0-b900066 reboot" "s1-b900066 session")
for p in "${POINTS[@]}"; do
  read -r label pin <<<"$p"
  echo "######## $label $(date +%H:%M:%S)"
  PIN=$pin ENH_ONLY=1 $D/point.sh "$label" "$BASE_V" "$BASE_L" || echo "######## $label ABORTED rc=$?"
done
git -C third_party/virglrs checkout -q main && echo "virglrs restored to $(git -C third_party/virglrs rev-parse --short HEAD)"
git -C third_party/libkrun checkout -q limina && echo "libkrun restored to $(git -C third_party/libkrun rev-parse --short HEAD)"
echo "######## legs done $(date +%H:%M:%S)"
