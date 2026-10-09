#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Does a VM's guest pipeline cache survive a reboot? Boots one clone of an enhanced image twice
# with the same --gpu-cache-key file, runs the same zink-on-venus glmark2 in each, and reads
# virglrs's verdict on every guest cache's initial data out of each boot's worker log:
#   [virglrs] pipeline cache initial data: accepted (N bytes)
#   [virglrs] pipeline cache initial data: ignored (<reason>, N bytes)
# Boot 1 runs on a key the image's saved caches were never signed with, so it can only ignore
# them; boot 2 must accept what boot 1 saved. Pass NOKEY=1 to run both boots without a key (the
# control: boot 2 then ignores everything, "not signed by this key").
#
# Usage: spikes/pipeline-cache-key/two-boot.sh <enhanced.raw> <outdir>
set -uo pipefail
IMG="${1:?enhanced image}"; OUT="${2:?output dir}"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
mkdir -p "$OUT"
CLONE="$OUT/pck-clone.raw"; KEY="$OUT/pipeline-cache.key"
TAG=$(basename "$CLONE" .raw); WL="/tmp/limina-worker-$TAG.log"
rm -f "$CLONE" "$KEY"; cp -c "$IMG" "$CLONE"
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
EXTRA="--display-resolution 1280x800"; [ "${NOKEY:-}" = 1 ] || EXTRA="$EXTRA --gpu-cache-key $KEY"
GL='export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0
  for i in $(seq 1 60); do [ -S /run/user/1000/wayland-0 ] && break; sleep 5; done
  s=$(date +%s.%N)
  MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
    glmark2-es2-wayland -b shading:duration=1 -b build:duration=1 -b texture:duration=1 --size 256x256 2>&1 | grep -E "Score|Venus"
  echo "glmark2 wall: $(echo "$(date +%s.%N) - $s" | bc) s"; sync'

for boot in 1 2; do
  echo "== boot $boot"
  env LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_EXTRA_ARGS="$EXTRA" \
    RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info \
    LIMINA_DISK="$CLONE" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$OUT/boot$boot.txt" 2>&1 &
  BPID=$!
  PORT=$(scripts/wait-guest-ssh.sh "$WL" 400 "$BPID") || { echo "boot $boot: no ssh"; kill "$BPID"; exit 3; }
  SSH=(ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1)
  gtimeout --kill-after=10 300 "${SSH[@]}" "$GL" | tee "$OUT/glmark2-boot$boot.txt"
  gtimeout --kill-after=10 30 "${SSH[@]}" 'sudo systemctl poweroff' >/dev/null 2>&1
  for _ in $(seq 1 60); do kill -0 "$BPID" 2>/dev/null || break; sleep 3; done
  kill -0 "$BPID" 2>/dev/null && { echo "boot $boot: poweroff timed out"; kill "$BPID"; }
  wait "$BPID" 2>/dev/null
  cp "$WL" "$OUT/worker-boot$boot.log"
  echo "boot $boot initial data: $(grep -c 'initial data: accepted' "$OUT/worker-boot$boot.log") accepted," \
    "$(grep -c 'initial data: ignored' "$OUT/worker-boot$boot.log") ignored;" \
    "$(grep -c '\[virglrs\] refused:' "$OUT/worker-boot$boot.log") refusals"
  grep -h 'initial data: ignored' "$OUT/worker-boot$boot.log" | sed 's/.*ignored (\([^,]*\),.*/  ignored: \1/' | sort | uniq -c
done
[ -f "$KEY" ] && echo "key: $(stat -f '%Lp %z bytes' "$KEY")"
rm -f "$CLONE"
