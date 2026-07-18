#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.3 FLOOR SPIKE (Fable 2026-07-18): run the EXISTING M9.2 suspend bracket on a WINDOWED venus guest
# (which HAS a virtio-gpu — excepted from the quiesce oracle), snapshot despite the GPU exception,
# restore into a fresh worker, and measure the honest floor. Two decisive machine oracles:
#   (1) does a seated-GNOME venus guest even suspend+snapshot? (phase 1: worker exit 126)
#   (2) on restore, does the guest touch the SKIPPED SHM window (venus ring/VkDeviceMemory HOST3D
#       blobs the fresh worker has no mapping for)? → the instrumented "M9.3 SHM-WINDOW FAULT" warn.
#       And does the guest OS survive at all (SSH round-trip / same boot_id)?
# The DESKTOP-recovery question (gnome-shell black/frozen/recovered) is a HUMAN oracle — eyeball the
# window during phase 2. Needs the KK mount + a signed worker (venus). Boots a CLONE (never the source).
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
SRC="${LIMINA_SRC_IMAGE:-Fedora-Workstation-44.enhanced.test.raw}"
LOG="$JOB/tmp/m93-floor.log"; DISK="$JOB/tmp/m93-floor.raw"; SNAP="$JOB/tmp/m93-floor-snap.bin"
WLOG="/tmp/enhanced-efi-kk-worker.log"   # boot-enhanced-efi-kk.sh hardcodes this
P1LOG="$JOB/tmp/m93-floor-p1.log"; P2LOG="$JOB/tmp/m93-floor-p2.log"
PORT=2246
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o BatchMode=yes -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*m93-floor.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cleanup(){ pkill -9 -f "m93-floor.raw" 2>/dev/null; }
cd "$REPO"; rm -f "$SNAP"
[ -f "$SRC" ] || { say "source image $SRC not found"; exit 1; }
say "cloning enhanced image $SRC (COW)…"; cp -c "$SRC" "$DISK" || { say "clone failed"; exit 1; }

# Common env for the windowed venus boot (KK stack set by boot-enhanced-efi-kk.sh). RUST_LOG so the
# bracket (info) + the SHM-WINDOW FAULT (warn) + restore surface. ROUND 2: override to
# `limina_vmm=info,krun_vmm=info` to also get the per-vCPU "resumed from snapshot at pc=" lines +
# venus/Mesa. Console capture pins WHERE the guest kernel resume stalls.
export RUST_LOG="${RUST_LOG:-limina_vmm=info,krun_vmm=warn}"
export LIMINA_DISK="$DISK" LIMINA_CPUS=4 LIMINA_RAM_MIB=4096
P1CONS="$JOB/tmp/m93-floor-p1-console.log"; P2CONS="$JOB/tmp/m93-floor-p2-console.log"
KEEP_WINDOW="${LIMINA_KEEP_WINDOW:-0}"   # 1 = leave the restore window up for a human eyeball

### PHASE 1 — windowed venus boot + suspend bracket + snapshot
say "PHASE1: windowed venus boot (a WINDOW will appear) + snapshot bracket"
LIMINA_EXTRA_ARGS="--ssh-port $PORT --snapshot-file $SNAP --console $P1CONS" \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh >"$JOB/tmp/m93-p1-boot.out" 2>&1 &
BOOTPID=$!
up=0; for i in $(seq 1 75); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "PHASE1 FAIL: no SSH (guest never booted)"; cp -f "$WLOG" "$P1LOG" 2>/dev/null; cleanup; rm -f "$DISK"; exit 1; }
say "SSH up; settling 75s for the seated GNOME/venus desktop…"; sleep 75
PRE_BOOTID=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d '\r')
VENUS=$($SSH 'grep -c "Virtio-GPU Venus" <(vulkaninfo 2>/dev/null) || true' 2>/dev/null | tr -d '\r')
say "pre: boot_id=$PRE_BOOTID (venus-in-ssh=$VENUS — 0 is a FALSE negative over non-login ssh)"
W=$(wpid); say "worker pid=$W; firing suspend bracket (SIGTSTP → button → quiesce poll → snapshot)"
kill -TSTP "$W"
rc=none; for i in $(seq 1 40); do
  if ! kill -0 "$BOOTPID" 2>/dev/null; then wait "$BOOTPID" 2>/dev/null; rc=$?; break; fi
  # worker gone but boot script still waiting? check worker pid
  kill -0 "$W" 2>/dev/null || { sleep 2; rc=exited; break; }
  sleep 2
done
cp -f "$WLOG" "$P1LOG" 2>/dev/null
SNAP_SZ=$(ls -la "$SNAP" 2>/dev/null | awk '{print $5}')
say "PHASE1 result: boot-script rc=$rc; worker alive=$(kill -0 "$W" 2>/dev/null && echo yes || echo no); snapshot=${SNAP_SZ:-MISSING}"
say "=== phase1 worker log — bracket/quiesce/holdout ==="; grep -iE 'bracket:|quiesce|holdout|suspend entry|exiting 126' "$P1LOG" 2>/dev/null | tail -15 | tee -a "$LOG"
if [ -z "${SNAP_SZ:-}" ] || [ "${SNAP_SZ:-0}" -lt 4096 ] 2>/dev/null; then
  say "VERDICT PHASE1: windowed venus guest did NOT snapshot (bracket aborted — the guest did not"
  say "  s2idle-quiesce; likely GNOME inhibits the suspend button, or a device stayed non-INIT)."
  say "  This is itself a key floor finding: windowed suspend needs more than the M9.2 bracket."
  cleanup; rm -f "$DISK" "$SNAP"; exit 0
fi
say "PHASE1 GREEN: windowed venus guest suspended + snapshotted ($SNAP_SZ bytes). Worker torn down."
cleanup; sleep 3

### PHASE 2 — restore into a fresh worker; measure the SHM-window fault + OS survival
say "PHASE2: restore the windowed venus guest into a FRESH worker (a WINDOW will appear again)"
LIMINA_EXTRA_ARGS="--ssh-port $PORT --restore $SNAP --console $P2CONS" \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh >"$JOB/tmp/m93-p2-boot.out" 2>&1 &
BOOTPID2=$!
say "restore launched; watching for the SHM-window fault + SSH survival (90s)…"
back=0; fault=""; for i in $(seq 1 30); do
  sleep 3
  cp -f "$WLOG" "$P2LOG" 2>/dev/null
  [ -z "$fault" ] && fault=$(grep -m1 'SHM-WINDOW FAULT' "$P2LOG" 2>/dev/null || true)
  ssh_up && { back=1; say "SSH BACK after restore (~$((i*3))s) — guest OS survived"; break; }
done
cp -f "$WLOG" "$P2LOG" 2>/dev/null
[ -z "$fault" ] && fault=$(grep -m1 'SHM-WINDOW FAULT' "$P2LOG" 2>/dev/null || true)
if [ "$back" = 1 ]; then
  POST_BOOTID=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d '\r')
  say "post: boot_id=$POST_BOOTID ($([ "$PRE_BOOTID" = "$POST_BOOTID" ] && echo SAME=resumed || echo CHANGED=rebooted))"
fi
say "=== per-vCPU RESUME PCs (needs RUST_LOG=krun_vmm=info) — did the vCPUs come back? ==="
grep -iE 'resumed from snapshot at pc=' "$P2LOG" 2>/dev/null | tail -8 | tee -a "$LOG" || true
say "=== guest CONSOLE tail (where the kernel resume stalls) ==="
tail -25 "$P2CONS" 2>/dev/null | tee -a "$LOG" || say "  (no console captured)"
say "=== SHM-WINDOW FAULT (the GPU-gone-on-restore oracle) ==="
if [ -n "$fault" ]; then say "  FAULT SEEN → $fault"; else say "  (no SHM-window fault — guest did NOT reach a userspace GPU-window touch)"; fi
say "=== phase2 worker log — restore/wake/venus/error (tcpproxy filtered) ==="; grep -iE 'restoring from snapshot|injecting guest wake|SHM-WINDOW FAULT|resumed from snapshot|Mesa:|venus|virgl_renderer|error|panic|segfault' "$P2LOG" 2>/dev/null | grep -viE 'tcpproxy|no route to host' | tail -25 | tee -a "$LOG"
say ""
say "SUMMARY (machine oracles):"
say "  OS survived restore (SSH): $([ "$back" = 1 ] && echo YES || echo NO)"
say "  boot_id continuity: $([ -n "${POST_BOOTID:-}" ] && ([ "$PRE_BOOTID" = "$POST_BOOTID" ] && echo 'SAME (resumed)' || echo 'CHANGED (rebooted)') || echo unknown)"
say "  SHM-window GPU fault: $([ -n "$fault" ] && echo 'YES (mode i)' || echo 'not observed (stalls earlier)')"
if [ "$KEEP_WINDOW" = 1 ]; then
  say "  → DESKTOP recovery is the HUMAN oracle: eyeball the window. Kill later: pkill -9 -f m93-floor.raw"
else
  say "cleanup (LIMINA_KEEP_WINDOW=1 to keep the window for a human eyeball)"; cleanup; rm -f "$DISK" "$SNAP"
fi
say "spike done."
