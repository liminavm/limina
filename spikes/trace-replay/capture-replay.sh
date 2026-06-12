#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Trace-replay spike driver: capture a glmark2 scene on zink->venus in the seated guest
# and replay it on both backends. See RESULTS.md for what works and what's blocked.
#
# Usage: spikes/trace-replay/capture-replay.sh [scene]   (default: build)
# Needs: the seated desktop up with --net (boot-seated-kk.sh), apitrace installed in
# the guest (sudo dnf install apitrace).
set -euo pipefail
SCENE="${1:-build}"

SSH=(ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1)

"${SSH[@]}" "
set -e
XAUTH=\$(ls /run/user/1000/.mutter-Xwaylandauth.* 2>/dev/null | head -1)
BASE=\"XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0 XAUTHORITY=\$XAUTH\"
# The session GL env (zink->venus) — SSH shells do NOT inherit environment.d, and
# without this the GL stack silently falls back to llvmpipe (virgl ctx fails with
# EINVAL; VERIFY on the host: worker log must show 'CTX_CREATE ... init=0x4').
ZINK=\"LD_LIBRARY_PATH=/opt/mesa-zink/lib64 LIBGL_DRIVERS_PATH=/opt/mesa-zink/lib64/dri __EGL_VENDOR_LIBRARY_DIRS=/opt/mesa-zink/share/glvnd/egl_vendor.d MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1\"

mkdir -p ~/traces
echo '== capture ($SCENE) on zink->venus (Wayland platform — works):'
env \$BASE \$ZINK apitrace trace --api egl --output ~/traces/glmark2-$SCENE.trace \
    glmark2-es2-wayland -b $SCENE:duration=2 --size 512x512 2>&1 | grep -E 'GL_RENDERER|Score'

echo '== replay on llvmpipe (works):'
env \$BASE GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=1 eglretrace --headless --benchmark ~/traces/glmark2-$SCENE.trace 2>&1 | tail -1

echo '== replay on zink->venus (CRASHES — the X11 kopper null-deref, see RESULTS.md):'
env \$BASE \$ZINK eglretrace --headless --benchmark ~/traces/glmark2-$SCENE.trace 2>&1 | tail -1 || true
"
