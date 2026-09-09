#!/bin/bash
# run-suite.sh — run the full HVF boot suite and end with its REAL verdict.
#
# THE one way to run (or wait on) the ~30-minute suite from a session:
#
#   scripts/run-suite.sh [logfile] [test args...]           # run it; exit code = the suite's own
#   scripts/run-suite.sh --detach [logfile] [test args...]  # start it OUTSIDE this session
#   scripts/run-suite.sh --wait <logfile> [pid]             # attach to a suite already running
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
#
# Trailing args are forwarded verbatim to the test run (xtask -> test-boot.sh -> nextest),
# so a partial re-run is `run-suite.sh <log> -E 'binary(venus) + binary(virgl)'`.
#
# --detach exists because an agent harness reaps the background commands it owns. One
# killed a 29-minute run at 116/138 with "stopped because the system is running low on
# memory" while the host had ~10 GB free (2026-09-09): the log showed `Cancelling due to
# signal` then `Killing due to second signal`, an escalating SIGTERM to the process group
# rather than an OS jetsam SIGKILL, which no amount of free RAM would have prevented. A
# 30-minute suite cannot be run in a tool foreground either (the Bash tool caps at 10
# minutes), so the way out is to put the suite in its OWN session, where a group-directed
# signal cannot reach it, and let the cheap --wait be the only thing the harness babysits.
# Kill the waiter and the suite survives; re-attach with --wait.
#
# --detach does NOT relax the false-green rule — it reports no verdict precisely because it
# does not have one yet. It prints the --wait line to run next, and the suite's real exit
# code lands in <logfile>.status so --wait can combine it with the log's own Summary lines.
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

refuse_if_live() {
    local pids
    if pids="$(live_suite_pids)" && [ -n "$pids" ]; then
        echo "a suite is already running (pid(s): $pids) — attach with:" >&2
        echo "  scripts/run-suite.sh --wait <its-logfile> ${pids%%$'\n'*}" >&2
        exit 2
    fi
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
    v=$?
    # A --detach run records its own exit code; a foreground one leaves no such file, and
    # there the log's Summary lines are the whole story.
    status=0
    [ -s "$log.status" ] && status="$(cat "$log.status")"
    [ "$status" -eq 0 ] && [ "$v" -eq 0 ]
    exit $?
fi

if [ "${1:-}" = "--detach" ]; then
    shift
    log="${1:-/tmp/limina-suite-$(date +%Y%m%d-%H%M%S).log}"
    [ $# -gt 0 ] && shift || true
    refuse_if_live
    rm -f "$log.status" "$log.pgid"
    # macOS ships no setsid(1), so borrow setsid(2) from python3: the child leaves this
    # shell's session and process group before exec'ing the suite, which is what makes a
    # group-directed signal aimed at our caller miss it. execvp keeps the pid, so $! below
    # is the suite's own shell and --wait can poll it.
    #
    # The detach is VERIFIED, not assumed: the child records its own pid and pgid, and we
    # refuse to report success unless they match (a new group leader). A swallowed setsid
    # failure would leave the suite in our group, still killable, and still looking fine —
    # the whole reason this mode exists would be gone with nothing on screen to say so.
    nohup python3 -c '
import os, sys
mark = sys.argv[1]
try:
    os.setsid()
except OSError as e:
    open(mark, "w").write("setsid-failed %s\n" % e)
    raise
open(mark, "w").write("%d %d\n" % (os.getpid(), os.getpgid(0)))
os.execvp(sys.argv[2], sys.argv[2:])
' "$log.pgid" bash -c '
cd "$1" || exit 111
log="$2"; shift 2
cargo xtask test "$@" >"$log" 2>&1
echo $? >"$log.status"
' _ "$repo" "$log" "$@" </dev/null >/dev/null 2>&1 &
    pid=$!
    # The child writes the mark before exec'ing, so this settles in milliseconds; give it a
    # couple of seconds rather than racing it.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        [ -s "$log.pgid" ] && break
        sleep 0.2
    done
    echo "suite log: $log"
    if [ ! -s "$log.pgid" ]; then
        echo "WARNING: detached child never reported its process group ($log.pgid absent)." >&2
        echo "         It may still be running, but treat it as ATTACHED and killable." >&2
    else
        read -r cpid cpgid < "$log.pgid"
        if [ "$cpid" = "$cpgid" ]; then
            echo "detached suite pid: $pid (session leader, pgid $cpgid — signals to this shell's group cannot reach it)"
        else
            echo "WARNING: setsid did not take (pid $cpid, pgid $cpgid) — the suite is STILL IN THIS" >&2
            echo "         SHELL'S PROCESS GROUP and a harness reaping this command will kill it." >&2
            echo "         Mark: $(cat "$log.pgid")" >&2
        fi
    fi
    echo "wait for the verdict with:"
    echo "  scripts/run-suite.sh --wait $log $pid"
    exit 0
fi

log="${1:-/tmp/limina-suite-$(date +%Y%m%d-%H%M%S).log}"
[ $# -gt 0 ] && shift || true

refuse_if_live

echo "suite log: $log"
cd "$repo"
cargo xtask test "$@" >"$log" 2>&1
status=$?
verdict "$log"
v=$?
[ "$status" -eq 0 ] && [ "$v" -eq 0 ]
