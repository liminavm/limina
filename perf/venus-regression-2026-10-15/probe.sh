#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Mechanism probe, not a scored point: boot this tree's worker on metal.test (venus), with the
# KosmicKrisp and zink-on-KK of a parked bundle, VIRGLRS_RING_FENCE_STATS on, and run vkmark and a
# glFinish loop so the worker log says where each ring fence's time goes. Extra worker env (e.g.
# VIRGLRS_RING_FENCE_UNORDERED=1) passes through as trailing NAME=value arguments.
#
# The stats and the seam are not in virglrs: apply virglrs-ring-fence-probe.patch (against virglrs
# 2efa024) to this tree's third_party/virglrs and build the worker first.
#
# Usage: perf/venus-regression-2026-10-15/probe.sh <label> <mounted Limina.app> [NAME=value...]
set -uo pipefail
LABEL="${1:?label}"; APP="${2:?mounted Limina.app}"; shift 2
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=perf/venus-regression-2026-10-15; WORK=$DIR/work.noindex; EV="$DIR/evidence/probe-$LABEL"
mkdir -p "$EV" "$WORK"
log() { echo "[$(date +%H:%M:%S)] $*"; }
BNAME=$(basename "$(dirname "$APP")")
PREFIX="$ROOT/$WORK/prefix-$BNAME"; mkdir -p "$PREFIX"; ln -sfn "$APP/Contents/Frameworks" "$PREFIX/lib"
CLONE=$WORK/probe.raw
GICD=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
rm -f perf/venus-regression-2026-10-15/work.noindex/probe.raw
cp -c Fedora-Workstation-44.metal.test.raw "$CLONE"
env "$@" VIRGLRS_RING_FENCE_STATS=1 LIMINA_FIRMWARE="$APP/Contents/Resources/KRUN_EFI.gop.fd" \
  LIMINA_KK_ICD="$APP/Contents/Resources/vulkan/kosmickrisp_icd.json" MESA_PREFIX="$PREFIX" \
  LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="--display-resolution 1280x800" \
  RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info \
  LIMINA_DISK="$CLONE" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot.txt" 2>&1 &
BPID=$!
WL=/tmp/limina-worker-probe.log
PORT=$(scripts/wait-guest-ssh.sh "$WL" 400 "$BPID") || { log "ABORT: no ssh"; exit 3; }
SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)
log "up on $PORT; worker log $WL"
timeout 300 "${SSH[@]}" 'sudo systemctl isolate graphical.target; for i in $(seq 1 60); do pgrep -x gnome-shell >/dev/null && break; sleep 2; done; sleep 60; rpm -q mesa-vulkan-drivers gcc mesa-libEGL-devel mesa-libGLES-devel' > "$EV/guest.txt" 2>&1
cat "$EV/guest.txt"
# Pin the display to 1280x800 @ 1.0, 60 Hz by a session restart, as point.sh does: perf-ledger
# refuses an unpinned guest, and vkmark's score depends on the mode too.
BUS='export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus'
scp -P "$PORT" "${SSHO[@]}" -q scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/
timeout 300 "${SSH[@]}" "$BUS; chmod +x /tmp/set-guest-display.py
  /tmp/set-guest-display.py --write-config 1280x800 1.0 60
  sudo systemctl isolate multi-user.target; sleep 5; sudo systemctl isolate graphical.target
  for i in \$(seq 1 60); do sleep 5; /tmp/set-guest-display.py --show >/dev/null 2>&1 && break; done
  /tmp/set-guest-display.py --verify 1280x800 1.0 60" > "$EV/pin.txt" 2>&1
log "display: $(tail -n 1 "$EV/pin.txt")"
mark() { echo "[probe] ---- $1 $(date -u +%H:%M:%S)" >> "$EV/marks.txt"; }
mark vkmark
timeout 300 "${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 VK_DRIVER_FILES=$GICD; vkmark -s 1280x720" > "$EV/vkmark.txt" 2>&1
log "vkmark: $(sed -n 's/.*vkmark Score: \([0-9]*\).*/\1/p' "$EV/vkmark.txt")"
# The zink half (gl-replay, glmark2) as perf-ledger runs it, rows kept with this probe.
mark perf-ledger
LIMINA_SSH_PORT="$PORT" LIMINA_PERF_LEDGER="$EV/ledger.csv" LIMINA_PERF_RATE=60 LIMINA_PERF_TRANSPORT=venus \
  timeout 900 scripts/perf-ledger.sh "probe $LABEL" > "$EV/ledger-run.txt" 2>&1
log "perf-ledger: $(grep -E '^(gl-replay|glmark2)' "$EV/ledger-run.txt" | tr '\n' ' ')"
# The test image has no EGL/GLES -devel packages: ship Mesa's Khronos headers and link the
# runtime libraries by path.
INC=/Volumes/mesa-cs/mesa/include
tar -C "$INC" -cf - EGL GLES2 GLES3 KHR | "${SSH[@]}" 'mkdir -p /tmp/khr && tar -C /tmp/khr -xf -'
if scp -P "$PORT" "${SSHO[@]}" -q "$DIR/vtxbench.c" claude@127.0.0.1:/tmp/ 2>/dev/null &&
   timeout 120 "${SSH[@]}" 'cc -O2 -I/tmp/khr -o /tmp/vtxbench /tmp/vtxbench.c /usr/lib64/libEGL.so.1 /usr/lib64/libGLESv2.so.2' > "$EV/vtx-build.txt" 2>&1; then
  mark vtxbench
  timeout 300 "${SSH[@]}" "MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=$GICD /tmp/vtxbench 200" > "$EV/vtxbench.txt" 2>&1
  cat "$EV/vtxbench.txt"
else
  log "no vtxbench: $(tail -n 2 "$EV/vtx-build.txt" 2>/dev/null)"
fi
timeout 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
for _ in $(seq 1 60); do kill -0 "$BPID" 2>/dev/null || break; sleep 3; done
kill -0 "$BPID" 2>/dev/null && log "poweroff timed out, VM still up (pid $BPID)"
cp "$WL" "$EV/worker.log"
grep -E 'ring fences:|MEASUREMENT SEAM|fences are ordered' "$EV/worker.log" > "$EV/fence-stats.txt"
cat "$EV/fence-stats.txt"
rm -f perf/venus-regression-2026-10-15/work.noindex/probe.raw
