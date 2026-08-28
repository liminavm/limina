#!/bin/bash
# How long after the guest's devices go quiet (dpm_suspend_*, what Vmm::is_quiesced
# observes) does the kernel actually suspend timekeeping (timekeeping_freeze)?
# That interval is the window in which stopping the vCPUs misclassifies time.
#
# usage: pm-stage-timing.sh <worker-pid> <ssh-port>
set -e
PID=$1; PORT=$2
SSHOPT="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
G="ssh -p $PORT $SSHOPT claude@127.0.0.1"

ps -o command= -p "$PID" | grep -q "s2idle-repro.raw" || { echo "REFUSING: pid $PID is not our worker"; exit 1; }

echo "== arming in-guest suspend"
$G 'sudo systemd-run --on-active=3 systemctl suspend -i >/dev/null 2>&1; echo armed'
sleep 25
echo "== waking via the SIGWINCH seam"
kill -WINCH "$PID"
sleep 20
echo "== trace"
$G 'sudo cat /sys/kernel/tracing/trace' || echo "ssh failed"
