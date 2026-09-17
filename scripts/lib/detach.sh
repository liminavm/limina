#!/bin/bash
# detach.sh — run a long job in its OWN session, orphaned to init, and wait on it safely.
#
# Sourced by scripts/run-suite.sh (the HVF suite) and scripts/run-battery.sh (the perf
# battery). Both need the same thing: a job that outlives the agent harness which started
# it, and a verdict that cannot be faked by a backgrounding shell's exit code.
#
# THE TWO FAILURES THIS EXISTS TO PREVENT, both paid for in real runs:
#
# 1. FALSE GREEN. `nohup <job> > log &` returns exit 0 seconds after launch — that status
#    is the backgrounding shell's, not the job's, and it reads exactly like a clean run.
#    One nearly shipped that way, 2026-08-14. So the job's real exit code is written to
#    <log>.status by the job's own wrapper, and the verdict combines it with the log.
#
# 2. A REAPED RUN. An agent harness reaps the background commands it owns. One killed a
#    29-minute suite at 116/138 with "stopped because the system is running low on memory"
#    while the host had ~10 GB free (2026-09-09): an escalating SIGTERM to the process
#    group, not an OS jetsam SIGKILL. Two mechanisms are needed and NEITHER SUBSTITUTES
#    FOR THE OTHER:
#      - setsid(2) puts the job in its own session and process group, so a group-directed
#        signal aimed at the caller misses it. macOS ships no setsid(1), so it is borrowed
#        from python3.
#      - a double fork orphans the job to init, so a harness that walks the CHILD TREE
#        (rather than signalling the group) cannot find it either. Without this the job
#        stays a child of the waiter on the waiting path, and a killed waiter takes it.
#
# The detach is VERIFIED, not assumed: the grandchild records its own pid and pgid before
# exec'ing, and the caller refuses to claim success unless they match (a new group leader).
# A swallowed setsid failure would leave the job in the caller's group — still killable,
# still looking fine — with nothing on screen to say the whole point had been lost.

# detach_launch <cwd> <log> <cmd...>
#   Starts <cmd...> detached, with stdout+stderr to <log>. Writes <log>.status (the job's
#   real exit code, when it finishes) and <log>.pgid (its pid and pgid, at once).
#   Sets DETACH_PID (the job's own pid) and DETACH_OK (yes/no — whether it truly detached).
#   Returns non-zero only when the child never reported a pid at all.
detach_launch() {
    local cwd="$1" log="$2"; shift 2
    rm -f "$log.status" "$log.pgid"
    # execvp keeps the pid, so the pid in the mark file is the job's own and --wait can poll it.
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
"$@" >"$log" 2>&1
echo $? >"$log.status"
' _ "$cwd" "$log" "$@" </dev/null >/dev/null 2>&1 &
    # $! is the intermediate, which exits at once; the job's real pid comes from the mark.
    wait $! 2>/dev/null || true
    # The grandchild writes the mark before exec'ing, so this settles in milliseconds; give
    # it a couple of seconds rather than racing it.
    local _i
    for _i in 1 2 3 4 5 6 7 8 9 10; do
        [ -s "$log.pgid" ] && break
        sleep 0.2
    done

    local cpid cpgid
    DETACH_OK=no
    if [ -s "$log.pgid" ]; then
        read -r cpid cpgid < "$log.pgid"
        [ "$cpid" = "$cpgid" ] && DETACH_OK=yes
    fi
    if [ -z "${cpid:-}" ]; then
        echo "the detached child never reported a pid ($log.pgid absent or empty);" >&2
        echo "cannot supervise a run we cannot name — aborting" >&2
        return 3
    fi
    DETACH_PID="$cpid"
    return 0
}

# detach_banner <what> <log> <reattach-command>
#   Prints the launch banner. Printed UP FRONT, not at the end, because it carries the pid
#   needed to re-attach after the waiter is killed — which is exactly when it is needed.
detach_banner() {
    local what="$1" log="$2" reattach="$3"
    echo "================================================================================"
    if [ "$DETACH_OK" = yes ]; then
        echo "  ${what} DETACHED — pid $DETACH_PID (session leader)"
    else
        echo "  ${what} STARTED — pid $DETACH_PID   *** NOT DETACHED ***"
    fi
    echo "  log: $log"
    if [ "$DETACH_OK" = yes ]; then
        echo "  This waiter is disposable. If it is killed, THE RUN KEEPS GOING."
    else
        echo "  setsid did not take, so the job is in THIS SHELL'S PROCESS GROUP and a"
        echo "  harness reaping this command WILL kill it. Mark: $(cat "$log.pgid" 2>/dev/null || echo absent)"
    fi
    echo "  Re-attach at any time with:"
    echo "      $reattach"
    echo "================================================================================"
}

# detach_wait <log> <pid> <verdict-fn>
#   Blocks until the job exits, then prints and returns its REAL verdict: the log's own
#   readout (via <verdict-fn>) combined with the recorded exit status. A run that died
#   before reporting must read as NOT green, which is the verdict function's job.
detach_wait() {
    local log="$1" pid="$2" verdict_fn="$3"
    while kill -0 "$pid" 2>/dev/null; do sleep 20; done
    "$verdict_fn" "$log"
    local v=$?
    # A detached run records its own exit code; an attached one leaves no such file, and
    # there the log is the whole story.
    local status=0
    [ -s "$log.status" ] && status="$(cat "$log.status")"
    [ "$status" -eq 0 ] && [ "$v" -eq 0 ]
}
