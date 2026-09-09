#!/bin/bash
# run-suite.sh — run the full HVF boot suite and end with its REAL verdict.
#
# THE one way to run (or wait on) the ~30-minute suite from a session:
#
#   scripts/run-suite.sh [logfile] [test args...]           # run it; exit code = the suite's own
#   scripts/run-suite.sh --detach [logfile] [test args...]  # start it and return immediately
#   scripts/run-suite.sh --wait <logfile> [pid]             # attach to a suite already running
#   scripts/run-suite.sh --attached [logfile] [test args...]  # old behaviour; see below
#
# Why this exists: `nohup cargo xtask test > log &` returns exit 0 seconds after
# launch — that status is the backgrounding shell's, not the suite's, and it reads
# exactly like a green run (a false green nearly shipped that way, 2026-08-14).
# Backgrounding THIS script instead (run_in_background, a terminal tab) yields a
# completion that IS the suite's completion, carrying the suite's real exit code,
# with the verdict lines printed at the end. It also refuses to start while another
# run is live: a concurrent `cargo build` — or a `git commit`, whose pre-commit hook
# runs clippy — relinks the binaries under the running tests.
#
# Trailing args are forwarded verbatim to the test run (xtask -> test-boot.sh -> nextest),
# so a partial re-run is `run-suite.sh <log> -E 'binary(venus) + binary(virgl)'`.
#
# THE SUITE IS DETACHED BY DEFAULT, because an agent harness reaps the background
# commands it owns. One killed a 29-minute run at 116/138 with "stopped because the
# system is running low on memory" while the host had ~10 GB free (2026-09-09): the log
# showed `Cancelling due to signal` then `Killing due to second signal`, an escalating
# SIGTERM to the process group rather than an OS jetsam SIGKILL, which no amount of free
# RAM would have prevented. A 30-minute suite cannot be run in a tool foreground either
# (the Bash tool caps at 10 minutes), so the only safe place for it is its OWN session,
# where a group-directed signal cannot reach it.
#
# So the default now starts the suite detached and then waits on it. The caller sees the
# same thing as before — it blocks, prints the verdict, exits with the suite's status —
# but the process the harness can reap is only the WAITER. Kill that and the suite keeps
# running; the launch banner carries the pid and the --wait line needed to re-attach,
# which is why the banner is printed up front rather than at the end.
#
# Detaching does NOT relax the false-green rule. --detach reports no verdict precisely
# because it does not have one yet; it prints the --wait line to run next. The suite's
# real exit code lands in <logfile>.status so --wait combines it with the log's own
# Summary lines, and a run that dies before reporting still reads as NOT green.
#
# --attached runs the suite in this process, as this script did before 2026-09-09. It is
# for a terminal you are sitting in front of, or a host with no usable python3 (the detach
# borrows setsid(2) from it, since macOS ships no setsid(1)). Under an agent harness it is
# the shape that got a run killed, so prefer the default.
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

# Start the suite in its own session and print the banner. Sets SUITE_PID.
start_detached() {
    local log="$1"; shift
    refuse_if_live
    rm -f "$log.status" "$log.pgid"
    # macOS ships no setsid(1), so borrow setsid(2) from python3: the child leaves this
    # shell's session and process group before exec'ing the suite, which is what makes a
    # group-directed signal aimed at our caller miss it. execvp keeps the pid, so $! below
    # is the suite's own shell and --wait can poll it.
    #
    # setsid ALONE IS NOT ENOUGH, measured 2026-09-09: a harness that reaps a background
    # command kills the child tree as well as the process group, and on the waiting path the
    # suite stays a CHILD of the waiter (it only reparents to init when the launcher exits,
    # which --detach does and the default does not). A killed waiter took the suite with it.
    # So fork twice: the intermediate exits immediately, orphaning the suite to init, which
    # takes it out of the tree a walk can find. setsid handles the group, the double fork
    # handles the tree, and neither substitutes for the other.
    #
    # The detach is VERIFIED, not assumed: the grandchild records its own pid and pgid, and
    # we refuse to claim success unless they match (a new group leader). A swallowed setsid
    # failure would leave the suite in our group, still killable, and still looking fine —
    # the whole reason this mode exists would be gone with nothing on screen to say so.
    nohup python3 -c '
import os, sys
mark = sys.argv[1]
if os.fork() > 0:
    os._exit(0)
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
    # $! is the intermediate, which exits at once; the suite's real pid comes from the mark.
    wait $! 2>/dev/null || true
    # The grandchild writes the mark before exec'ing, so this settles in milliseconds; give
    # it a couple of seconds rather than racing it.
    local _i
    for _i in 1 2 3 4 5 6 7 8 9 10; do
        [ -s "$log.pgid" ] && break
        sleep 0.2
    done

    local detached=no cpid cpgid
    if [ -s "$log.pgid" ]; then
        read -r cpid cpgid < "$log.pgid"
        [ "$cpid" = "$cpgid" ] && detached=yes
    fi
    if [ -z "${cpid:-}" ]; then
        echo "the detached child never reported a pid ($log.pgid absent or empty);" >&2
        echo "cannot supervise a run we cannot name — aborting" >&2
        exit 3
    fi
    SUITE_PID="$cpid"

    echo "================================================================================"
    if [ "$detached" = yes ]; then
        echo "  SUITE DETACHED — pid $SUITE_PID (session leader, pgid $cpgid)"
    else
        echo "  SUITE STARTED — pid $SUITE_PID   *** NOT DETACHED ***"
    fi
    echo "  log: $log"
    if [ "$detached" = yes ]; then
        echo "  This waiter is disposable. If it is killed, THE SUITE KEEPS RUNNING."
    else
        echo "  setsid did not take, so the suite is in THIS SHELL'S PROCESS GROUP and a"
        echo "  harness reaping this command WILL kill it. Mark: $(cat "$log.pgid" 2>/dev/null || echo absent)"
    fi
    echo "  Re-attach at any time with:"
    echo "      scripts/run-suite.sh --wait $log $SUITE_PID"
    echo "================================================================================"
}

# Block until the suite at $2 exits, then print and return its real verdict.
wait_for() {
    local log="$1" pid="$2"
    while kill -0 "$pid" 2>/dev/null; do sleep 20; done
    verdict "$log"
    local v=$?
    # A detached run records its own exit code; --attached leaves no such file, and there
    # the log's Summary lines are the whole story.
    local status=0
    [ -s "$log.status" ] && status="$(cat "$log.status")"
    [ "$status" -eq 0 ] && [ "$v" -eq 0 ]
}

default_log() { echo "/tmp/limina-suite-$(date +%Y%m%d-%H%M%S).log"; }

case "${1:-}" in
--wait)
    log="${2:?usage: run-suite.sh --wait <logfile> [pid]}"
    pid="${3:-}"
    if [ -z "$pid" ]; then
        pid="$(live_suite_pids | head -1)"
        [ -n "$pid" ] || { echo "no running suite found and no pid given" >&2; exit 2; }
        echo "attaching to suite pid $pid"
    fi
    wait_for "$log" "$pid"
    exit $?
    ;;
--detach)
    shift
    log="${1:-$(default_log)}"
    [ $# -gt 0 ] && shift || true
    start_detached "$log" "$@"
    exit 0
    ;;
--attached)
    shift
    log="${1:-$(default_log)}"
    [ $# -gt 0 ] && shift || true
    refuse_if_live
    echo "suite log: $log (ATTACHED — a harness reaping this command kills the suite)"
    cd "$repo"
    cargo xtask test "$@" >"$log" 2>&1
    status=$?
    verdict "$log"
    v=$?
    [ "$status" -eq 0 ] && [ "$v" -eq 0 ]
    exit $?
    ;;
esac

log="${1:-$(default_log)}"
[ $# -gt 0 ] && shift || true

start_detached "$log" "$@"
wait_for "$log" "$SUITE_PID"
