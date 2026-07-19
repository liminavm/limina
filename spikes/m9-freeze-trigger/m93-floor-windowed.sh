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
# ROUND 5 LESSON: a bare directive list (limina_vmm=info,krun_vmm=info) leaves every OTHER target
# at error-only, silently suppressing the [GPUTRACE] reporter + per-event stale-ctx warns (target
# krun_devices) AND the vkr context dump (vkr_log = virgl INFO → target krun_rutabaga_gfx). The
# bare `warn` default restores production visibility; rutabaga at info carries the dump lines.
export RUST_LOG="${RUST_LOG:-warn,limina_vmm=info,krun_vmm=info,krun_rutabaga_gfx=info}"
# ROUND 4+ (M9.3 probes, libkrun 0071 + virglrenderer 0033): counted GPU evidence. GPU_TRACE=1
# emits one [GPUTRACE] aggregate line per 2s tick (submit/unknown_ctx/unknown_res deltas + the
# fence ledger with outstanding ages); GPU_TRACE_VKR=1 additionally dumps the vkr context table
# (objects tallied by VkObjectType — the retain-and-replay bill of materials on a healthy
# session) every 10th tick, and ALWAYS on the first stale-ctx submission after restore.
export LIMINA_GPU_TRACE="${LIMINA_GPU_TRACE:-1}" LIMINA_GPU_TRACE_VKR="${LIMINA_GPU_TRACE_VKR:-1}"
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
cp -f "$WLOG" "$P1LOG" 2>/dev/null
say "=== phase1 [GPUTRACE] healthy-session baseline (fence flow + vkr object tally = replay bill of materials) ==="
grep -E '\[GPUTRACE\]' "$P1LOG" 2>/dev/null | tail -25 | tee -a "$LOG" || say "  (no GPUTRACE lines — worker built without 0071, or env not reaching it)"
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
# ROUND 8: LIMINA_MMIO_TRACE=1 (phase 2 only — phase-1 boot negotiation is noise) logs every
# transport register write post-restore: the direct observation of whether the guest re-programs
# each device's queue addresses on s2idle thaw (prediction: blk/net DO — driver PM ops re-create
# vqs; virtio-gpu does NOT — no PM ops → DRIVER_OK re-driven onto DEAD queues = the wedge).
LIMINA_MMIO_TRACE=1 LIMINA_EXTRA_ARGS="--ssh-port $PORT --restore $SNAP --console $P2CONS" \
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
  # ROUND 3 (Fable r2): the OS survived — the interesting question is now whether the GNOME/venus
  # SESSION wedged (predicted: silent virtio-gpu ring/fence wait, no crash/log). These guest-side
  # oracles are the real signal; the SHM-window fault below is EXPECTED-SILENT (dead oracle — the
  # window is hv_vm_map'd on restore, never faults) and is NOT part of the verdict.
  # ROUND 7: every guest probe is host-side timeboxed (round 6: a probe that touched the GPU
  # went D-state — guest `timeout` can't kill it — and stalled the script 30 min; later even
  # FRESH ssh sessions hung as the guest degraded, so guest-side timeouts are not enough).
  ssht(){ local t=$1; shift; $SSH "$@" 2>/dev/null & local p=$!; ( sleep "$t"; kill "$p" 2>/dev/null ) & local k=$!; wait "$p" 2>/dev/null; local rc=$?; kill "$k" 2>/dev/null; wait "$k" 2>/dev/null; return $rc; }
  say "=== GUEST WEDGE PROBES (over SSH) ==="
  say "--- ⭐ EARLY: PM resume trace (did dpm_resume finish? which device resumed last?) ---"
  ssht 30 "sudo dmesg 2>/dev/null | grep -aE 'PM:|s2idle|suspend|resume|virtio|Freezing|Restarting' | tail -25" | tee -a "$LOG"
  say "--- ⭐ EARLY: arm hung-task detector (15s) + dmesg stream + a venus CANARY (expected to D-state; its hung-task report names the exact stall stack) ---"
  ssht 20 'sudo sysctl -w kernel.hung_task_timeout_secs=15 >/dev/null 2>&1; sudo -u claude XDG_RUNTIME_DIR=/run/user/$(id -u) nohup vulkaninfo >/tmp/vkinfo.out 2>&1 & echo "(venus canary launched)"' | tee -a "$LOG"
  DMSTREAM="$JOB/tmp/m93-p2-dmesg-stream.log"
  $SSH 'sudo dmesg -w' >"$DMSTREAM" 2>/dev/null &
  DMPID=$!
  say "dmesg streaming → $DMSTREAM; giving the canary 50s to trip the hung-task detector…"
  sleep 50
  say "--- ⭐ D-STATE SWEEP (canary should be D; stacks below name the wedge) ---"
  ssht 30 "ps axo pid,stat,wchan:32,comm | awk 'NR==1 || \$2 ~ /^D/'" | tee -a "$LOG"
  say "--- D-state kernel stacks (what fence/queue are they blocked on?) ---"
  ssht 30 'for p in $(ps axo pid,stat | awk "\$2 ~ /^D/ {print \$1}"); do echo "== pid $p ($(cat /proc/$p/comm 2>/dev/null)) =="; sudo cat /proc/$p/stack 2>/dev/null | head -14; done | head -80 || echo "(no D-state procs)"' | tee -a "$LOG"
  say "--- ⭐ DRM atomic commit state (is a commit stuck? which FB does the CRTC think it scans?) ---"
  ssht 30 'sudo cat /sys/kernel/debug/dri/0/state 2>/dev/null | head -40 || echo "(no dri/0/state — debugfs off or driver mismatch)"' | tee -a "$LOG"
  say "--- gnome-shell / Xwayland state (R=run D=uninterruptible S=sleep; wchan=blocked-on) ---"
  ssht 30 "ps -eo stat,wchan:24,comm | egrep 'gnome-shell|Xwayland|gnome-remote|mutter' || echo '(no compositor procs)'" | tee -a "$LOG"
  say "--- gnome-shell kernel stack (is it parked in a virtio-gpu/dma-fence wait?) ---"
  ssht 30 'p=$(pgrep -x gnome-shell | head -1); [ -n "$p" ] && sudo cat /proc/$p/stack 2>/dev/null | head -12 || echo "(no gnome-shell pid / no stack)"' | tee -a "$LOG"
  say "--- dmesg tail: virtio_gpu / drm timeouts, fence waits ---"
  ssht 30 "sudo dmesg 2>/dev/null | egrep -i 'virtio.?gpu|drm|fence|timeout|gpu hang' | tail -12 || echo '(none)'" | tee -a "$LOG"
  say "--- journal (last 3 min): compositor / session errors ---"
  ssht 30 "sudo journalctl -b --since '-3 min' --no-pager 2>/dev/null | egrep -i 'gnome-shell|mutter|venus|vulkan|virtio|gpu|fail|error' | tail -15 || echo '(none)'" | tee -a "$LOG"
  say "=== ⭐ HUNG-TASK REPORTS from the dmesg stream (the exact guest stall stacks) ==="
  kill "$DMPID" 2>/dev/null; wait "$DMPID" 2>/dev/null
  grep -aA16 "blocked for more" "$DMSTREAM" 2>/dev/null | head -70 | tee -a "$LOG" || true
  [ -s "$DMSTREAM" ] || say "  (dmesg stream empty — sshd may already be degraded)"
fi
say "=== per-vCPU RESUME PCs (needs RUST_LOG=krun_vmm=info) — did the vCPUs come back? ==="
grep -iE 'resumed from snapshot at pc=' "$P2LOG" 2>/dev/null | tail -8 | tee -a "$LOG" || true
say "=== guest CONSOLE tail (where the kernel resume stalls) ==="
tail -25 "$P2CONS" 2>/dev/null | tee -a "$LOG" || say "  (no console captured)"
say "=== SHM-WINDOW FAULT (DEAD ORACLE — expected silent, NOT part of the verdict) ==="
if [ -n "$fault" ]; then say "  FAULT SEEN → $fault (SURPRISING — the window is normally hv_vm_map'd on restore; investigate)"; else say "  (silent, as expected: the window is hv_vm_map'd on restore so a touch never faults — this line proves nothing)"; fi
say "=== phase2 ⭐ [GPUTRACE] COUNTED EVIDENCE (stale-ctx submissions / fence ledger / fresh-renderer dump) ==="
grep -E '\[GPUTRACE\]' "$P2LOG" 2>/dev/null | tail -35 | tee -a "$LOG" || say "  (no GPUTRACE lines)"
say "=== phase2 ⭐ [MMIOTRACE] post-thaw transport writes per device (did the guest re-program THIS device's queues?) ==="
for dev in $(grep -aoE '\[MMIOTRACE\] [a-z0-9_-]+' "$P2LOG" 2>/dev/null | awk '{print $2}' | sort -u); do
  QW=$(grep -ac "\[MMIOTRACE\] $dev write 0x\(80\|84\|90\|94\|a0\|a4\|44\)" "$P2LOG" 2>/dev/null || echo 0)
  ST=$(grep -a "\[MMIOTRACE\] $dev write 0x70" "$P2LOG" 2>/dev/null | tail -1 | grep -oE '= 0x[0-9a-f]+' || echo '= none')
  say "  $dev: queue-geometry writes=$QW last-status $ST"
done
say "--- gpu MMIOTRACE sequence (full) ---"
grep -a "\[MMIOTRACE\] gpu" "$P2LOG" 2>/dev/null | head -40 | tee -a "$LOG" || say "  (no gpu transport writes post-restore)"
say "=== phase2 per-event stale-ctx/resource warns (first 10) ==="
grep -E 'ErrRutabaga\(Invalid(Context|Resource)Id\)|ErrInvalid(Context|Resource)Id' "$P2LOG" 2>/dev/null | head -10 | tee -a "$LOG" || say "  (none)"
say "=== phase2 worker log — restore/wake/venus/error (tcpproxy filtered) ==="; grep -iE 'restoring from snapshot|injecting guest wake|SHM-WINDOW FAULT|resumed from snapshot|Mesa:|venus|virgl_renderer|error|panic|segfault' "$P2LOG" 2>/dev/null | grep -viE 'tcpproxy|no route to host|GPUTRACE' | tail -25 | tee -a "$LOG"
say ""
UC=$(grep -oE 'unknown_ctx=\+[0-9]+' "$P2LOG" 2>/dev/null | awk -F'+' '{s+=$2} END {print s+0}')
UR=$(grep -oE 'unknown_res=\+[0-9]+' "$P2LOG" 2>/dev/null | awk -F'+' '{s+=$2} END {print s+0}')
FREQ=$(grep -oE 'fences_req=\+[0-9]+' "$P2LOG" 2>/dev/null | awk -F'+' '{s+=$2} END {print s+0}')
FRET=$(grep -oE 'fences_ret=\+[0-9]+' "$P2LOG" 2>/dev/null | awk -F'+' '{s+=$2} END {print s+0}')
OUTLAST=$(grep -oE 'outstanding=[0-9]+' "$P2LOG" 2>/dev/null | tail -1 | cut -d= -f2)
say "SUMMARY (machine oracles):"
say "  [GPUTRACE] phase2 totals: unknown_ctx=$UC unknown_res=$UR fences req=$FREQ ret=$FRET outstanding(last tick)=${OUTLAST:-?}"
say "  OS survived restore (SSH): $([ "$back" = 1 ] && echo YES || echo NO)"
say "  boot_id continuity: $([ -n "${POST_BOOTID:-}" ] && ([ "$PRE_BOOTID" = "$POST_BOOTID" ] && echo 'SAME (resumed)' || echo 'CHANGED (rebooted)') || echo unknown)"
say "  SHM-window GPU fault: $([ -n "$fault" ] && echo 'YES (unexpected — investigate)' || echo 'silent (expected — dead oracle, ignore)')"
if [ "$KEEP_WINDOW" = 1 ]; then
  say "  → DESKTOP recovery is the HUMAN oracle: eyeball the window. Kill later: pkill -9 -f m93-floor.raw"
else
  say "cleanup (LIMINA_KEEP_WINDOW=1 to keep the window for a human eyeball)"; cleanup
  # ROUND 8: keep DISK+SNAP by default — they enable fast restore-only iteration (a matched
  # clone+snapshot pair); LIMINA_KEEP_ARTIFACTS=0 to delete.
  [ "${LIMINA_KEEP_ARTIFACTS:-1}" = 1 ] || rm -f "$DISK" "$SNAP"
fi
say "spike done."
