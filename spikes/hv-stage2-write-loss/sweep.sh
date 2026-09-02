#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Sweep the one-shot mid-fill protect (--race-once) over the fill's duration.
# Usage: ./sweep.sh <out-file> [reps=3] [step_ns=1000] [max_ns=120000] [extra probe args...]
# Prints, per (fill shape, granule): fills run, protects that landed inside the fill, fills that
# lost data — and every lossy line verbatim.
set -e
cd "$(dirname "$0")"
out=${1:?out file}; reps=${2:-3}; step=${3:-1000}; max=${4:-120000}
shift 4 2>/dev/null || shift $#
: > "$out"
for fill in gpr simd; do
    for gran in default 4k; do
        g=""; [ "$gran" = 4k ] && g="--granule 4k"
        d=0
        while [ "$d" -le "$max" ]; do
            r=0
            while [ "$r" -lt "$reps" ]; do
                ./probe payload.bin --mmu --fill "$fill" $g --race-once "$d" "$@" 2>&1 \
                    | grep '^\[once\]' | sed "s/^/fill=$fill granule=$gran /" >> "$out"
                r=$((r + 1))
            done
            d=$((d + step))
        done
        total=$(grep -c "fill=$fill granule=$gran " "$out" || true)
        inside=$(grep "fill=$fill granule=$gran " "$out" | grep -c 'protect=inside' || true)
        lossy=$(grep "fill=$fill granule=$gran " "$out" | grep -vc 'mismatching=0 ' || true)
        echo "fill=$fill granule=$gran fills=$total protect-inside=$inside lossy=$lossy"
    done
done
echo '--- lossy fills'
grep -v 'mismatching=0 ' "$out" || echo none
