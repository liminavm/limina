#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The same measurement at real decode speed across frame sizes: how long a decode takes on its
# thread (the bound on how long it can hold back another context's fence), and whether that
# holding back shows at any size. `a` is asynchronous decode with the phase timing (virglrs
# 76b1364, target/lat-async); `s` is synchronous decode (1dd26d3, target/lat-sync), at 4K only,
# for contrast. Two rounds, alternated, one boot per point.
set -uo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
# ARMS narrows the set (e.g. ARMS="a720 a1080 a2160"); LABEL_SUFFIX keeps a rerun's evidence apart.
ARMS="${ARMS:-a720 a1080 a2160 s2160}"
point() { # <arm> -> "<worker-dir> <resolution>"; a case, since macOS bash 3.2 has no maps
  case "$1" in
    a720) echo "target/lat-async 1280x720" ;; a1080) echo "target/lat-async 1920x1080" ;;
    a2160) echo "target/lat-async 3840x2160" ;; s2160) echo "target/lat-sync 3840x2160" ;;
  esac
}
for round in 1 2; do
  for arm in $ARMS; do
    p="$arm $(point "$arm")"
    read -r arm dir res <<<"$p"
    L="res-$arm-r$round${LABEL_SUFFIX:-}"
    echo "######## $L $(date +%H:%M:%S)"
    CLIP_RES="$res" spikes/flush-latency/point.sh "$L" "$dir" 0 video || echo "######## $L ABORTED rc=$?"
  done
done
echo "######## done $(date +%H:%M:%S)"
