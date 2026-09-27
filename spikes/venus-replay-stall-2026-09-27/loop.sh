#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Run the venus_replay test binary over and over until one of its replays stalls, and hold that
# VM for live debugging (docs/hardening-backlog.md, "Seated venus replay stalls under suite load").
#
# Builds and signs once, then runs the binary itself -- never cargo -- in LANES parallel lanes of
# up to ITER iterations each. Each iteration is the three venus_replay tests in order, one process,
# which is what the suite runs. The first failure holds its VM (LIMINA_TEST_HOLD_ON_FAIL, see
# Guest::forensics) and stops every other lane at its next iteration, so the held guest has the
# host to itself. Its forensics directory is named in the lane's log and in this script's output.
#
# Usage:
#   spikes/venus-replay-stall-2026-09-27/loop.sh [log]           # detached; prints the pid
#   spikes/venus-replay-stall-2026-09-27/loop.sh --wait <log> <pid>
#   LANES=3 ITER=15 spikes/venus-replay-stall-2026-09-27/loop.sh
set -u
repo="$(cd "$(dirname "$0")/../.." && pwd)"
here="$repo/spikes/venus-replay-stall-2026-09-27"
. "$repo/scripts/lib/detach.sh"

verdict() {
    local log="$1"
    echo "== verdict ($log) =="
    grep -E '^(lane|STALL|loop)' "$log" 2>/dev/null
    grep -q '^loop done' "$log" || { echo "no 'loop done' line: the loop died"; return 1; }
    ! grep -q '^STALL' "$log"
}

case "${1:-}" in
--wait) detach_wait "$2" "$3" verdict; exit $? ;;
--run) ;;
*)
    log="${1:-$here/work.noindex/loop-$(date +%Y%m%d-%H%M%S).log}"
    mkdir -p "$(dirname "$log")"
    detach_launch "$repo" "$log" "$here/loop.sh" --run || exit 3
    detach_banner "REPLAY LOOP" "$log" "$here/loop.sh --wait $log $DETACH_PID"
    detach_wait "$log" "$DETACH_PID" verdict
    exit $?
    ;;
esac

LANES=${LANES:-3}
ITER=${ITER:-15}
work="$here/work.noindex/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$work"
stop="$work/STOP"

echo "loop: LANES=$LANES ITER=$ITER limina $(git -C "$repo" rev-parse --short HEAD) \
virglrs $(git -C "$repo/third_party/virglrs" rev-parse --short HEAD) work $work"
# `cargo xtask build` builds and signs the worker; test-boot.sh cannot build without running,
# because nextest refuses its --no-fail-fast beside --no-run.
{ (cd "$repo" && cargo xtask build && cargo test -p limina-test --test venus_replay --no-run); } \
    > "$work/build.log" 2>&1 ||
    { echo "loop: build failed, see $work/build.log"; exit 2; }
bin=$(ls -t "$repo"/target/debug/deps/venus_replay-* | grep -v '\.d$' | head -n 1)
echo "loop: binary $bin"

lane() {
    local l=$1 i t0 rc
    for i in $(seq 1 "$ITER"); do
        [ -e "$stop" ] && { echo "lane $l: stopped before iteration $i"; return; }
        t0=$(date +%s)
        local out="$work/lane$l-iter$i.log" pid held=
        # --nocapture, or the HOLDING line would surface only once the held test is released.
        LIMINA_HVF_TESTS=1 LIMINA_TEST_HOLD_ON_FAIL=1 \
            "$bin" --test-threads=1 --nocapture venus_ > "$out" 2>&1 &
        pid=$!
        while kill -0 "$pid" 2>/dev/null; do
            if [ -z "$held" ] && grep -q '^HOLDING' "$out"; then
                held=1
                touch "$stop"
                echo "STALL lane $l iter $i $(date +%H:%M:%S): $(grep -m1 -E '^forensics for' "$out")"
                grep -m1 '^HOLDING' "$out"
            fi
            sleep 5
        done
        wait "$pid"
        rc=$?
        echo "lane $l iter $i: rc=$rc $(( $(date +%s) - t0 ))s $(date +%H:%M:%S)"
        if [ "$rc" -ne 0 ]; then
            touch "$stop"
            [ -n "$held" ] || echo "STALL lane $l iter $i (no hold): $(grep -m1 -E 'forensics|panicked' "$out")"
            return
        fi
    done
}

for l in $(seq 1 "$LANES"); do
    lane "$l" &
    sleep 20 # stagger the boots so the lanes do not replay in lockstep
done
wait
echo "loop done $(date +%H:%M:%S)"
