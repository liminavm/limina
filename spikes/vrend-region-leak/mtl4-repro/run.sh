#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Build + run the standalone MTL4 region-ratchet repro. See mtl4cycle.m for what it tests.
#
#   spikes/vrend-region-leak/mtl4-repro/run.sh                 # baseline, KK's shape
#   NOHANDLER=1 spikes/vrend-region-leak/mtl4-repro/run.sh      # one A/B knob at a time
set -eu
cd "$(dirname "$0")"
BIN=/tmp/mtl4cycle
# -fno-objc-arc: the code retains/releases explicitly, matching the bridge it mirrors.
clang -fno-objc-arc -O1 -g -o "$BIN" mtl4cycle.m \
  -framework Foundation -framework Metal -framework QuartzCore
exec "$BIN"
