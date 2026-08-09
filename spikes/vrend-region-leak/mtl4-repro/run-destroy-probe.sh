#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Build + run the allocator-destroy efficacy probe. See destroy-probe.m for what it tests and
# for the four ways it could lie.
#
# Each MODE runs as a SEPARATE PROCESS so no phase contaminates another's measurements.
#
#   run-destroy-probe.sh              # all modes, render class (the one that matters)
#   MODE=a run-destroy-probe.sh       # just one
#   CLASS=compute run-destroy-probe.sh
set -eu
cd "$(dirname "$0")"
BIN=/tmp/destroy-probe
# -fno-objc-arc: explicit retain/release, matching the bridge this mirrors — and the probe's
# whole point is controlling exactly when objects die.
clang -fno-objc-arc -O1 -g -o "$BIN" destroy-probe.m \
  -framework Foundation -framework Metal -framework QuartzCore

if [ -n "${MODE:-}" ]; then
  exec "$BIN"
fi

for m in a b c d big; do
  echo
  echo "################################################################ MODE=$m"
  MODE=$m "$BIN" || echo "  (mode $m exited $?)"
done
