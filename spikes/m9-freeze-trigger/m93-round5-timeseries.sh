#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.3 ROUND 5 — clean longitudinal A-vs-B time-series after a windowed venus restore.
#
# Round-4 left two hypotheses for the post-restore black screen:
#   (A) resume-notification gap: the display is never re-enabled (CRTC stays active=0, zero GPU
#       traffic, no D-state GPU kworker at ANY tick) — mutter/logind never ran output re-enable.
#   (B) delayed transport-dead: the display re-enable DOES fire later; the first GPU touch then
#       parks in D (CRTC flips active=1 and/or a virtio/dma-fence kworker appears, and/or the host
#       sees a GPU kick then silence).
# Round-4's 12s sample favoured (A); its 3-min D-pileup favoured (B) but was CONFOUNDED (the
# vulkaninfo probe itself D-hangs on the GPU + concurrent manual ssh). Round 5 = same spike, but
# phase 2 takes ONE ssh round-trip per tick at t=5,15,30,60,120s and NOTHING else touches the
# guest: no vulkaninfo, no redraw forcing, no extra sessions. Guest-side `timeout` guards every
# sub-probe so a D-hung /proc read can't wedge a sample.
#
# Phase 1 is m93-floor-windowed.sh's phase 1 unchanged (windowed venus boot, M9.2 bracket,
# snapshot, worker exit). Needs the KK mount + signed worker. Boots a CLONE (never the source).
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
mkdir -p "$JOB/tmp"
SRC="${LIMINA_SRC_IMAGE:-Fedora-Workstation-44.enhanced.test.raw}"
LOG="$JOB/tmp/m93-round5.log"; DISK="$JOB/tmp/m93-round5.raw"; SNAP="$JOB/tmp/m93-round5-snap.bin"
WLOG="/tmp/enhanced-efi-kk-worker.log"   # boot-enhanced-efi-kk.sh hardcodes this
P1LOG="$JOB/tmp/m93-round5-p1.log"; P2LOG="$JOB/tmp/m93-round5-p2.log"
P1CONS="$JOB/tmp/m93-round5-p1-console.log"; P2CONS="$JOB/tmp/m93-round5-p2-console.log"
PORT=2246
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=6 -o BatchMode=yes -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*m93-round5.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cleanup(){ pkill -9 -f "m93-round5.raw" 2>/dev/null; }
cd "$REPO"; rm -f "$SNAP"
[ -f "$SRC" ] || { say "source image $SRC not found"; exit 1; }
say "cloning enhanced image $SRC (COW)…"; cp -c "$SRC" "$DISK" || { say "clone failed"; exit 1; }

export RUST_LOG="${RUST_LOG:-limina_vmm=debug,krun_vmm=debug}"
export LIMINA_DISK="$DISK" LIMINA_CPUS=4 LIMINA_RAM_MIB=4096

### PHASE 1 — windowed venus boot + suspend bracket + snapshot (unchanged from m93-floor-windowed.sh)
say "PHASE1: windowed venus boot (a WINDOW will appear) + snapshot bracket"
LIMINA_EXTRA_ARGS="--ssh-port $PORT --snapshot-file $SNAP --console $P1CONS" \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh >"$JOB/tmp/m93-r5-p1-boot.out" 2>&1 &
BOOTPID=$!
up=0; for i in $(seq 1 75); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "PHASE1 FAIL: no SSH (guest never booted)"; cp -f "$WLOG" "$P1LOG" 2>/dev/null; cleanup; rm -f "$DISK"; exit 1; }
say "SSH up; settling 75s for the seated GNOME/venus desktop…"; sleep 75
PRE_BOOTID=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d '\r')
say "pre: boot_id=$PRE_BOOTID"
W=$(wpid); say "worker pid=$W; firing suspend bracket (SIGTSTP → button → quiesce poll → snapshot)"
kill -TSTP "$W"
rc=none; for i in $(seq 1 40); do
  if ! kill -0 "$BOOTPID" 2>/dev/null; then wait "$BOOTPID" 2>/dev/null; rc=$?; break; fi
  kill -0 "$W" 2>/dev/null || { sleep 2; rc=exited; break; }
  sleep 2
done
cp -f "$WLOG" "$P1LOG" 2>/dev/null
SNAP_SZ=$(stat -f%z "$SNAP" 2>/dev/null)
say "PHASE1 result: boot-script rc=$rc; snapshot=${SNAP_SZ:-MISSING}"
if [ -z "${SNAP_SZ:-}" ] || [ "${SNAP_SZ:-0}" -lt 4096 ]; then
  say "PHASE1 FAIL: no snapshot written"; cleanup; rm -f "$DISK" "$SNAP"; exit 1
fi
say "PHASE1 GREEN: snapshotted ($SNAP_SZ bytes). Worker torn down."
cleanup; sleep 3

### PHASE 2 — restore into a fresh worker + NON-PERTURBING time-series
say "PHASE2: restore (a WINDOW will appear again); t0 = restore launch"
LIMINA_EXTRA_ARGS="--ssh-port $PORT --restore $SNAP --console $P2CONS" \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh >"$JOB/tmp/m93-r5-p2-boot.out" 2>&1 &
T0=$(date +%s)

# One ssh round-trip per tick. Heredoc is quoted → everything runs guest-side. Every sub-probe is
# wrapped in `timeout` so a D-hung /proc read can't wedge the sample (round-4 lesson).
sample(){
  local t=$1
  say "--- SAMPLE t=${t}s ---"
  $SSH 'bash -s' <<'EOS' 2>&1 | tee -a "$LOG"
echo "== CRTC =="
sudo timeout 8 grep -E 'crtc-[0-9]+:|enable=|active=' /sys/kernel/debug/dri/0/state 2>&1 | head -12
echo "== DSTATE (procs in D) =="
timeout 8 ps -eo stat,wchan:28,comm 2>/dev/null | awk '$1 ~ /^D/'
echo "D_COUNT=$(timeout 8 ps -eo stat 2>/dev/null | grep -c '^D')"
echo "== LOAD =="
cat /proc/loadavg
EOS
  local rc=$?
  say "sample t=${t}s ssh_rc=$rc $( [ $rc -ne 0 ] && echo '(SSH FAILED — guest not reachable at this tick)' )"
}

for T in 5 15 30 60 120; do
  now=$(date +%s); target=$((T0 + T)); d=$((target - now))
  [ "$d" -gt 0 ] && sleep "$d"
  sample "$T"
done

# End-of-run: boot_id continuity (one extra ssh AFTER the sampling window — allowed)
POST_BOOTID=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d '\r')
say "post: boot_id=$POST_BOOTID ($([ -n "$POST_BOOTID" ] && ([ "$PRE_BOOTID" = "$POST_BOOTID" ] && echo SAME=resumed || echo CHANGED=rebooted) || echo unreachable))"

# Host-side GPU-traffic verdict: the worker log, filtered for anything virtio-gpu shaped after
# the restore markers. Zero post-restore GPU lines = the guest never submitted.
cp -f "$WLOG" "$P2LOG" 2>/dev/null
say "=== HOST worker log: restore markers + any GPU traffic ==="
grep -inE 'KEY_WAKEUP|resumed from snapshot|restoring from snapshot' "$P2LOG" 2>/dev/null | tail -8 | tee -a "$LOG"
say "--- post-restore virtio-gpu/queue/scanout/fence lines (absence = zero GPU traffic) ---"
grep -inE 'gpu|kick|queue_notify|scanout|resource|flush|fence' "$P2LOG" 2>/dev/null | grep -viE 'tcpproxy|virgl_flags' | tail -30 | tee -a "$LOG"
say "=== P2 guest console tail ==="
tail -25 "$P2CONS" 2>/dev/null | tee -a "$LOG" || say "  (no console captured)"
say "round5 done. Window + guest LEFT UP for follow-up probing (disk m93-round5.raw)."
say "Teardown later: pkill -9 -f m93-round5.raw ; rm -f $DISK $SNAP"
