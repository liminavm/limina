#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# B4 — does a >=1h in-place s2idle preserve the guest wall clock?
# Boots a stock F44 clone headless, records host+guest wall clock, arms rtcwake -m freeze for ~62min,
# waits for wake, records again, and reports the post-wake host<->guest skew.
# skew_after ~= 0  -> clock preserved across s2idle.
# skew_after ~= sleep_secs -> guest CLOCK_REALTIME froze during suspend (the drift the review predicts).
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/b4-clock.log"
DISK="$JOB/tmp/b4-clock.raw"
BOOTLOG="$JOB/tmp/b4-boot.log"
SLEEP_SECS=3720   # ~62 min
SSH="ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"
say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }

cd "$REPO"
say "B4 start. cloning stock F44..."
cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }

say "booting headless (keeping host awake via caffeinate)..."
caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 4096 --net > "$BOOTLOG" 2>&1 &
LIMINA_PID=$!
say "limina pid=$LIMINA_PID"

# wait for SSH
up=0
for i in $(seq 1 60); do
  if $SSH true 2>/dev/null; then up=1; say "SSH up after ~$((i*4))s"; break; fi
  sleep 4
done
[ "$up" = 1 ] || { say "guest never came up; aborting"; kill -9 $LIMINA_PID 2>/dev/null; exit 1; }

HOST_BEFORE=$(date +%s)
GUEST_BEFORE=$($SSH 'date +%s' 2>/dev/null)
say "before: host_epoch=$HOST_BEFORE guest_epoch=$GUEST_BEFORE skew=$((HOST_BEFORE-GUEST_BEFORE))s"
$SSH 'sudo timedatectl set-ntp false 2>/dev/null; echo ntp-disabled' >>"$LOG" 2>&1 || true

say "arming rtcwake -m freeze -s $SLEEP_SECS (in-place s2idle for ~62 min)..."
$SSH "sudo rm -f /tmp/b4woke; sudo systemd-run --no-block --unit=b4freeze bash -c 'rtcwake -m freeze -s $SLEEP_SECS -d rtc1 && date +%s > /tmp/b4woke'" >>"$LOG" 2>&1
say "armed; waiting for wake (poll up to 75 min)..."

woke=0
for i in $(seq 1 450); do   # 450*10s = 75 min
  sleep 10
  W=$($SSH 'cat /tmp/b4woke 2>/dev/null' 2>/dev/null)
  if echo "$W" | grep -qE '^[0-9]+$'; then woke=1; say "guest woke (poll $i, ~$((i*10))s past arm)"; break; fi
done

HOST_AFTER=$(date +%s)
if [ "$woke" = 1 ]; then
  GUEST_AFTER=$($SSH 'date +%s' 2>/dev/null)
  GUEST_DELTA=$((GUEST_AFTER-GUEST_BEFORE))
  HOST_DELTA=$((HOST_AFTER-HOST_BEFORE))
  SKEW_AFTER=$((HOST_AFTER-GUEST_AFTER))
  say "after: host_epoch=$HOST_AFTER guest_epoch=$GUEST_AFTER"
  say "RESULT: host_delta=${HOST_DELTA}s guest_delta=${GUEST_DELTA}s post_wake_skew=${SKEW_AFTER}s"
  if [ "$SKEW_AFTER" -lt 30 ]; then
    say "VERDICT: clock PRESERVED across in-place s2idle (skew < 30s)"
  else
    say "VERDICT: clock DRIFTED by ~${SKEW_AFTER}s across s2idle (guest froze wall clock)"
  fi
  say "guest dmesg PM:"; $SSH 'sudo dmesg | grep -iE "PM: suspend entry|suspend exit|resume devices" | tail -4' 2>/dev/null | tee -a "$LOG"
  $SSH 'timedatectl' 2>/dev/null | tee -a "$LOG"
else
  say "VERDICT: guest did NOT wake within 75min (s2idle wake failure at long interval?) host_delta=$((HOST_AFTER-HOST_BEFORE))s"
fi

say "cleanup: killing limina pid=$LIMINA_PID"
kill -9 $LIMINA_PID 2>/dev/null
pkill -9 -f "b4-clock.raw" 2>/dev/null
rm -f "$DISK"
say "B4 done."
