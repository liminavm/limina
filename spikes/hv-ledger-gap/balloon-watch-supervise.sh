#!/bin/sh
# Keep the full-fidelity balloon watch attached across ssh drops and VM restarts.
#
# The bare `ssh ... tail -F | balloon-watch.py` pipeline dies on any network blip, on the
# dogfood Mac sleeping, and (observed 2026-08-14) on the session harness reaping a
# long-running background task. Each death is silent: the log simply stops growing, which
# looks exactly like a calm guest. That is the failure mode this wrapper exists to remove --
# a monitor that dies quietly is worse than no monitor, because its silence reads as good news.
#
# Reconnects forever, stamping every (re)connection into the log so a gap is always visible
# as a gap rather than as calm.
#
#   LIMINA_WATCH_HOST=<dogfood mac>  LIMINA_WATCH_VM=Dev  ./balloon-watch-supervise.sh <logfile>
set -u

HOST="${LIMINA_WATCH_HOST:?set LIMINA_WATCH_HOST to the dogfood Mac}"
VM="${LIMINA_WATCH_VM:-Dev}"
LOG="${1:?usage: balloon-watch-supervise.sh <logfile>}"
HERE=$(dirname "$0")
TRACE="\$HOME/Library/Application Support/Limina/VMs/${VM}.liminavm/logs/balloon-trace.jsonl"

while :; do
    echo "=== watch connecting $(date '+%H:%M:%S') ===" >> "$LOG"
    ssh -o ServerAliveInterval=30 -o ServerAliveCountMax=6 \
        -o ConnectTimeout=15 -o BatchMode=yes \
        "$HOST" "tail -F \"$TRACE\"" 2>/dev/null \
        | python3 -u "$HERE/balloon-watch.py" >> "$LOG" 2>&1
    echo "=== watch DROPPED $(date '+%H:%M:%S'), reconnecting in 15s ===" >> "$LOG"
    sleep 15
done
