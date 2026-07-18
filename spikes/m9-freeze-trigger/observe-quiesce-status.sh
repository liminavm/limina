#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.2 PREMISE CHECK — what do virtio-mmio device_status registers actually read at s2idle-quiesce?
# The INIT-invariant assert / quiesce oracle rests on the assumption that a device quiesced by its
# driver's .suspend callback resets to INIT (0), and only virtio-gpu stays DRIVER_OK (0xf). Verify
# it empirically before designing the oracle: boot stock F44, pulse the suspend button, snapshot the
# frozen guest, and read the `snapshot: virtio device type=T id=.. device_status=0xNN` lines the
# worker now logs in save_snapshot. Observation only — no restore.
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
LOG="$JOB/tmp/observe-quiesce.log"; DISK="$JOB/tmp/observe-quiesce.raw"; SNAP="$JOB/tmp/observe-quiesce-snap.bin"
BOOT="$JOB/tmp/observe-quiesce-boot.log"
PORT=2231
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*observe-quiesce.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cd "$REPO"; rm -f "$SNAP"
say "cloning stock F44..."; cp -c Fedora-Workstation-44.accessible.raw "$DISK" || { say "clone failed"; exit 1; }

say "boot + suspend + snapshot (observe device_status at quiesce)"
RUST_LOG=limina_vmm=info,krun_vmm=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --firmware "$REPO/target/krun-efi/KRUN_EFI.gop.fd" \
  --disk "$DISK" --cpus 2 --ram-mib 1024 --net --ssh-port "$PORT" --snapshot-file "$SNAP" > "$BOOT" 2>&1 &
LP=$!; say "limina pid=$LP"
up=0; for i in $(seq 1 60); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP 2>/dev/null; pkill -9 -f observe-quiesce.raw; rm -f "$DISK"; exit 1; }
W=$(wpid)
say "suspend button (SIGUSR2 -> $W)"; kill -USR2 "$W"
fr=0; for i in $(seq 1 20); do sleep 2; ssh_up || { fr=1; say "frozen (poll $i, ~$((i*2))s after button)"; break; }; done
[ "$fr" = 1 ] || { say "did not freeze"; kill -9 $LP; pkill -9 -f observe-quiesce.raw; rm -f "$DISK"; exit 1; }
sleep 3
say "snapshot (SIGUSR1 -> $W)"; kill -USR1 "$W"
g=0; for i in $(seq 1 30); do kill -0 "$LP" 2>/dev/null || { g=1; break; }; sleep 2; done
wait "$LP" 2>/dev/null; RC=$?
say "exit rc=$RC (want 126); snap bytes: $(ls -la "$SNAP" 2>/dev/null | awk '{print $5}')"
say ""
say "=== DEVICE STATUS AT QUIESCE (the premise under test) ==="
grep -E 'snapshot: virtio device' "$BOOT" | tee -a "$LOG"
say "=== (virtio type ids: 1=net 2=blk 3=console 4=rng 5=balloon 16=gpu 19=vsock 26=fs) ==="
say ""
say "For reference — status transitions during the suspend window (last 40):"
grep -E 'status transition' "$BOOT" | tail -40 | tee -a "$LOG"
say "cleanup"; pkill -9 -f "observe-quiesce.raw" 2>/dev/null; rm -f "$DISK" "$SNAP"; say "done."
