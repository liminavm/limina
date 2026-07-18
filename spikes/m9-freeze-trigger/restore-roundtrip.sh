#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.2 RESTORE round-trip: suspend -> snapshot -> teardown -> restore into a fresh worker -> the guest
# RESUMES from s2idle with LIVE devices. Proof: (a) boot_id identical across restore (same boot =
# resumed, NOT rebooted); (b) SSH round-trips + outbound curl 200 (virtio-net + virtio-blk re-init'd).
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/restore-rt.log"; DISK="$JOB/tmp/restore-rt.raw"; SNAP="$JOB/tmp/restore-rt-snap.bin"
B1="$JOB/tmp/restore-rt-boot1.log"; B2="$JOB/tmp/restore-rt-boot2.log"
PORT=2230
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*restore-rt.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cd "$REPO"; rm -f "$SNAP"
say "cloning..."; cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }

### PHASE 1 — save
say "PHASE1: boot + suspend + snapshot"
RUST_LOG=limina_vmm=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 1024 --net --ssh-port "$PORT" --snapshot-file "$SNAP" > "$B1" 2>&1 &
LP1=$!; say "limina#1 pid=$LP1"
up=0; for i in $(seq 1 60); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP1 2>/dev/null; exit 1; }
BOOTID_PRE=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d "\r")
CURL_PRE=$($SSH 'curl -s -o /dev/null -w "%{http_code}" --max-time 8 http://example.com' 2>/dev/null | tr -d "\r")
say "pre: boot_id=$BOOTID_PRE curl=$CURL_PRE"
W1=$(wpid)
say "suspend button (SIGUSR2 -> $W1)"; kill -USR2 "$W1"
fr=0; for i in $(seq 1 20); do sleep 2; ssh_up || { fr=1; say "frozen (poll $i)"; break; }; done
[ "$fr" = 1 ] || { say "did not freeze"; kill -9 $LP1; exit 1; }
sleep 3
say "snapshot (SIGUSR1 -> $W1)"; kill -USR1 "$W1"
g=0; for i in $(seq 1 30); do kill -0 "$LP1" 2>/dev/null || { g=1; break; }; sleep 2; done
wait "$LP1" 2>/dev/null; RC1=$?
say "phase1 exit rc=$RC1 (want 126); snapshot: $(ls -la "$SNAP" 2>/dev/null | awk '{print $5}')"
[ "$RC1" = 126 ] && [ -s "$SNAP" ] || { say "SAVE FAILED"; pkill -9 -f restore-rt.raw; rm -f "$DISK" "$SNAP"; exit 1; }

### PHASE 2 — restore
say "PHASE2: restore into a fresh worker (--restore, same disk, same port)"
RUST_LOG=limina_vmm=info,krun_devices=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 1024 --net --ssh-port "$PORT" --restore "$SNAP" > "$B2" 2>&1 &
LP2=$!; say "limina#2 pid=$LP2 (restoring)"
# after restore + wake injection, the guest should resume and SSH should return
back=0; for i in $(seq 1 40); do sleep 3; ssh_up && { back=1; say "SSH BACK after restore (poll $i, ~$((i*3))s)"; break; }; done
if [ "$back" = 1 ]; then
  BOOTID_POST=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d "\r")
  CURL_POST=$($SSH 'curl -s -o /dev/null -w "%{http_code}" --max-time 8 http://example.com' 2>/dev/null | tr -d "\r")
  UPTIME=$($SSH 'cat /proc/uptime' 2>/dev/null | tr -d "\r")
  say "post: boot_id=$BOOTID_POST curl=$CURL_POST uptime=$UPTIME"
  say "guest PM dmesg:"; $SSH 'sudo dmesg | grep -iE "suspend entry|suspend exit|resume devices" | tail -4' 2>/dev/null | tee -a "$LOG"
  if [ "$BOOTID_PRE" = "$BOOTID_POST" ] && [ -n "$BOOTID_POST" ]; then CONT="SAME boot_id (RESUMED, not rebooted)"; else CONT="boot_id CHANGED (rebooted, NOT resumed!)"; fi
  say "continuity: $CONT"
  say "=== restore/wake worker log ==="; grep -iE 'restor|wake|snapshot|resumed from snapshot|s2idle' "$B2" | tail -12 | tee -a "$LOG"
  say "----"
  if [ "$BOOTID_PRE" = "$BOOTID_POST" ] && [ "$CURL_POST" = 200 ]; then
    say "VERDICT: RESTORE ROUND-TRIP GREEN — resumed same boot + live devices (curl 200)"
  else
    say "VERDICT: PARTIAL/FAIL — cont='$CONT' curl_post=$CURL_POST"
  fi
else
  say "VERDICT: FAIL — guest did not come back after restore+wake"
  say "=== restore worker log tail ==="; grep -iE 'restor|wake|snapshot|error|panic|resumed' "$B2" | tail -20 | tee -a "$LOG"
fi
say "cleanup"; kill -9 $LP2 2>/dev/null; pkill -9 -f "restore-rt.raw" 2>/dev/null; rm -f "$DISK" "$SNAP"; say "spike done."
