#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.2 WORKER-BRACKET round-trip: validate the worker's SIGTSTP suspend bracket in isolation (before
# the managed-VM supervisor plumbing). ONE SIGTSTP to the worker must: pulse the guest suspend button
# -> poll the quiesce oracle -> snapshot -> exit 126. Then restore into a fresh worker and the guest
# RESUMES with live devices. Contrast with restore-roundtrip.sh which drove the raw SIGUSR2+SIGUSR1
# seams by hand; here the single bracket signal does the whole quiesce-and-snapshot itself.
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/bracket-rt.log"; DISK="$JOB/tmp/bracket-rt.raw"; SNAP="$JOB/tmp/bracket-rt-snap.bin"
B1="$JOB/tmp/bracket-rt-boot1.log"; B2="$JOB/tmp/bracket-rt-boot2.log"
PORT=2232
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*bracket-rt.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cd "$REPO"; rm -f "$SNAP"
say "cloning stock F44..."; cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }

### PHASE 1 — bracket suspend via ONE SIGTSTP to the worker
say "PHASE1: boot + SIGTSTP bracket (button->quiesce-poll->snapshot->126)"
RUST_LOG=limina_vmm=info,krun_vmm=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 1024 --net --ssh-port "$PORT" --snapshot-file "$SNAP" > "$B1" 2>&1 &
LP1=$!; say "limina#1 pid=$LP1"
up=0; for i in $(seq 1 60); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP1 2>/dev/null; pkill -9 -f bracket-rt.raw; rm -f "$DISK"; exit 1; }
BOOTID_PRE=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d "\r")
CURL_PRE=$($SSH 'curl -s -o /dev/null -w "%{http_code}" --max-time 8 http://example.com' 2>/dev/null | tr -d "\r")
say "pre: boot_id=$BOOTID_PRE curl=$CURL_PRE"
W1=$(wpid)
say "SIGTSTP bracket -> worker $W1 (single signal; worker does button+quiesce+snapshot)"; kill -TSTP "$W1"
g=0; for i in $(seq 1 40); do kill -0 "$LP1" 2>/dev/null || { g=1; break; }; sleep 2; done
wait "$LP1" 2>/dev/null; RC1=$?
say "phase1 exit rc=$RC1 (want 126); snap: $(ls -la "$SNAP" 2>/dev/null | awk '{print $5}')"
say "=== bracket worker log ==="; grep -iE 'bracket:|quiesced|holdout' "$B1" | tail -12 | tee -a "$LOG"
[ "$RC1" = 126 ] && [ -s "$SNAP" ] || { say "BRACKET SAVE FAILED"; pkill -9 -f bracket-rt.raw; rm -f "$DISK" "$SNAP"; exit 1; }

### PHASE 2 — restore
say "PHASE2: restore into a fresh worker (--restore)"
RUST_LOG=limina_vmm=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 1024 --net --ssh-port "$PORT" --restore "$SNAP" > "$B2" 2>&1 &
LP2=$!; say "limina#2 pid=$LP2 (restoring)"
back=0; for i in $(seq 1 40); do sleep 3; ssh_up && { back=1; say "SSH BACK after restore (~$((i*3))s)"; break; }; done
if [ "$back" = 1 ]; then
  BOOTID_POST=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d "\r")
  CURL_POST=$($SSH 'curl -s -o /dev/null -w "%{http_code}" --max-time 8 http://example.com' 2>/dev/null | tr -d "\r")
  say "post: boot_id=$BOOTID_POST curl=$CURL_POST"
  if [ "$BOOTID_PRE" = "$BOOTID_POST" ] && [ -n "$BOOTID_POST" ] && [ "$CURL_POST" = 200 ]; then
    say "VERDICT: WORKER-BRACKET ROUND-TRIP GREEN — one SIGTSTP suspended+snapshotted; restore resumed same boot + live net"
  else
    say "VERDICT: PARTIAL/FAIL — boot_id pre=$BOOTID_PRE post=$BOOTID_POST curl_post=$CURL_POST"
  fi
else
  say "VERDICT: FAIL — guest did not come back after restore"
  grep -iE 'restor|wake|error|panic' "$B2" | tail -15 | tee -a "$LOG"
fi
say "cleanup"; kill -9 $LP2 2>/dev/null; pkill -9 -f "bracket-rt.raw" 2>/dev/null; rm -f "$DISK" "$SNAP"; say "bracket spike done."
