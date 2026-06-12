#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Native-Vulkan trace capture (phase 2 of the trace-replay plan): capture vkcube on
# venus with the gfxreconstruct layer in the seated guest, prove replay on both venus
# and lavapipe, and pull the .gfxr into fixtures/traces/ (the venus_vk_replay fixture).
#
# Usage: spikes/trace-replay/capture-replay-vk.sh [frames]   (default: 200)
# Needs: the seated desktop up with --net (boot-seated-kk.sh) and /opt/gfxreconstruct
# in the guest (baked into dev-enh since 2026-06-12; rebuild via
# scripts/build-gfxreconstruct.sh).
#
# Notes from the prototyping run (2026-06-12):
# - Capture: VK_LAYER_PATH + VK_INSTANCE_LAYERS=VK_LAYER_LUNARG_gfxreconstruct +
#   GFXRECON_CAPTURE_FILE; GFXRECON_CAPTURE_FILE_TIMESTAMP=false keeps the name stable.
# - The lavapipe replay needs --remove-unsupported: the venus capture records instance
#   extensions lavapipe lacks and vkCreateInstance hard-fails the replay otherwise.
#   The venus leg stays strict (same driver as capture).
set -euo pipefail
FRAMES="${1:-200}"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)

SSH=(ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1)

"${SSH[@]}" "
set -e
XAUTH=\$(ls /run/user/1000/.mutter-Xwaylandauth.* 2>/dev/null | head -1)
BASE=\"XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0 XAUTHORITY=\$XAUTH VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1\"
VENUS=\"VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json\"
LVP=\"VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json\"
GFXR=/opt/gfxreconstruct

mkdir -p ~/traces
echo '== capture vkcube ($FRAMES frames) on venus:'
env \$BASE \$VENUS VK_LAYER_PATH=\$GFXR/share/vulkan/explicit_layer.d \
    LD_LIBRARY_PATH=\$GFXR/lib64 VK_INSTANCE_LAYERS=VK_LAYER_LUNARG_gfxreconstruct \
    GFXRECON_CAPTURE_FILE=~/traces/vkcube.gfxr GFXRECON_CAPTURE_FILE_TIMESTAMP=false \
    vkcube --c $FRAMES 2>&1 | grep -E 'Selected GPU' || true

echo '== replay on venus:'
env \$BASE \$VENUS \$GFXR/bin/gfxrecon-replay ~/traces/vkcube.gfxr 2>&1 | tail -1
echo '== replay on lavapipe (--remove-unsupported):'
env \$BASE \$LVP \$GFXR/bin/gfxrecon-replay --remove-unsupported ~/traces/vkcube.gfxr 2>&1 | tail -1
"

mkdir -p "$ROOT/fixtures/traces"
scp -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "claude@127.0.0.1:traces/vkcube.gfxr" "$ROOT/fixtures/traces/"
echo "fixture updated: fixtures/traces/vkcube.gfxr"
