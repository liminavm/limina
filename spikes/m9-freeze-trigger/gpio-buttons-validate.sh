#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Validate the three-button GPIO design:
#   SIGHUP  -> KEY_RESTART -> guest reboots -> worker exit 125 -> supervisor RELAUNCHES (restart)
#   SIGTERM -> KEY_POWER   -> guest powers off -> worker exits (no relaunch) -> supervisor EXITS
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/pwr-probe.log"; DISK="$JOB/tmp/pwr-probe.raw"; BOOTLOG="$JOB/tmp/pwr-probe-boot.log"
PORT=2226
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*pwr-probe.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cd "$REPO"
say "cloning..."; cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }
say "booting headless on $PORT..."
caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 4096 --net --ssh-port "$PORT" > "$BOOTLOG" 2>&1 &
LP=$!; say "limina pid=$LP"
up=0; for i in $(seq 1 60); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP 2>/dev/null; exit 1; }

W1=$(wpid); say "worker#1 pid=$W1"
# ---- TEST 1: restart (SIGHUP) ----
say "TEST1 restart: SIGHUP -> worker $W1 (expect reboot + relaunch)"
kill -HUP "$W1"
# wait for the worker pid to CHANGE (relaunch), then SSH to come back
W2=""; for i in $(seq 1 45); do
  cur=$(wpid)
  if [ -n "$cur" ] && [ "$cur" != "$W1" ]; then W2=$cur; break; fi
  sleep 2
done
if [ -n "$W2" ]; then
  back=0; for i in $(seq 1 30); do ssh_up && { back=1; break; }; sleep 3; done
  say "TEST1 result: worker#2 pid=$W2 (relaunched, != $W1); ssh_back=$back"
  [ "$back" = 1 ] && say "TEST1 VERDICT: RESTART OK (guest rebooted + relaunched + SSH back)" || say "TEST1 VERDICT: relaunched but SSH did not return"
else
  say "TEST1 VERDICT: FAIL — worker did not relaunch (limina alive=$(kill -0 $LP 2>/dev/null && echo yes || echo no))"
fi

# ---- TEST 2: poweroff (SIGTERM) ----
WNOW=$(wpid); say "TEST2 poweroff: SIGTERM -> worker $WNOW (expect poweroff, NO relaunch, supervisor exits)"
kill -TERM "$WNOW"
gone=0; for i in $(seq 1 45); do
  if ! kill -0 "$LP" 2>/dev/null; then gone=1; break; fi
  sleep 2
done
wait "$LP" 2>/dev/null; RC=$?
still=$(wpid)
if [ "$gone" = 1 ] && [ -z "$still" ]; then
  say "TEST2 VERDICT: POWEROFF OK (supervisor exited rc=$RC, no relaunched worker)"
else
  say "TEST2 VERDICT: FAIL (gone=$gone, leftover worker='$still', rc=$RC)"
fi
say "=== poweroff/reboot console tells ==="
grep -inE 'Power(ing)? off|Power(ing)? down|reboot|Reached target (Power|Reboot)|systemd-shutdown|System Power' "$BOOTLOG" | tail -12 | tee -a "$LOG"
say "cleanup"; kill -9 $LP 2>/dev/null; pkill -9 -f "pwr-probe.raw" 2>/dev/null; rm -f "$DISK"; say "probe done."
