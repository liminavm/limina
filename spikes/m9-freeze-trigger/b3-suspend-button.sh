#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# B3 — stock, no-agent suspend trigger via the GPIO KEY_SLEEP button.
#
# Proves: pulsing libkrun's new GPIO suspend button (host writes the suspend eventfd, which
# the worker does on SIGUSR2) makes a STOCK Fedora guest — no limina-agent, no custom kernel —
# enter suspend-to-idle, because systemd-logind maps KEY_SLEEP to HandleSuspendKey (default:
# suspend). This is the stock-tier freeze trigger for the M9 host-side snapshot.
#
# Method: boot a stock F44 clone headless (+net, SSH). Pre-arm an RTC wake alarm WITHOUT
# suspending (`rtcwake -m no`), so the guest wakes on its own after the button-driven suspend
# and we can read its dmesg back. Then SIGUSR2 the worker (== press the suspend button) and
# verify the guest logged `PM: suspend entry (s2idle)` and came back.
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/b3-suspend.log"
DISK="$JOB/tmp/b3-suspend.raw"
BOOTLOG="$JOB/tmp/b3-suspend-boot.log"
PORT=2223
WAKE_SECS=30
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"
say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }

cd "$REPO"
say "B3 start. cloning stock F44..."
cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }

say "booting headless on port $PORT (caffeinate keeps host awake)..."
caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 4096 --net --ssh-port "$PORT" > "$BOOTLOG" 2>&1 &
LIMINA_PID=$!
say "limina pid=$LIMINA_PID"

up=0
for i in $(seq 1 60); do
  if $SSH true 2>/dev/null; then up=1; say "SSH up after ~$((i*4))s"; break; fi
  sleep 4
done
[ "$up" = 1 ] || { say "guest never came up; aborting"; kill -9 $LIMINA_PID 2>/dev/null; exit 1; }

# Identify THIS boot's worker (never B4's) by the disk path in argv.
WPID=$(pgrep -f "limina-vmm.*b3-suspend.raw" | head -1)
say "worker pid=$WPID"
[ -n "$WPID" ] || { say "could not find worker pid; aborting"; kill -9 $LIMINA_PID 2>/dev/null; exit 1; }

say "guest rtc/logind sanity:"
$SSH 'ls -l /sys/class/rtc; cat /sys/class/rtc/rtc1/name 2>/dev/null; cat /sys/power/mem_sleep; systemctl show systemd-logind -p HandleSuspendKey 2>/dev/null' 2>/dev/null | tee -a "$LOG"

# Confirm the guest actually enumerated the KEY_SLEEP button (gpio-keys input dev).
say "gpio-keys input device (should advertise KEY_SLEEP / 'GPIO Key Suspend'):"
$SSH 'for d in /sys/class/input/event*/device; do n=$(cat $d/name 2>/dev/null); echo "$d -> $n"; done | grep -i gpio; sudo evtest --query /dev/input/event0 EV_KEY KEY_SLEEP 2>/dev/null; echo "(evtest query exit=$?)"' 2>/dev/null | tee -a "$LOG"

say "pre-arming RTC wake (rtcwake -m no -s $WAKE_SECS -d rtc1) WITHOUT suspending..."
$SSH "sudo rtcwake -m no -s $WAKE_SECS -d rtc1" 2>&1 | tee -a "$LOG"

say "clearing dmesg marker, then pressing the SUSPEND BUTTON (SIGUSR2 -> worker $WPID)..."
$SSH 'sudo dmesg -C' 2>/dev/null
kill -USR2 "$WPID" || { say "kill -USR2 failed"; }

say "waiting for the guest to suspend then self-wake (armed alarm, ~${WAKE_SECS}s)..."
# It should briefly become unreachable (frozen), then return.
went_down=0; came_back=0
for i in $(seq 1 20); do
  sleep 3
  if $SSH true 2>/dev/null; then
    [ "$went_down" = 1 ] && { came_back=1; say "SSH back after down (poll $i)"; break; }
  else
    went_down=1; say "SSH unreachable (poll $i) — guest frozen?"
  fi
done

say "post-run guest PM dmesg:"
PMLOG=$($SSH 'sudo dmesg | grep -iE "PM: suspend entry|suspend exit|resume|rtc" | tail -8' 2>/dev/null)
echo "$PMLOG" | tee -a "$LOG"

echo "$PMLOG" | grep -qiE 'suspend entry \(s2idle\)' && S2=1 || S2=0
say "----"
if [ "$S2" = 1 ]; then
  say "VERDICT: PASS — stock no-agent guest entered s2idle via the GPIO suspend button (went_down=$went_down came_back=$came_back)"
else
  say "VERDICT: FAIL/INCONCLUSIVE — no 's2idle' suspend-entry seen (went_down=$went_down came_back=$came_back); inspect $LOG / $BOOTLOG"
fi

say "cleanup: killing limina pid=$LIMINA_PID"
kill -9 $LIMINA_PID 2>/dev/null
pkill -9 -f "b3-suspend.raw" 2>/dev/null
rm -f "$DISK"
say "B3 done."
