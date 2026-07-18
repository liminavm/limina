#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.2 composition spike (SAVE half): can we snapshot a guest that is already parked in s2idle?
# Boot stock F44 EFI + --snapshot-file, pulse the SUSPEND button (SIGUSR2 -> KEY_SLEEP -> logind
# s2idle, no wake armed so it stays frozen), wait for the guest to freeze, then trigger the SNAPSHOT
# (SIGUSR1). save_snapshot self-quiesces via the per-vCPU Snapshot event + kick_all — the open
# question is whether that reaches vCPUs sitting in the s2idle idle loop. GREEN = worker writes the
# file and exits 126.
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/m92-comp-save.log"; DISK="$JOB/tmp/m92-comp.raw"; BOOTLOG="$JOB/tmp/m92-comp-boot.log"
SNAP="$JOB/tmp/m92-comp-snap.bin"
PORT=2228
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*m92-comp.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cd "$REPO"; rm -f "$SNAP"
say "cloning..."; cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }
say "booting stock F44 EFI + --snapshot-file on $PORT..."
RUST_LOG=limina_vmm=info,krun_devices=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 2048 --net --ssh-port "$PORT" --snapshot-file "$SNAP" > "$BOOTLOG" 2>&1 &
LP=$!; say "limina pid=$LP"
up=0; for i in $(seq 1 60); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP 2>/dev/null; exit 1; }
W=$(wpid); say "worker=$W"

say "STEP1: pulse SUSPEND button (SIGUSR2 -> $W); expect guest s2idle (SSH drops, no wake armed)"
kill -USR2 "$W"
frozen=0; for i in $(seq 1 20); do
  sleep 2
  if ! ssh_up; then frozen=1; say "guest unreachable (poll $i) — s2idle frozen"; break; fi
done
[ "$frozen" = 1 ] || say "WARN: guest still reachable after suspend pulse (did it enter s2idle?)"
# give it a moment to settle fully into s2idle
sleep 3

say "STEP2: trigger SNAPSHOT (SIGUSR1 -> $W) on the s2idle'd guest"
kill -USR1 "$W"
# worker should save then exit 126; supervisor exits with 126
gone=0; for i in $(seq 1 30); do kill -0 "$LP" 2>/dev/null || { gone=1; break; }; sleep 2; done
if [ "$gone" = 1 ]; then wait "$LP" 2>/dev/null; RC=$?; else RC="(still running — HUNG)"; fi
say "supervisor exit: gone=$gone rc=$RC (want 126)"
say "snapshot file:"; ls -la "$SNAP" 2>/dev/null | tee -a "$LOG" || say "  (NOT written)"
say "=== worker snapshot/quiesce log ==="
grep -iE 'snapshot|quiesc|pause|Snapshot|resumed|save|suspend key|s2idle|freeze' "$BOOTLOG" | tail -25 | tee -a "$LOG"
if [ "$gone" = 1 ] && [ "$RC" = 126 ] && [ -s "$SNAP" ]; then
  say "VERDICT: SAVE-HALF GREEN — snapshotted an s2idle'd guest (exit 126, file written)"
else
  say "VERDICT: SAVE-HALF FAIL/HUNG — inspect log (gone=$gone rc=$RC)"
fi
say "cleanup"; kill -9 $LP 2>/dev/null; pkill -9 -f "m92-comp.raw" 2>/dev/null; rm -f "$DISK"; say "spike done."
