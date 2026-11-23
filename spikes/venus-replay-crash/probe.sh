#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# One arm of the distro-venus vk-replay crash bisect: boot a clone of <image> on a host stack, replay
# fixtures/traces/vkcube.gfxr headless on the guest's DISTRO venus ICD (and limina's, if the image
# has it) twice each, and record exit codes, the tail of the replay output and the guest versions.
#
# Usage: spikes/venus-replay-crash/probe.sh <label> <image.raw> [<mounted Limina.app>]
#   With no app, the host is this tree's target/debug build, as the suite runs it.
set -uo pipefail
LABEL="${1:?label}"; IMG="${2:?image}"; APP="${3:-}"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel); cd "$ROOT"
DIR=spikes/venus-replay-crash; WORK=$DIR/work.noindex; EV=$DIR/evidence/$LABEL
mkdir -p "$WORK" "$EV"
CLONE="$ROOT/$WORK/$LABEL.raw"; WLOG="/tmp/limina-worker-$LABEL.log"
log() { echo "[$(date +%H:%M:%S)] $*"; }
ARM_ENV=(LIMINA_KK_ICD=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
         MESA_PREFIX=/Volumes/mesa-cs/zink-kk-prefix)
if [ -n "$APP" ]; then
  [ -x "$APP/Contents/MacOS/limina-vmm" ] || { echo "no worker in $APP" >&2; exit 2; }
  FWF=$(ls "$APP/Contents/Resources/efi.fd" "$APP/Contents/Resources/KRUN_EFI.gop.fd" 2>/dev/null | head -n 1)
  PREFIX="$ROOT/$WORK/prefix-$LABEL"; mkdir -p "$PREFIX"; ln -sfn "$APP/Contents/Frameworks" "$PREFIX/lib"
  ARM_ENV=(LIMINA_BIN="$APP/Contents/MacOS/limina" LIMINA_VMM_BIN="$APP/Contents/MacOS/limina-vmm"
           LIMINA_FIRMWARE="$FWF" LIMINA_KK_ICD="$APP/Contents/Resources/vulkan/kosmickrisp_icd.json"
           MESA_PREFIX="$PREFIX")
fi
{ echo "label=$LABEL image=$IMG app=${APP:-tree $(git rev-parse --short HEAD)}"; printf '%s\n' "${ARM_ENV[@]}"; } > "$EV/arm.txt"
SSHO=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

kill_by_disk() {
  for p in $(pgrep -f "[l]imina.*--disk $CLONE"); do
    log "killing our own VM: $(ps -ww -o pid,lstart,command -p "$p" | tail -n 1)"; kill "$p"
  done
  for _ in $(seq 1 20); do pgrep -f "[l]imina.*--disk $CLONE" >/dev/null || return 0; sleep 1; done
  for p in $(pgrep -f "[l]imina.*--disk $CLONE"); do kill -9 "$p"; done
}
finish() {
  [ -n "${PORT:-}" ] && timeout 30 ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1 'sudo systemctl poweroff' >/dev/null 2>&1
  for _ in $(seq 1 40); do pgrep -f "[l]imina.*--disk $CLONE" >/dev/null || break; sleep 3; done
  kill_by_disk; cp "$WLOG" "$EV/worker.log" 2>/dev/null; rm -f "$CLONE"
}
trap finish EXIT

rm -f "$CLONE"; cp -c "$IMG" "$CLONE" || exit 3
log "boot $LABEL"
env "${ARM_ENV[@]}" LIMINA_CPUS=4 LIMINA_RAM_MIB=4096 LIMINA_NET=1 LIMINA_BOOT_LOG="$WLOG" \
  RUST_LOG=warn,limina=info,krun::vmm=info,krun_devices=info LIMINA_DISK="$CLONE" \
  spikes/venus-draw-probe/boot-enhanced-efi-kk.sh > "$EV/boot.txt" 2>&1 &
BPID=$!
PORT=$(scripts/wait-guest-ssh.sh "$WLOG" 400 "$BPID") || { log "ABORT: no ssh"; exit 4; }
log "ssh on $PORT"
scp -P "$PORT" "${SSHO[@]}" -q fixtures/traces/vkcube.gfxr claude@127.0.0.1:/tmp/ || exit 5
# EXTRACT=<host path>: keep this guest's distro venus driver there.
[ -n "${EXTRACT:-}" ] && scp -P "$PORT" "${SSHO[@]}" -q claude@127.0.0.1:/usr/lib64/libvulkan_virtio.so "$EXTRACT"
# SWAP_LIB=<host path>: also replay on that venus driver, through an ICD of its own (/tmp/swap/icd.json).
if [ -n "${SWAP_LIB:-}" ]; then
  ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1 'mkdir -p /tmp/swap' &&
  scp -P "$PORT" "${SSHO[@]}" -q "$SWAP_LIB" claude@127.0.0.1:/tmp/swap/libvulkan_virtio.so &&
  ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1 'sed "s|\"library_path\": *\"[^\"]*\"|\"library_path\": \"/tmp/swap/libvulkan_virtio.so\"|" /usr/share/vulkan/icd.d/virtio_icd.aarch64.json > /tmp/swap/icd.json; cat /tmp/swap/icd.json' > "$EV/swap.txt" 2>&1
fi
timeout 900 ssh -p "$PORT" "${SSHO[@]}" claude@127.0.0.1 '
  echo "kernel=$(uname -r) module=$(modinfo -n virtio_gpu)"
  rpm -q mesa-vulkan-drivers mesa-libgbm libdrm
  GFXR=$(command -v gfxrecon-replay || ls /opt/gfxreconstruct/bin/gfxrecon-replay 2>/dev/null)
  echo "gfxr=${GFXR:-NONE}"; [ -n "$GFXR" ] || exit 0
  sudo systemctl isolate graphical.target; sleep 30
  for icd in /usr/share/vulkan/icd.d/virtio_icd.aarch64.json /tmp/swap/icd.json /etc/vulkan/icd.d/limina-venus.json; do
    [ -f "$icd" ] || continue
    for n in 1 2; do
      XDG_RUNTIME_DIR=/run/user/1000 VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1 VK_DRIVER_FILES=$icd \
        timeout 200 "$GFXR" --wsi headless /tmp/vkcube.gfxr > /tmp/r.txt 2>&1
      rc=$?
      echo "RESULT icd=$icd run=$n rc=$rc $(grep -E "FPS" /tmp/r.txt | tail -n 1)"
      tail -n 4 /tmp/r.txt | sed "s/^/  | /"
    done
  done
  journalctl -k -b --no-pager | grep -iE "segfault|virtio_gpu|virtgpu" | tail -n 10 | sed "s/^/  k| /"' \
  > "$EV/replay.txt" 2>&1
log "$LABEL: $(grep -E '^(kernel|mesa-vulkan|gfxr|RESULT)' "$EV/replay.txt" | tr '\n' ' ')"
