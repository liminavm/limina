#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# ab-vkmark.sh — boot the CURRENTLY-BUILT worker on a fresh clone of the venus specimen,
# run vkmark (venus) N times reading the Score, and sample host wakeups under load, then
# tear the VM down. Used for the ring-relax latency vs host-wakeup A/B (task #38/#39 retune).
#
# Usage:  spikes/wakeup-probe/ab-vkmark.sh <label> [runs]
#   env:  LIMINA_AB_DISK   base image to clone   (default host-sleep-eyeball.raw)
#         LIMINA_AB_CPUS   vcpus                 (default 6)
#         LIMINA_AB_OUT    output dir            (default $CLAUDE_JOB_DIR/tmp or /tmp)
# Prereqs: worker already built+signed (crates/limina-vmm/sign.sh) and linking the intended
#          virgl-prefix; /Volumes/mesa-cs mounted; firmware at target/krun-efi/KRUN_EFI.gop.fd.
set -euo pipefail
cd "$(dirname "$0")/../.."

LABEL="${1:?usage: ab-vkmark.sh <label> [runs]}"
RUNS="${2:-2}"
BASE="${LIMINA_AB_DISK:-host-sleep-eyeball.raw}"
CPUS="${LIMINA_AB_CPUS:-6}"
OUT="${LIMINA_AB_OUT:-${CLAUDE_JOB_DIR:-/tmp}/tmp}"
mkdir -p "$OUT"
CLONE="$OUT/ab-$LABEL.raw"
BOOTLOG="$OUT/ab-$LABEL-boot.log"
WLOG=/tmp/enhanced-efi-kk-worker.log
GENV='export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json VN_PERF=no_fence_feedback'
ssh_g() { ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
             -o BatchMode=yes -o ConnectTimeout=8 -o ServerAliveInterval=30 claude@127.0.0.1 "$@"; }

echo "### AB leg: $LABEL (runs=$RUNS, cpus=$CPUS, base=$BASE)"
# fresh COW clone (instant on APFS) so every leg starts from identical guest state
rm -f "$CLONE"; cp -c "$BASE" "$CLONE"

# boot windowed venus+net in the background (the boot script exec's the supervisor)
LIMINA_DISK="$CLONE" LIMINA_CPUS="$CPUS" LIMINA_NET=1 LIMINA_GLOBAL_SCANOUT=1 \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$BOOTLOG" 2>&1 &
BOOT_PID=$!

# wait for ssh + a seated venus session
for _ in $(seq 1 60); do grep -qa "guest SSH forward ready" "$WLOG" 2>/dev/null && break; sleep 3; done
for _ in $(seq 1 40); do ssh_g true 2>/dev/null && break; sleep 3; done
sleep 8   # let the session settle
DEV=$(ssh_g "$GENV; vulkaninfo --summary 2>/dev/null | awk -F= '/deviceName/{print \$2; exit}'")
echo "venus device:$DEV"

WORKER=$(pgrep -f "target/debug/limina-vmm --cpus" | head -1)
echo "worker pid=$WORKER"

# idle baseline: seated desktop, no GPU load — the regime the relax backoff protects.
echo "-- $LABEL IDLE wakeups (5s x3, no load) --"
spikes/wakeup-probe/procwake "$WORKER" 5 3 2>&1 | awk 'NR>2{print "   idle intr/s="$5}'

for r in $(seq 1 "$RUNS"); do
  ssh_g "$GENV; vkmark -s 1280x720 2>&1" > "$OUT/ab-$LABEL-vkmark$r.log" 2>&1 &
  VKPID=$!
  sleep 12
  echo "-- $LABEL run $r wakeups (5s x3) --"
  spikes/wakeup-probe/procwake "$WORKER" 5 3 2>&1 | awk 'NR>2{print "   intr/s="$5}'
  wait $VKPID 2>/dev/null || true
  echo "$LABEL run $r: $(grep -E 'vkmark Score' "$OUT/ab-$LABEL-vkmark$r.log" || echo 'NO SCORE')"
done

# teardown
for p in $(pgrep -f "target/debug/limina-vmm|target/debug/limina "); do kill -9 "$p" 2>/dev/null || true; done
wait "$BOOT_PID" 2>/dev/null || true
sleep 2
echo "### $LABEL done"
