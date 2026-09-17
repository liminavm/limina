#!/bin/bash
# run-battery.sh — run a multi-hour perf battery and end with its REAL verdict.
#
# THE one way to run (or wait on) a perf sweep from a session, the way scripts/run-suite.sh
# is for the HVF suite. Same detach mechanism (scripts/lib/detach.sh), different verdict.
#
#   scripts/run-battery.sh <driver> [logfile]          # run it; exit = the battery's own
#   scripts/run-battery.sh --detach <driver> [logfile] # start it and return immediately
#   scripts/run-battery.sh --wait <logfile> [pid]      # attach to a battery already running
#
# <driver> is a legs-style script (perf/<pass>/legs.sh) that runs its points in order and
# prints `######## <label> <time>` per point, `######## <label> ABORTED rc=N` for a point
# that bailed, and `######## legs done` at the end.
#
# WHY A BATTERY NEEDS THIS AS MUCH AS THE SUITE DOES: it runs for hours, it is started from
# an agent session that may be reaped, and — unlike the suite — a lost run is not merely a
# re-run. Its legs are alternated against a baseline precisely because the host's renderer
# speed drifts over tens of minutes; a sweep resumed hours later is not the same experiment.
#
# WHAT THE VERDICT DOES NOT DO: it never says the numbers are good. A green battery means
# every point RAN — not that any instrument moved, and not that the host was quiet. Only the
# ledger's gl-replay-llvmpipe control is entitled to say the host was quiet (healthy 717-734;
# a low reading means DISCARD the leg, not label it), and only a human reads the trend.
set -u

repo="$(cd "$(dirname "$0")/.." && pwd)"
. "$repo/scripts/lib/detach.sh"

# grep -c prints 0 AND exits 1 when nothing matches, so `$(grep -c … || echo 0)` captures
# both zeros as the string "0\n0" and every later [ -gt ] test dies with "integer expression
# expected" — which reads as a FAILED battery for a run that finished clean. Let grep's own
# output stand; the default only covers a missing file, where grep prints nothing at all.
count_matches() { # <pattern> <log>
    local n; n=$(grep -cE "$1" "$2" 2>/dev/null); echo "${n:-0}"
}

verdict() {
    # Read the log, not the exit code. A driver that dies mid-sweep still leaves points
    # behind it unrun, and `legs done` is the only line that says it reached the end.
    local log="$1"
    echo "== verdict ($log) =="
    local started done_ aborted
    started=$(count_matches '^######## [^ ]+ [0-9]{2}:[0-9]{2}:[0-9]{2}$' "$log")
    aborted=$(count_matches '^######## .* ABORTED' "$log")
    done_=$(count_matches '^######## legs done' "$log")
    echo "points started: $started, aborted: $aborted"
    grep -E '^######## .* ABORTED' "$log" 2>/dev/null

    # Conditions that invalidate rows rather than fail the run — surfaced because a battery
    # that "passed" while every row said INVALID is the worst possible green.
    local suspect
    suspect=$(count_matches 'WATCHDOG|WEDGED|POISONED|INVALID|UNPROVEN|REFUSING' "$log")
    if [ "$suspect" -gt 0 ]; then
        echo "-- $suspect line(s) that invalidate or weaken rows:"
        grep -nE 'WATCHDOG|WEDGED|POISONED|INVALID|UNPROVEN|REFUSING' "$log" 2>/dev/null
    fi

    if [ "$done_" -eq 0 ]; then
        echo "no 'legs done' line — the battery died before finishing; NOT complete"
        return 1
    fi
    # Say the green out loud. The failure this script exists to prevent looked exactly like a
    # pass, so a verdict that only ever speaks up to complain is one glance away from the bug.
    echo "reached 'legs done' — every point ran"
    [ "$aborted" -eq 0 ]
}

live_battery_pids() {
    # A legs driver, a point, or a VM one of them booted; never this script.
    pgrep -f "[l]egs\.sh|[p]oint\.sh" 2>/dev/null
}

refuse_if_live() {
    local pids
    if pids="$(live_battery_pids)" && [ -n "$pids" ]; then
        echo "a battery is already running (pid(s): $pids) — attach with:" >&2
        echo "  scripts/run-battery.sh --wait <its-logfile> ${pids%%$'\n'*}" >&2
        exit 2
    fi
    # Two batteries on one host measure each other. So does a suite.
    if pids="$(pgrep -f 'cargo xtask test|scripts/test-boot.sh|cargo-nextest' 2>/dev/null)" && [ -n "$pids" ]; then
        echo "the HVF suite is running (pid(s): $pids) — it would contend with every reading" >&2
        exit 2
    fi
}

default_log() { echo "/tmp/limina-battery-$(date +%Y%m%d-%H%M%S).log"; }

start() {
    local driver="$1" log="$2"
    [ -x "$repo/$driver" ] || [ -x "$driver" ] || { echo "driver not executable: $driver" >&2; exit 2; }
    refuse_if_live
    detach_launch "$repo" "$log" "$driver" || exit 3
    detach_banner "BATTERY" "$log" "scripts/run-battery.sh --wait $log $DETACH_PID"
}

case "${1:-}" in
--wait)
    log="${2:?usage: run-battery.sh --wait <logfile> [pid]}"
    pid="${3:-}"
    if [ -z "$pid" ]; then
        pid="$(live_battery_pids | head -1)"
        [ -n "$pid" ] || { echo "no running battery found and no pid given" >&2; exit 2; }
        echo "attaching to battery pid $pid"
    fi
    detach_wait "$log" "$pid" verdict
    exit $?
    ;;
--detach)
    shift
    driver="${1:?usage: run-battery.sh --detach <driver> [logfile]}"; shift
    log="${1:-$(default_log)}"
    start "$driver" "$log"
    exit 0
    ;;
esac

driver="${1:?usage: run-battery.sh <driver> [logfile]}"; shift
log="${1:-$(default_log)}"
start "$driver" "$log"
detach_wait "$log" "$DETACH_PID" verdict
