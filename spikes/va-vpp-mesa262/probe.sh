#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Run the VA post-processing oracle of l2_video_vaapi (a BGRA image scaled by scale_vaapi) on a
# CoW clone of a stock image, with the host's GPU logging on, and keep the worker log.
#
#   spikes/va-vpp-mesa262/probe.sh <image.raw> <out-dir> [extra guest env]
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

SRC="$1"
OUT="$2"
GUEST_ENV="${3:-}"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
mkdir -p "$OUT"
DISK="$OUT/disk.raw"
LOG="$OUT/worker.log"

BOOT=""
cleanup() {
  if [ -n "$BOOT" ] && kill -0 "$BOOT" 2>/dev/null; then
    kill "$BOOT" 2>/dev/null || true
    wait "$BOOT" 2>/dev/null || true
  fi
  rm -f "$DISK"
}
trap cleanup EXIT

cp -c "$SRC" "$DISK"
RUST_LOG="warn,limina=info,krun::vmm=info,krun_devices=info,virglrs=info" \
  LIMINA_DISK="$DISK" LIMINA_BOOT_LOG="$LOG" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh \
  >"$OUT/boot.log" 2>&1 &
BOOT=$!
PORT="$(scripts/wait-guest-ssh.sh "$LOG" 300 "$BOOT")"
g() { ssh -p "$PORT" "${SSH_OPTS[@]}" claude@127.0.0.1 "$@"; }

g "rpm -q mesa-dri-drivers mesa-va-drivers-freeworld" >"$OUT/guest.txt" 2>&1 || true
echo "---- worker log mark: before vpp ----" >>"$LOG"
g "export LIBVA_DRIVER_NAME=virtio_gpu LIBVA_MESSAGING_LEVEL=2 $GUEST_ENV; set -x
   ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=size=320x240:rate=1:duration=1 \
     -frames:v 1 -pix_fmt bgra -f rawvideo /tmp/src.bgra
   ffmpeg -hide_banner -loglevel verbose -y -vaapi_device /dev/dri/renderD128 \
     -f rawvideo -pix_fmt bgra -s 320x240 -i /tmp/src.bgra \
     -vf 'format=bgra,hwupload,scale_vaapi=w=160:h=120:format=bgra,hwdownload,format=bgra' \
     -frames:v 1 -f rawvideo /tmp/dst.bgra
   echo bgra_distinct=\$(od -An -tu1 -v < /tmp/dst.bgra | tr ' ' '\n' | grep -v '^\$' | sort -u | wc -l)
   od -An -tx1 -N32 /tmp/dst.bgra" >>"$OUT/guest.txt" 2>&1 || true
echo "---- worker log mark: after vpp ----" >>"$LOG"

g 'sudo systemctl poweroff -i' >/dev/null 2>&1 || true
wait "$BOOT" 2>/dev/null || true
BOOT=""
