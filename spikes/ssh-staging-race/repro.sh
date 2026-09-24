#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# repro.sh — hammer the harness's boot -> SSH-banner -> first ssh_exec staging step.
#
# venus_clear_rect failed once in a 3-wide nextest suite (2026-09-23) at its FIRST
# ssh_exec, right after wait_for_ssh_banner had already returned a real banner. ssh
# exited 255 with an EMPTY stderr, which at LogLevel=ERROR means the TCP connect was
# ACCEPTED and the SSH exchange then died -- "Connection refused" is the one
# connection-level failure ERROR still prints (measured; see RESULTS.md). So the forward
# was listening and something hung up mid-handshake.
#
# The suite reaches that step once per networked test, ~50 times in 38 minutes. This
# reaches it every ~10 seconds instead, at the same width, so a race that needs
# co-resident VMs has somewhere to show itself.
#
# Usage:  spikes/ssh-staging-race/repro.sh [outdir] [iters] [wide]
# Env:    TEST_BIN=... to point at a different test binary.
#
# Each concurrent copy gets its OWN TMPDIR: the harness scratch dir, the supervisor's
# unix sockets and the gvproxy packet log all land under it, so a failing run's evidence
# survives (passing runs are deleted) and nothing collides. It stays SHORT because those
# are unix socket paths -- macOS caps sun_path at 104 bytes.
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="${1:-$repo/spikes/ssh-staging-race/runs/$(date +%Y%m%d-%H%M%S)}"
iters="${2:-20}"
wide="${3:-3}"
bin="${TEST_BIN:-}"

if [ -z "$bin" ]; then
    bin="$(ls -t "$repo"/target/debug/deps/venus_clear_rect-* 2>/dev/null | grep -v '\.d$' | head -1)"
fi
[ -x "$bin" ] || { echo "no test binary (build with: cargo test -p limina-test --test venus_clear_rect --no-run)" >&2; exit 2; }

mkdir -p "$out"
echo "repro: $iters iterations x $wide concurrent, binary $(basename "$bin")"
echo "repro: logs in $out"

fails=0
for i in $(seq 1 "$iters"); do
    pids=()
    for k in $(seq 1 "$wide"); do
        run="$(printf '%02d-%d' "$i" "$k")"
        tmp="/tmp/limina-sshrace/$run"
        rm -rf "$tmp"; mkdir -p "$tmp"
        (
            TMPDIR="$tmp" LIMINA_HVF_TESTS=1 LIMINA_TEST_KEEP_SCRATCH=1 \
                "$bin" --nocapture --test-threads 1 > "$out/$run.log" 2>&1
            echo $? > "$out/$run.status"
        ) &
        pids+=($!)
    done
    wait "${pids[@]}"

    bad=""
    for k in $(seq 1 "$wide"); do
        run="$(printf '%02d-%d' "$i" "$k")"
        st="$(cat "$out/$run.status" 2>/dev/null || echo '?')"
        if [ "$st" = "0" ]; then
            rm -rf "/tmp/limina-sshrace/$run" "$out/$run.log" "$out/$run.status"
        else
            bad="$bad $run(exit $st)"
            # Keep the scratch AND note where it is; the gvproxy packet log in it is the
            # host-side oracle for whether the forward ever dialed the guest.
            echo "scratch kept: /tmp/limina-sshrace/$run" >> "$out/$run.log"
        fi
    done

    if [ -n "$bad" ]; then
        fails=$((fails + 1))
        echo "iteration $i: FAILED —$bad"
    else
        echo "iteration $i: all $wide passed"
    fi
done

echo
echo "repro: $fails/$iters iterations had a failure; surviving logs in $out"
[ "$fails" -eq 0 ]
