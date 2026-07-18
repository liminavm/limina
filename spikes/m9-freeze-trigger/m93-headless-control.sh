#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.3 floor spike — HEADLESS CONTROL. Isolate GPU-vs-config for the windowed restore stall: boot the
# SAME enhanced image (16k kernel), but HEADLESS (no virtio-gpu) at the SAME 4 vCPU / 4 GiB, run the
# M9.2 suspend bracket, and restore. If SSH comes back → the GPU is what breaks the windowed restore;
# if it ALSO stalls → the enhanced 16k kernel / config is the cause, not the GPU. Machine-observable
# (no window, no eyeball). Clone only.
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
SRC="${LIMINA_SRC_IMAGE:-Fedora-Workstation-44.enhanced.test.raw}"
FW="$REPO/target/krun-efi/KRUN_EFI.gop.fd"
LOG="$JOB/tmp/m93-hctl.log"; DISK="$JOB/tmp/m93-hctl.raw"; SNAP="$JOB/tmp/m93-hctl-snap.bin"
B1="$JOB/tmp/m93-hctl-b1.log"; B2="$JOB/tmp/m93-hctl-b2.log"
PORT=2247
CPUS="${LIMINA_CPUS:-4}"; RAM="${LIMINA_RAM_MIB:-4096}"
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o BatchMode=yes -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*m93-hctl.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cd "$REPO"; rm -f "$SNAP"
[ -f "$SRC" ] || { say "source image $SRC not found"; exit 1; }
say "cloning enhanced image $SRC (COW), HEADLESS ${CPUS}cpu/${RAM}mib…"; cp -c "$SRC" "$DISK" || { say "clone failed"; exit 1; }

### PHASE 1 — headless boot + bracket snapshot (NO --window → no virtio-gpu)
say "PHASE1: enhanced image HEADLESS boot + suspend bracket"
RUST_LOG=limina_vmm=info,krun_vmm=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --vmm-bin "$REPO/target/debug/limina-vmm" --firmware "$FW" \
  --disk "$DISK" --cpus "$CPUS" --ram-mib "$RAM" --net --ssh-port "$PORT" --snapshot-file "$SNAP" > "$B1" 2>&1 &
LP1=$!; say "limina#1 pid=$LP1 (headless, no GPU)"
up=0; for i in $(seq 1 75); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP1 2>/dev/null; pkill -9 -f m93-hctl.raw; rm -f "$DISK"; exit 1; }
PRE=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d '\r')
say "pre: boot_id=$PRE; firing bracket (SIGTSTP)"
W=$(wpid); kill -TSTP "$W"
g=0; for i in $(seq 1 40); do kill -0 "$LP1" 2>/dev/null || { g=1; break; }; sleep 2; done
wait "$LP1" 2>/dev/null; RC1=$?
SZ=$(ls -la "$SNAP" 2>/dev/null | awk '{print $5}')
say "phase1 exit rc=$RC1 (want 126); snapshot=${SZ:-MISSING}"
[ "$RC1" = 126 ] && [ -s "$SNAP" ] || { say "PHASE1 did not snapshot — cannot run the control"; grep -iE 'bracket:|quiesce|holdout' "$B1"|tail -8|tee -a "$LOG"; pkill -9 -f m93-hctl.raw; rm -f "$DISK" "$SNAP"; exit 1; }

### PHASE 2 — headless restore; does the SAME enhanced kernel come back WITHOUT a GPU?
say "PHASE2: headless restore (--restore, no GPU)"
RUST_LOG=limina_vmm=info,krun_vmm=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --vmm-bin "$REPO/target/debug/limina-vmm" --firmware "$FW" \
  --disk "$DISK" --cpus "$CPUS" --ram-mib "$RAM" --net --ssh-port "$PORT" --restore "$SNAP" > "$B2" 2>&1 &
LP2=$!; say "limina#2 pid=$LP2 (restoring, headless)"
back=0; for i in $(seq 1 40); do sleep 3; ssh_up && { back=1; say "SSH BACK after restore (~$((i*3))s)"; break; }; done
grep -iE 'resumed from snapshot at pc=' "$B2" 2>/dev/null | tail -6 | tee -a "$LOG" || true
if [ "$back" = 1 ]; then
  POST=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d '\r')
  say "post: boot_id=$POST ($([ "$PRE" = "$POST" ] && echo SAME=resumed || echo CHANGED=rebooted))"
  say "VERDICT: HEADLESS enhanced restore CAME BACK → the GPU/display is what breaks the WINDOWED restore."
else
  W2=$(wpid); CPU=$([ -n "$W2" ] && ps -o %cpu= -p "$W2" | tr -d ' ' || echo n/a)
  say "worker cpu=$CPU"
  say "VERDICT: HEADLESS enhanced restore ALSO STALLED (no SSH) → the ENHANCED 16k KERNEL / config"
  say "  is the cause, NOT the GPU. (Same failure as windowed → GPU is a red herring for the stall.)"
  grep -iE 'restor|wake|error|panic' "$B2" 2>/dev/null | grep -viE 'tcpproxy|no route' | tail -12 | tee -a "$LOG"
fi
say "cleanup"; kill -9 $LP2 2>/dev/null; pkill -9 -f "m93-hctl.raw" 2>/dev/null; rm -f "$DISK" "$SNAP"; say "control done."
