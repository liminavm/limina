#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Sample the DRM atomic state's cursor planes at ~50 Hz, one line per observation:
#   t=<epoch µs> plane=<N> crtc=<name|null> pos=<x>,<y> size=<WxH> fb=<id|0>
# virtio-gpu exposes two planes per CRTC in pair order — primary (even N), cursor (odd N);
# the state file's plane names carry no type, so odd N IS the cursor selector (verified
# against a live guest: plane-1 = 64x64 AR24 at the pointer's offset).
# Run as root in the guest (debugfs). Ctrl-C to stop; output to stdout.
set -euo pipefail

STATE=""
for f in /sys/kernel/debug/dri/*/state; do
    [ -r "$f" ] || continue
    STATE="$f"
    break
done
[ -n "$STATE" ] || { echo "no readable dri state (root? debugfs?)" >&2; exit 1; }
echo "# sampling $STATE" >&2

while :; do
    t=$(date +%s%6N)
    awk -v t="$t" '
        /^plane\[/ {
            n = $2; sub(/^plane-/, "", n)
            iscursor = (n % 2 == 1); crtc=""; fb=0
            next
        }
        iscursor && /^\tcrtc=/     { crtc=$0; sub(/.*crtc=/, "", crtc) }
        iscursor && /^\tfb=/       { fb=$0; sub(/.*fb=/, "", fb) }
        iscursor && /^\tcrtc-pos=/ {
            pos=$0; sub(/.*crtc-pos=/, "", pos)
            if (match(pos, /^([0-9]+)x([0-9]+)\+(-?[0-9]+)\+(-?[0-9]+)/, m))
                printf "t=%s plane=%s crtc=%s pos=%s,%s size=%sx%s fb=%s\n", \
                       t, n, crtc, m[3], m[4], m[1], m[2], fb
            iscursor=0
        }
    ' "$STATE"
    sleep 0.02
done
