#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Append a dated row-set of tier-2 graphics measurements to perf/ledger.csv — the
# perf TREND LEDGER (read perf/README.md: trends for humans, NOT a pass/fail gate).
#
# Usage: scripts/perf-ledger.sh [notes]
# Needs: the seated desktop up with --net (spikes/venus-draw-probe/boot-seated-kk.sh)
# and the trace fixtures in fixtures/traces/ (regenerate via spikes/trace-replay/).
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
ZINK="LD_LIBRARY_PATH=/opt/mesa-zink/lib64 LIBGL_DRIVERS_PATH=/opt/mesa-zink/lib64/dri __EGL_VENDOR_LIBRARY_DIRS=/opt/mesa-zink/share/glvnd/egl_vendor.d MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback"

# Backend guard: refuse to record numbers from a silent llvmpipe fallback (the env trap).
env $BASE $ZINK glmark2-es2 -b build:duration=0.5 --size 128x128 2>&1 | grep -q "Virtio-GPU Venus" \
  || { echo "GUARD_FAIL: GL stack is not on venus" >&2; exit 1; }

gl_venus=$(env $BASE $ZINK eglretrace --headless --benchmark /tmp/glmark2-build.trace 2>&1 \
  | sed -n "s/.*average of \([0-9.]*\) fps.*/\1/p")
gl_lvp=$(env $BASE GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=1 eglretrace --headless --benchmark /tmp/glmark2-build.trace 2>&1 \
  | sed -n "s/.*average of \([0-9.]*\) fps.*/\1/p")
vk_venus=$(env $BASE VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
  /opt/gfxreconstruct/bin/gfxrecon-replay --wsi headless /tmp/vkcube.gfxr 2>&1 \
  | sed -n "s/.*Replay FPS: \([0-9.]*\) fps.*/\1/p")
glmark=$(env $BASE $ZINK glmark2-es2-wayland -b build:duration=3 --size 512x512 2>&1 \
  | sed -n "s/.*glmark2 Score: \([0-9]*\).*/\1/p")

echo "gl-replay-venus,fps,$gl_venus"
echo "gl-replay-llvmpipe,fps,$gl_lvp"
echo "vk-replay-venus-headless,fps,$vk_venus"
echo "glmark2-wayland-venus,score,$glmark"
')

echo "$OUT" | while IFS= read -r line; do
  [ -n "$line" ] && echo "$DATE,$COMMIT,$line,$NOTES" >> "$LEDGER"
done
echo "appended to $LEDGER:"
tail -4 "$LEDGER"
