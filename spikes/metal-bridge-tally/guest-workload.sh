#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Guest half of run-tally.sh: run one workload for a fixed time in the seated session's env.
# Usage (in the guest): guest-workload.sh <workload> <seconds> | guest-workload.sh provenance
# Every GL leg prints its GL_RENDERER, so a leg that silently landed on the wrong path is visible.
set -uo pipefail
W="$1"; D="${2:-30}"

XAUTH=$(ls /run/user/1000/.mutter-Xwaylandauth.* 2>/dev/null | head -1)
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0 XAUTHORITY="$XAUTH"
export VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1
VENUS_ICD=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
ZINK=(env MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=$VENUS_ICD)
# vrend: virgl's gallium driver, whatever environment.d says.
VREND=(env -u MESA_LOADER_DRIVER_OVERRIDE -u GALLIUM_DRIVER MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu)
GFXR=$(command -v gfxrecon-replay 2>/dev/null || ls /opt/gfxreconstruct/bin/gfxrecon-replay 2>/dev/null | head -1)

loop() {  # loop <cmd...>: rerun until D seconds have passed; print each run's output
  local end=$((SECONDS + D)) n=0
  while [ $SECONDS -lt $end ]; do "$@"; n=$((n + 1)); done
  echo "RUNS: $n in ${D}s"
}

case "$W" in
provenance)
  echo "kernel=$(uname -r) pagesize=$(getconf PAGESIZE) mesa=$(rpm -q --qf '%{VERSION}-%{RELEASE}' mesa-dri-drivers) vcpus=$(nproc)"
  cat /etc/environment.d/*.conf 2>/dev/null
  echo "gfxr=$GFXR"; command -v vkmark glmark2-es2 eglretrace gcc glslangValidator
  ;;
idle)
  sleep "$D"
  ;;
vk-replay)
  loop env VK_DRIVER_FILES=$VENUS_ICD "$GFXR" --wsi headless /tmp/vkcube.gfxr
  ;;
vkmark)
  # Steady state: one process, scenes back to back (vk-replay restarts a process every ~0.1 s,
  # so it measures device and pipeline churn rather than frames).
  per=$(awk -v d="$D" 'BEGIN { printf "%.0f", d / 4 }')
  env VK_DRIVER_FILES=$VENUS_ICD vkmark --winsys headless --size 1280x720 \
    -b vertex:duration=$per -b texture:duration=$per -b shading:shading=phong:duration=$per \
    -b cube:duration=$per 2>&1
  ;;
drawstorm)
  if [ ! -x /tmp/drawstorm ]; then
    # vert.spv/frag.spv come compiled from the host: the image has no glslang.
    (cd /tmp && gcc -O2 -o drawstorm drawstorm.c -lvulkan) || exit 1
  fi
  # ~3 ms/frame on venus at n=1000: D seconds is roughly D*300 frames.
  cd /tmp && env VK_DRIVER_FILES=$VENUS_ICD ./drawstorm -n 1000 -f $((D * 250))
  ;;
gl-replay-venus)
  "${ZINK[@]}" glmark2-es2 -b build:duration=0.2 --size 64x64 2>&1 | grep -m1 'GL_RENDERER'
  loop "${ZINK[@]}" eglretrace --headless --benchmark /tmp/glmark2-build.trace
  ;;
gl-replay-vrend)
  "${VREND[@]}" glmark2-es2 -b build:duration=0.2 --size 64x64 2>&1 | grep -m1 'GL_RENDERER'
  loop "${VREND[@]}" eglretrace --headless --benchmark /tmp/glmark2-build.trace
  ;;
glmark2-venus | glmark2-vrend)
  if [ "$W" = glmark2-venus ]; then E=("${ZINK[@]}"); else E=("${VREND[@]}"); fi
  per=$(awk -v d="$D" 'BEGIN { printf "%.1f", d / 4 }')
  "${E[@]}" glmark2-es2 --off-screen --size 1280x720 \
    -b build:duration=$per -b texture:duration=$per -b shading:shading=phong:duration=$per \
    -b refract:duration=$per 2>&1
  ;;
*)
  echo "unknown workload $W" >&2; exit 2
  ;;
esac
