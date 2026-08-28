#!/bin/bash
# Level 0: stop the worker's threads for a window while the guest believes it is
# running. If the guest's CNTVCT keeps advancing, the window lands in
# CLOCK_MONOTONIC and systemd's service watchdogs fire on the far side.
#
# usage: stop-window.sh <worker-pid> <seconds> <ssh-port>
set -e
PID=$1; SECS=$2; PORT=$3
SSHOPT="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
G="ssh -p $PORT $SSHOPT claude@127.0.0.1"

# Guard: only ever signal a worker whose disk is ours.
ps -o command= -p "$PID" | grep -q "s2idle-repro.raw" || { echo "REFUSING: pid $PID is not our worker"; exit 1; }

echo "== BEFORE (host $(date -u +%H:%M:%S.%N))"
$G 'tail -1 /var/log/clocklog.txt; echo "uptime=$(cut -d" " -f1 /proc/uptime)"'

echo "== SIGSTOP $PID for ${SECS}s"
kill -STOP "$PID"
HOST_T0=$(date +%s.%N)
sleep "$SECS"
kill -CONT "$PID"
HOST_T1=$(date +%s.%N)
echo "== SIGCONT; host window = $(echo "$HOST_T1 - $HOST_T0" | bc) s"

sleep 20
echo "== AFTER (host $(date -u +%H:%M:%S.%N))"
$G 'tail -1 /var/log/clocklog.txt; echo "uptime=$(cut -d" " -f1 /proc/uptime)"' || echo "ssh FAILED after the window"
