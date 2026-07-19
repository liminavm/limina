#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# M9.3 R2 — the behavioral RED/GREEN for virtio-gpu transport-state restore, HEADLESS + software-2D.
# No venus, no KK, no window: a --gpu-software-2d + --display-capture boot still gives the guest a real
# virtio-gpu transport whose kernel driver goes DRIVER_OK and stays there (no s2idle PM ops) — the exact
# sticky-transport that round 5 proved wedges the restored guest. Sequence: boot, suspend-bracket
# snapshot, restore, then over SSH force an fbdev damage flush through the control queue
# (`dd .. of=/dev/fb0`) and confirm the guest is NOT D-wedged (a follow-up `ps -e` completes). Pre-fix
# (no transport restore) the dd/ps D-hang (round-5 mechanism); with the fix both complete + same boot_id.
# Machine-observable, no eyeball. Clone only; never the source image.
set -uo pipefail
REPO=~/Projects/limina
JOB="${CLAUDE_JOB_DIR:-~/.claude/jobs/916f2b3c}"
SRC="${LIMINA_SRC_IMAGE:-Fedora-Workstation-44.enhanced.test.raw}"
FW="$REPO/target/krun-efi/KRUN_EFI.gop.fd"
LOG="$JOB/tmp/m93-r2.log"; DISK="$JOB/tmp/m93-r2.raw"; SNAP="$JOB/tmp/m93-r2-snap.bin"
PNG="$JOB/tmp/m93-r2-capture.png"
B1="$JOB/tmp/m93-r2-b1.log"; B2="$JOB/tmp/m93-r2-b2.log"
PORT=2248
CPUS="${LIMINA_CPUS:-4}"; RAM="${LIMINA_RAM_MIB:-4096}"
SSH="ssh -p $PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=6 -o BatchMode=yes -o LogLevel=ERROR claude@127.0.0.1"
: > "$LOG"; say(){ echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
wpid(){ pgrep -f "limina-vmm.*m93-r2.raw" | head -1; }
ssh_up(){ $SSH true 2>/dev/null; }
cd "$REPO"; rm -f "$SNAP" "$PNG"
[ -f "$SRC" ] || { say "source image $SRC not found"; exit 1; }
say "cloning $SRC (COW), HEADLESS sw-2D ${CPUS}cpu/${RAM}mib…"; cp -c "$SRC" "$DISK" || { say "clone failed"; exit 1; }

### PHASE 1 — headless software-2D boot (real virtio-gpu, no window) + suspend bracket snapshot
say "PHASE1: headless software-2D boot + suspend bracket"
RUST_LOG=limina_vmm=info,krun_vmm=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --vmm-bin "$REPO/target/debug/limina-vmm" --firmware "$FW" \
  --disk "$DISK" --cpus "$CPUS" --ram-mib "$RAM" --net --ssh-port "$PORT" \
  --gpu-software-2d --display-capture "$PNG" --snapshot-file "$SNAP" > "$B1" 2>&1 &
LP1=$!; say "limina#1 pid=$LP1 (headless sw-2D GPU, capture=$PNG)"
up=0; for i in $(seq 1 90); do ssh_up && { up=1; say "SSH up ~$((i*4))s"; break; }; sleep 4; done
[ "$up" = 1 ] || { say "no boot"; kill -9 $LP1 2>/dev/null; pkill -9 -f m93-r2.raw; rm -f "$DISK"; exit 1; }
# Confirm the guest really has a virtio-gpu fbdev (the transport under test).
FB=$($SSH 'ls /dev/fb0 2>/dev/null && cat /sys/class/graphics/fb0/name 2>/dev/null' 2>/dev/null | tr '\n' ' ')
say "guest fbdev: ${FB:-MISSING}"
PRE=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d '\r')
say "pre: boot_id=$PRE; firing bracket (SIGTSTP)"
W=$(wpid); kill -TSTP "$W"
for i in $(seq 1 40); do kill -0 "$LP1" 2>/dev/null || break; sleep 2; done
wait "$LP1" 2>/dev/null; RC1=$?
SZ=$(ls -la "$SNAP" 2>/dev/null | awk '{print $5}')
say "phase1 exit rc=$RC1 (want 126); snapshot=${SZ:-MISSING}"
grep -iE 'capturing transport|did not drain|virtio device type=16' "$B1" | tail -6 | tee -a "$LOG"
[ "$RC1" = 126 ] && [ -s "$SNAP" ] || { say "PHASE1 did not snapshot — abort"; pkill -9 -f m93-r2.raw; rm -f "$DISK" "$SNAP"; exit 1; }
# NON-VACUOUSNESS (Fable): the snapshot MUST have captured a virtio-gpu (type=16) transport, else the
# headless boot created no GPU device and R2 would "pass" testing nothing.
if ! grep -q 'capturing transport for virtio type=16' "$B1"; then
  say "PHASE1 VACUOUS: no 'capturing transport for virtio type=16' — the GPU device was not present/sticky."
  say "  (R2 requires --gpu-software-2d + --display-capture so a real virtio-gpu transport exists.)"
  pkill -9 -f m93-r2.raw; rm -f "$DISK" "$SNAP"; exit 1
fi
say "non-vacuous: virtio-gpu (type=16) transport WAS captured."

### PHASE 2 — restore, then exercise the GPU control queue + prove no D-wedge
# LIMINA_R2_ARM=red sets LIMINA_SKIP_TRANSPORT_RESTORE so the worker rebuilds the GPU in INIT (pre-fix
# behavior). This is the discrimination proof: the SAME test must go RED with the fix disabled.
ARM="${LIMINA_R2_ARM:-green}"
if [ "$ARM" = red ]; then say "PHASE2: restore with LIMINA_SKIP_TRANSPORT_RESTORE=1 (RED baseline arm)"; export LIMINA_SKIP_TRANSPORT_RESTORE=1
else say "PHASE2: restore (--restore) headless sw-2D"; fi
RUST_LOG=limina_vmm=info,krun_vmm=info caffeinate -dimsu "$REPO/target/debug/limina" \
  --vmm-bin "$REPO/target/debug/limina-vmm" --firmware "$FW" \
  --disk "$DISK" --cpus "$CPUS" --ram-mib "$RAM" --net --ssh-port "$PORT" \
  --gpu-software-2d --display-capture "$PNG" --restore "$SNAP" > "$B2" 2>&1 &
LP2=$!; say "limina#2 pid=$LP2 (restoring)"
back=0; for i in $(seq 1 40); do sleep 3; ssh_up && { back=1; say "SSH BACK ~$((i*3))s"; break; }; done
grep -iE 'transport restore|restore: |resumed from snapshot at pc=|layout' "$B2" 2>/dev/null | tail -8 | tee -a "$LOG" || true
if [ "$back" != 1 ]; then
  W2=$(wpid); CPU=$([ -n "$W2" ] && ps -o %cpu= -p "$W2" | tr -d ' ' || echo n/a)
  say "VERDICT: FAIL — no SSH after restore (worker cpu=$CPU). Transport restore did not bring the guest back."
  grep -iE 'restor|transport|error|panic|drain' "$B2" 2>/dev/null | grep -viE 'tcpproxy|no route' | tail -15 | tee -a "$LOG"
  kill -9 $LP2 2>/dev/null; pkill -9 -f m93-r2.raw; rm -f "$DISK" "$SNAP"; exit 1
fi
POST=$($SSH 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null | tr -d '\r')
say "post: boot_id=$POST ($([ "$PRE" = "$POST" ] && echo SAME=resumed || echo CHANGED=rebooted))"

# THE keystone: force a SYNCHRONOUS, FENCED virtio-gpu control-queue command. A plain `dd >/dev/fb0`
# is NOT enough — fbdev deferred-io only marks pages dirty and flushes later in a kworker, so the
# write returns instantly (a shadow-buffer memcpy) whether or not the transport is live (this false-
# GREENed both arms once). `conv=fsync` calls fb_deferred_io_fsync → flush_delayed_work → the damage
# flush runs INLINE and blocks on the control-queue fence, so a dead transport D-hangs dd itself
# (timeout → rc=124). We also kick it in the background and sample D-state 4s later to catch any async
# GPU kworker that parks in D (the round-5 virtio_gpu_queue_ctrl_sgs / commit_work signature).
say "--- GPU control-queue exercise: SYNCHRONOUS fenced flush (dd conv=fsync) + delayed D-state ---"
PROBE=$($SSH 'bash -s' <<'EOS' 2>&1
set -u
# background a synchronous, fenced fbdev flush; a dead control queue wedges it in D
sudo sh -c 'timeout 15 dd if=/dev/zero of=/dev/fb0 bs=4096 count=64 conv=fsync >/tmp/dd.out 2>&1; echo $? >/tmp/dd.rc' &
sleep 4
echo "DSTATE_BEGIN"
ps -eo stat,comm,wchan:28 2>/dev/null | awk '$1 ~ /^D/'
echo "DSTATE_END"
echo "D_COUNT=$(ps -eo stat 2>/dev/null | grep -c '^D')"
# wait out the flush (or its timeout) and report rc: 0 = completed, 124 = HUNG on a dead queue
for i in $(seq 1 14); do [ -f /tmp/dd.rc ] && break; sleep 1; done
echo "DD_RC=$(cat /tmp/dd.rc 2>/dev/null || echo STILL_HUNG)"
echo "PS_RC=$(timeout 8 ps -e >/dev/null 2>&1; echo $?)"
echo "GPU_STATUS=$(cat /sys/bus/virtio/devices/*/status 2>/dev/null | tr '\n' ',')"
sudo dmesg 2>/dev/null | grep -iE 'virtio_gpu|hung task|blocked for more than' | tail -3
EOS
)
echo "$PROBE" | sed 's/^/    /' | tee -a "$LOG"
DDRC=$(echo "$PROBE" | grep -oE 'DD_RC=[0-9A-Z_]+' | cut -d= -f2)
PSRC=$(echo "$PROBE" | grep -oE 'PS_RC=[0-9]+' | cut -d= -f2)
DCOUNT=$(echo "$PROBE" | grep -oE 'D_COUNT=[0-9]+' | cut -d= -f2)

# Restore-log health (Fable's (e) list): any of these warns means the negotiation replay hit a bad
# gate — a RED signal even if the oracles pass.
WARNS=$(grep -iE 'invalid state|invalid virtio driver status transition|ack virtio features in invalid|unknown virtio mmio register write|marked ready with QueueNum=0|transport restore failed|refusing \(fail closed\)' "$B2" 2>/dev/null | tail -6)
say "--- restore-log negotiation-replay warnings (want NONE) ---"
[ -n "$WARNS" ] && { echo "$WARNS" | tee -a "$LOG"; } || say "  (none)"

ok_dd=$([ "$DDRC" = 0 ] && echo yes || echo no)   # 124/STILL_HUNG = wedged on a dead control queue
ok_ps=$([ "${PSRC:-1}" = 0 ] && echo yes || echo no)
ok_dstate=$([ "${DCOUNT:-9}" -le 1 ] 2>/dev/null && echo yes || echo no)
ok_warns=$([ -z "$WARNS" ] && echo yes || echo no)
say ""
say "SUMMARY (R2 machine oracles) [arm=$ARM]:"
say "  resumed (same boot_id):        $([ "$PRE" = "$POST" ] && echo YES || echo NO)"
say "  synchronous flush (dd fsync):  $ok_dd (DD_RC=${DDRC:-?}; 0=completed, 124/STILL_HUNG=dead queue)"
say "  guest responsive (ps -e):      $ok_ps (PS_RC=${PSRC:-?})"
say "  D-state pileup:                $ok_dstate (D_COUNT=${DCOUNT:-?}; 0-1 healthy, >1=wedge)"
say "  no replay warns:               $ok_warns"
GREEN=no
if [ "$ok_dd" = yes ] && [ "$ok_ps" = yes ] && [ "$ok_dstate" = yes ] && [ "$PRE" = "$POST" ] && [ "$ok_warns" = yes ]; then
  GREEN=yes
  say "  ==> R2 GREEN: restored guest's virtio-gpu transport is LIVE (fenced flush completes, no wedge)."
else
  say "  ==> R2 RED: the restored transport is DEAD (fenced flush D-hangs / D-pileup) — the round-5 wedge."
fi
# Discrimination check: the RED arm MUST be RED, the GREEN arm MUST be GREEN.
if [ "$ARM" = red ] && [ "$GREEN" = yes ]; then
  say "  !! DISCRIMINATION FAILURE: RED arm (skip transport restore) did NOT wedge — oracle is not exercising the sticky transport."
fi
if [ "${LIMINA_R2_KEEP:-0}" = 1 ]; then
  say "LIMINA_R2_KEEP=1 — leaving worker (pid $LP2) + guest UP for inspection. Teardown: pkill -9 -f m93-r2.raw ; rm -f $DISK $SNAP $PNG"
else
  say "cleanup"; kill -9 $LP2 2>/dev/null; pkill -9 -f "m93-r2.raw" 2>/dev/null; rm -f "$DISK" "$SNAP" "$PNG"
fi
say "R2 done."
