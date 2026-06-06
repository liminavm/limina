#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Venus viability spike — does flipping the Fedora guest to the real renderer
# (rutabaga -> virglrenderer-Apple -> Venus -> MoltenVK -> Metal) init at all on this
# host, and does the guest produce a scanout through it (vs. software-2D)?
#
# Headless: --display-capture (no NSWindow) so it runs from a non-GUI session. Bounded.
# LIMINA_VIRGL_FLAGS=0x343 switches the gpu device OFF software-2D and INTO virglrenderer.
set -uo pipefail
cd "$(dirname "$0")/../.."

SECS="${SECS:-75}"
FLAGS="${FLAGS:-0x343}"   # USE_EGL|THREAD_SYNC|VENUS|ASYNC_FENCE_CB|RENDER_SERVER
IMAGE="Fedora-Workstation-43.raw"
SCRATCH="Fedora-Workstation-43.venus-spike.raw"   # *.raw gitignored
FW="target/krun-efi/KRUN_EFI.gop.fd"
OUT="spikes/venus-viability/out"
mkdir -p "$OUT"

echo "==> clone $IMAGE -> $SCRATCH (APFS cow)"
rm -f "$SCRATCH"; cp -c "$IMAGE" "$SCRATCH"

echo "==> boot Fedora headless with LIMINA_VIRGL_FLAGS=$FLAGS for ${SECS}s"
( LIMINA_VIRGL_FLAGS="$FLAGS" \
  RUST_LOG="info,krun_devices::virtio::gpu=debug" \
  target/debug/limina \
    --vmm-bin target/debug/limina-vmm \
    --firmware "$FW" --disk "$SCRATCH" \
    --cpus 4 --ram-mib 6144 \
    --display-capture "$OUT/frame.png" --display-size 1280x800 \
    --console "$OUT/console.log" \
    >"$OUT/worker.log" 2>&1 & P=$!
  sleep "$SECS"; kill -9 $P 2>/dev/null; pkill -9 -f target/debug/limina-vmm 2>/dev/null ) 
echo "==> done. artifacts in $OUT/"
rm -f "$SCRATCH"
