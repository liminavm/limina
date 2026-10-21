#!/bin/sh
# Build the three probes and run the sweep, each run under a hard cap. See RESULTS.md.
# Usage: ./run.sh [probe|rx|tx|all]   (default all); SECS per run (default 5).
# TX_LEVEL=1 registers tx's write filter level-triggered.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${NETD_SPIKE_OUT:-$here/build}
mkdir -p "$out"
# NETD_SPIKE_NOBUILD=1 runs binaries already in $out (a host without a usable clang).
if [ -z "${NETD_SPIKE_NOBUILD:-}" ]; then
    for p in probe rx tx; do
        xcrun clang -O2 -Wall -Wextra -o "$out/$p" "$here/$p.c"
    done
fi
secs=${SECS:-5}
cap() { gtimeout --kill-after=5 $((secs + 30)) "$@"; }
what=${1:-all}

if [ "$what" = probe ] || [ "$what" = all ]; then
    echo "== probe"
    cap "$out/probe"
fi
if [ "$what" = rx ] || [ "$what" = all ]; then
    echo "== rx: strategy x window x socket buffer (default, 8 MiB), ack lag 100 us"
    for buf in 0 8388608; do
        for w in 131072 524288 2097152 8388608; do
            for s in forget retain peek; do
                cap "$out/rx" $s $w "$secs" $buf 100
            done
        done
    done
    echo "== rx: one frame lost after 1 GiB, window 2 MiB, 8 MiB socket buffers"
    for s in forget retain peek; do
        cap "$out/rx" $s 2097152 "$secs" 8388608 100 1
    done
fi
if [ "$what" = tx ] || [ "$what" = all ]; then
    echo "== tx"
    for buf in 0 8388608; do
        for m in none kevent sockopt; do
            cap "$out/tx" $m "$secs" $buf
        done
    done
fi
