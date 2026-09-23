#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
#
# Test scripts/run-suite.sh's verdict readout against synthetic suite logs.
#
# The verdict is the only thing standing between a dead run and a "green suite" report, so
# it gets a test of its own. It has to read BOTH shapes scripts/test-boot.sh can produce:
# nextest's `Summary` line, and — when cargo-nextest is not installed — the serial
# `cargo test` fallback's libtest `test result:` lines, which never include a Summary.
#
# Run it directly: scripts/tests/run-suite-verdict.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_SUITE="$REPO/scripts/run-suite.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

pass=0
fail=0

# check <name> <expected-exit> <log-contents>
check() {
    local name="$1" want="$2" body="$3"
    local log="$TMP/$name.log"
    printf '%s\n' "$body" > "$log"
    "$RUN_SUITE" --verdict "$log" > "$TMP/$name.out" 2>&1
    local got=$?
    if [ "$got" -eq "$want" ]; then
        printf 'ok   %-34s (exit %d)\n' "$name" "$got"
        pass=$((pass + 1))
    else
        printf 'FAIL %-34s expected exit %d, got %d\n' "$name" "$want" "$got"
        sed 's/^/       | /' "$TMP/$name.out"
        fail=$((fail + 1))
    fi
}

# --- nextest shape (cargo-nextest installed) ---------------------------------------------
check nextest-green 0 '    Starting 144 tests across 88 binaries
        PASS [   1.234s] limina-test::boot boots_to_login
------------
     Summary [ 1234.567s] 144 tests run: 144 passed, 0 skipped'

check nextest-red 1 '    Starting 144 tests across 88 binaries
        FAIL [   1.234s] limina-test::boot boots_to_login
------------
     Summary [ 1234.567s] 144 tests run: 143 passed, 1 failed, 0 skipped'

# A test killed by a signal, captured verbatim from cargo-nextest 0.9.146 on 2026-09-23.
# The whole log contains the string "FAILED" ZERO times — nextest prints SIGABRT and counts
# it only in the Summary — so a verdict keyed on "FAILED" calls this run green. The worker
# builds with panic = "abort", so this is the shape a panicking HVF test actually takes.
check nextest-signal 1 '    Starting 2 tests across 1 binary
        PASS [   0.012s] (1/2) nxprobe passes
     SIGABRT [   0.012s] (2/2) nxprobe aborts
  stdout ───

    running 1 test

    (test aborted with signal 6: SIGABRT)

------------
     Summary [   0.013s] 2 tests run: 1 passed, 1 failed, 0 skipped
     SIGABRT [   0.012s] (2/2) nxprobe aborts
error: test run failed'

# --- libtest shape (the serial `cargo test` fallback; no Summary line, ever) --------------
# This is the case that made a 144-passed/0-failed run report NOT green on 2026-09-23.
check libtest-green 0 '==> cargo-nextest not found (brew install cargo-nextest) — serial cargo test fallback

running 18 tests
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.58s

running 1 test
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 71.31s'

check libtest-red 1 '==> cargo-nextest not found (brew install cargo-nextest) — serial cargo test fallback

running 2 tests
test boots_to_login ... FAILED

failures:
    boots_to_login

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.00s'

# --- a run that never reported -----------------------------------------------------------
check died-empty 1 ''

check died-midway 1 '   Compiling limina-vmm v0.1.0 (/repo/crates/limina-vmm)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 41.02s
     Running tests/boot.rs (target/debug/deps/boot-deadbeef)

running 1 test'

check compile-error 1 'error[E0425]: cannot find value `nope` in this scope
  --> crates/limina-test/tests/boot.rs:12:5
error: could not compile `limina-test` (test "boot") due to 1 previous error'

echo
echo "verdict tests: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
