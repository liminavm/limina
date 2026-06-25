#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Append a dated row-set of enhanced-tier (venus) graphics measurements to perf/ledger.csv —
# the perf TREND LEDGER (read perf/README.md: trends for humans, NOT a pass/fail gate).
#
# Usage: scripts/perf-ledger.sh [notes]
# Needs: a seated enhanced desktop up with --net. Works on BOTH enhanced images and adapts itself:
#   - RPM-enhanced (Fedora-Workstation-43.enhanced.raw): mesa at /usr, zink via environment.d
#   - dev-enh (Fedora-Workstation-43.dev-enh.raw):        mesa at /opt/mesa-zink
# and the trace fixtures in fixtures/traces/ (regenerate via spikes/trace-replay/).
# apitrace (eglretrace, gl-replay) is dnf-installed if missing. gfxrecon-replay (vk-replay) is NOT
# packaged — build it once in the guest (~/gfxreconstruct) or use dev-enh's /opt/gfxreconstruct;
# if absent the vk-replay row is simply skipped (the other three still record).
#
# One-time gfxrecon-replay build on the RPM image (deps the minimal guest lacks, learned 2026-06-25):
#   sudo dnf install -y gcc-c++ cmake ninja-build lz4-devel libzstd-devel zlib-devel \
#                       xcb-util-keysyms-devel xcb-util-devel vulkan-headers vulkan-loader-devel
#   git clone --depth 1 --recurse-submodules https://github.com/LunarG/gfxreconstruct
#   cmake -B gfxreconstruct/build -G Ninja -S gfxreconstruct -DCMAKE_BUILD_TYPE=Release \
#         -DGFXRECON_AGENT_BUILD=OFF -DBUILD_WERROR=OFF -DLZ4_OPTIONAL=ON -DZSTD_OPTIONAL=ON
#   cmake --build gfxreconstruct/build --target gfxrecon-replay -j2   # -j2: -j4 OOMs the 4 GiB guest
set -euo pipefail
NOTES="${1:-}"
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"

LEDGER=perf/ledger.csv
DATE=$(date +%Y-%m-%d)
COMMIT=$(git rev-parse --short HEAD)
[ -f "$LEDGER" ] || echo "date,commit,workload,metric,value,notes" > "$LEDGER"

SSH=(ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1)
SCP=(scp -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -q)

"${SCP[@]}" fixtures/traces/glmark2-build.trace fixtures/traces/vkcube.gfxr claude@127.0.0.1:/tmp/

OUT=$("${SSH[@]}" '
set -e
XAUTH=$(ls /run/user/1000/.mutter-Xwaylandauth.* | head -1)
BASE="XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0 XAUTHORITY=$XAUTH VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1"
# Image-adaptive zink env. The source-built dev-enh image keeps mesa under /opt/mesa-zink and needs
# the loader paths spelled out; the RPM-enhanced image (Fedora-Workstation-43.enhanced.raw) installs
# mesa at /usr and ships the zink selection in /etc/environment.d, so only the driver-override knobs
# are needed. Detect by the presence of /opt/mesa-zink.
if [ -d /opt/mesa-zink ]; then
  ZINK="LD_LIBRARY_PATH=/opt/mesa-zink/lib64 LIBGL_DRIVERS_PATH=/opt/mesa-zink/lib64/dri __EGL_VENDOR_LIBRARY_DIRS=/opt/mesa-zink/share/glvnd/egl_vendor.d MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback"
else
  ZINK="MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json VN_PERF=no_fence_feedback"
fi
# eglretrace ships in apitrace (dnf-installable on the RPM image; baked on dev-enh). gfxrecon-replay
# is not packaged — use a local build (~/gfxreconstruct) or the dev-enh /opt; skip vk-replay if absent.
command -v eglretrace >/dev/null || sudo dnf install -y -q apitrace >/dev/null 2>&1 || true
GFXR=$(command -v gfxrecon-replay 2>/dev/null || ls /opt/gfxreconstruct/bin/gfxrecon-replay ~/gfxreconstruct/build/tools/replay/gfxrecon-replay 2>/dev/null | head -1)

# Backend guard: refuse to record numbers from a silent llvmpipe fallback (the env trap).
env $BASE $ZINK glmark2-es2 -b build:duration=0.5 --size 128x128 2>&1 | grep -q "Virtio-GPU Venus" \
  || { echo "GUARD_FAIL: GL stack is not on venus" >&2; exit 1; }

gl_venus=$(env $BASE $ZINK eglretrace --headless --benchmark /tmp/glmark2-build.trace 2>&1 \
  | sed -n "s/.*average of \([0-9.]*\) fps.*/\1/p")
# The RPM image forces MESA_LOADER_DRIVER_OVERRIDE=zink globally (environment.d), so the llvmpipe
# control leg must explicitly UNSET it (and the venus ICD) or zink hijacks the CPU run and fails.
gl_lvp=$(env -u MESA_LOADER_DRIVER_OVERRIDE -u VK_DRIVER_FILES $BASE GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=1 eglretrace --headless --benchmark /tmp/glmark2-build.trace 2>&1 \
  | sed -n "s/.*average of \([0-9.]*\) fps.*/\1/p")
vk_venus=""
[ -n "$GFXR" ] && vk_venus=$(env $BASE VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
  "$GFXR" --wsi headless /tmp/vkcube.gfxr 2>&1 \
  | sed -n "s/.*\(Measured\|Replay\) FPS: \([0-9.]*\) fps.*/\2/p")
glmark=$(env $BASE $ZINK glmark2-es2-wayland -b build:duration=3 --size 512x512 2>&1 \
  | sed -n "s/.*glmark2 Score: \([0-9]*\).*/\1/p")

echo "gl-replay-venus,fps,$gl_venus"
echo "gl-replay-llvmpipe,fps,$gl_lvp"
[ -n "$vk_venus" ] && echo "vk-replay-venus-headless,fps,$vk_venus"
echo "glmark2-wayland-venus,score,$glmark"
')

echo "$OUT" | while IFS= read -r line; do
  [ -n "$line" ] && echo "$DATE,$COMMIT,$line,$NOTES" >> "$LEDGER"
done
echo "appended to $LEDGER:"
tail -4 "$LEDGER"
