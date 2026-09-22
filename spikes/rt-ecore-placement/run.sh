#!/bin/bash
# Build and run the RT placement matrix.
#
#   run.sh calib <outdir>             encoding + E-core id calibration, and an idle monitor baseline
#   run.sh matrix <outdir> <ecores> [reps]   the scenario matrix, <reps> times (default 3), arms interleaved
#
# Every RT arm is bounded by --secs, enforced inside the RT threads themselves, so a starved main
# thread cannot extend it. The probe refuses more RT threads than (P-cores - 1), (CPUs - 2) or 4.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
clang -O2 -Wall -Wextra -o "$here/placement" "$here/placement.c"
P="$here/placement"
mode="${1:?mode}"
out="${2:?outdir}"
mkdir -p "$out"

busy_check() {
    if pgrep -fl '[t]est-boot.sh|[r]un-suite.sh|[l]imina-vmm' >"$out/busy.txt"; then
        echo "WARNING: heavy processes present:" >&2
        /bin/cat "$out/busy.txt" >&2
    fi
}

arm() { # label secs args...
    local label="$1" secs="$2"
    shift 2
    local t0=$SECONDS
    if pgrep -fl '[l]imina-vmm' >"$out/vmm.txt"; then
        echo "# NOTE before $label: limina-vmm running: $(tr '\n' ' ' <"$out/vmm.txt")" >>"$out/matrix.txt"
    fi
    "$P" run --secs "$secs" --ecores "$ECORES" --label "$label" "$@" >>"$out/matrix.txt"
    local took=$((SECONDS - t0))
    if ((took > secs + 5)); then
        echo "ABORT: $label took ${took}s for a ${secs}s run" | tee -a "$out/matrix.txt" >&2
        exit 1
    fi
    sleep 3 # let the host settle between arms
}

case "$mode" in
calib)
    busy_check
    "$P" calib-all --secs 3 --label calib-all >"$out/calib.txt"
    "$P" calib-bg --threads 2 --secs 3 --label calib-bg2 >>"$out/calib.txt"
    "$P" calib-bg --threads 4 --secs 3 --label calib-bg4 >>"$out/calib.txt"
    "$P" run --secs 10 --label monitor-idle >>"$out/calib.txt"
    ;;
matrix)
    ECORES="${3:?ecores}"
    reps="${4:-3}"
    busy_check
    : >"$out/matrix.txt"
    for rep in $(seq 1 "$reps"); do
        date '+# rep '"$rep"' %F %T' >>"$out/matrix.txt"
        # (a) idle-ish vCPU: 16.667 ms timer wake, 300 us burst
        arm "a-burst-plain2 r$rep" 10 --plain 2 --work burst
        arm "a-burst-rt1 r$rep" 10 --rt 1 --work burst
        arm "a-burst-rt2 r$rep" 10 --rt 2 --work burst
        # (e) idle, then saturate: does a thread that woke on E move once it spins?
        arm "e-burst2spin-plain1 r$rep" 12 --plain 1 --work burst --spin-after 5
        arm "e-burst2spin-rt1 r$rep" 12 --rt 1 --work burst --spin-after 5
        # (b) saturated on an idle host
        arm "b-spin-plain2 r$rep" 12 --plain 2 --work spin
        arm "b-spin-rt1 r$rep" 12 --rt 1 --work spin
        arm "b-spin-rt2 r$rep" 12 --rt 2 --work spin
        # (c) saturated while ordinary threads saturate every P-core
        arm "c-spin-plain2-hog8 r$rep" 12 --plain 2 --hog 8 --work spin
        arm "c-spin-rt2-hog8 r$rep" 12 --rt 2 --hog 8 --work spin
        # (d) RT + QOS_CLASS_BACKGROUND, both orders
        arm "d-burst-rt2-bg r$rep" 10 --rt 2 --work burst --rt-qos bg
        arm "d-burst-rt2-bgafter r$rep" 10 --rt 2 --work burst --rt-qos bg-after
        arm "d-spin-rt2-bg r$rep" 12 --rt 2 --work spin --rt-qos bg
        arm "d-spin-rt2-bgafter r$rep" 12 --rt 2 --work spin --rt-qos bg-after
    done
    ;;
*)
    echo "unknown mode $mode" >&2
    exit 2
    ;;
esac
