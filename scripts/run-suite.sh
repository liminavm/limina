#!/bin/bash
# run-suite.sh — run the full HVF boot suite and end with its REAL verdict.
#
# THE one way to run (or wait on) the ~30-minute suite from a session:
#
#   scripts/run-suite.sh [logfile]            # run it; exit code = the suite's own
#   scripts/run-suite.sh --wait <logfile> [pid]   # attach to a suite already running
#
# Why this exists: `nohup cargo xtask test > log &` returns exit 0 seconds after
# launch — that status is the backgrounding shell's, not the suite's, and it reads
# exactly like a green run (a false green nearly shipped that way, 2026-08-14).
# This script keeps the suite in ITS OWN foreground, so backgrounding the SCRIPT
# (run_in_background, a terminal tab) yields a completion that IS the suite's
# completion, carrying the suite's real exit code, with the verdict lines printed
# at the end. It also refuses to start while another run is live: a concurrent
# `cargo build` — or a `git commit`, whose pre-commit hook runs clippy — relinks
# the binaries under the running tests.
set -u

repo="$(cd "$(dirname "$0")/.." && pwd)"

verdict() {
    # The Summary/FAILED lines are the ONLY trustworthy readout. Missing Summary
    # means the run died before nextest reported — that is a failure, not a pass.
    local log="$1"
    echo "== verdict ($log) =="
    if ! grep -E "^ *Summary|test result:|FAILED|error\[|error:" "$log"; then
        echo "no Summary line in the log — the run died before reporting; NOT green"
        return 1
    fi
    grep -qE "^ *Summary" "$log" || { echo "no Summary line — NOT green"; return 1; }
    ! grep -q "FAILED" "$log"
}

live_suite_pids() {
    # The xtask wrapper, the script it shells to, and nextest itself; never this script.
    pgrep -f "cargo xtask test|xtask test$|scripts/test-boot.sh|cargo-nextest" 2>/dev/null
}

if [ "${1:-}" = "--wait" ]; then
    log="${2:?usage: run-suite.sh --wait <logfile> [pid]}"
    pid="${3:-}"
    if [ -z "$pid" ]; then
        pid="$(live_suite_pids | head -1)"
        [ -n "$pid" ] || { echo "no running suite found and no pid given" >&2; exit 2; }
        echo "attaching to suite pid $pid"
    fi
    while kill -0 "$pid" 2>/dev/null; do sleep 20; done
    verdict "$log"
    exit $?
fi

log="${1:-/tmp/limina-suite-$(date +%Y%m%d-%H%M%S).log}"

if pids="$(live_suite_pids)" && [ -n "$pids" ]; then
    echo "a suite is already running (pid(s): $pids) — attach with:" >&2
    echo "  scripts/run-suite.sh --wait <its-logfile> ${pids%%$'\n'*}" >&2
    exit 2
fi

echo "suite log: $log"
cd "$repo"
cargo xtask test >"$log" 2>&1
status=$?
verdict "$log"
v=$?
[ "$status" -eq 0 ] && [ "$v" -eq 0 ]
