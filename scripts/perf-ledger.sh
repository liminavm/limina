#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Append a dated row-set of enhanced-tier (venus) graphics measurements to perf/ledger.csv —
# the perf TREND LEDGER (read perf/README.md: trends for humans, NOT a pass/fail gate).
#
# Usage: LIMINA_SSH_PORT=<port> scripts/perf-ledger.sh [notes]
#   LIMINA_SSH_PORT defaults to 2222, but `limina --net` AUTO-ALLOCATES from 2222 up so two VMs can
#   run at once — read the real port off the supervisor log ("guest SSH forward ready: ssh -p N …")
#   and pass it, or you will silently benchmark the wrong VM.
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

PORT="${LIMINA_SSH_PORT:-2222}"
SSH=(ssh -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR claude@127.0.0.1)
SCP=(scp -P "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -q)

"${SCP[@]}" fixtures/traces/glmark2-build.trace fixtures/traces/vkcube.gfxr \
            scripts/perf/set-guest-display.py claude@127.0.0.1:/tmp/

# VERIFY THE DISPLAY GEOMETRY before measuring — read-only, never prompts.
# `match-host` (default since 2026-07-03) drives the guest to the host screen and GNOME picks a
# fractional scale — 2560x1440 @ 2.5 on the M1 Max, which reaches wayland clients as buffer_scale 3.
# That makes glmark2's 512x512 request a wl protocol error AND a ~9x-pixels workload, so the score is
# both invalid and incomparable (2026-07-26: it produced 97 and 274 on back-to-back identical runs).
#
# We only VERIFY here, we do not apply: the live D-Bus apply pops GNOME's "Keep changes?" dialog,
# which reverts in ~20 s if nobody clicks — it will hang or, worse, silently un-pin the display
# mid-measurement. Pin it dialog-free instead, BEFORE the run:
#     limina --display-resolution 1280x800 …        # supervisor drives the mode, no match-host
#     ssh guest '/tmp/set-guest-display.py --write-config 1280x800 1.0' && reboot the guest
# LIMINA_PERF_MODE/_SCALE override the expectation; the defaults reproduce the pre-2026-07-03
# measurement geometry, which is what the historical ledger rows were taken at.
PERF_MODE="${LIMINA_PERF_MODE:-1280x800}"
PERF_SCALE="${LIMINA_PERF_SCALE:-1.0}"
"${SSH[@]}" "export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
              chmod +x /tmp/set-guest-display.py
              /tmp/set-guest-display.py --verify $PERF_MODE $PERF_SCALE" >&2 || {
  echo "ABORT: guest display is not pinned to $PERF_MODE @ $PERF_SCALE — see the note above." >&2
  exit 1
}

OUT=$("${SSH[@]}" '
set -e
XAUTH=$(ls /run/user/1000/.mutter-Xwaylandauth.* | head -1)
BASE="XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DISPLAY=:0 XAUTHORITY=$XAUTH VK_LOADER_DISABLE_DYNAMIC_LIBRARY_UNLOADING=1"
# Image-adaptive zink env. The source-built dev-enh image keeps mesa under /opt/mesa-zink and needs
# the loader paths spelled out; the RPM-enhanced image (Fedora-Workstation-43.enhanced.raw) installs
# mesa at /usr and ships the zink selection in /etc/environment.d, so only the driver-override knobs
# are needed. Detect by the presence of /opt/mesa-zink.
#
# NO VN_PERF. Both branches used to force `no_*_feedback`; it was RETIRED from the shipped guest env
# 2026-07-25 (docs/images.md) once its root cause — the 16 KiB hv_vm_map blob-coherency bug — was
# fixed (libkrun 0043 + virglrenderer 0023 + patches/linux/0004). Setting it here would benchmark a
# config we no longer ship, and it is not free: it forces every fence check onto a synchronous
# driver<->renderer round trip (25-30% of wall clock on submits carrying real work). Ledger rows
# from 2026-07-26 on are as-shipped; rows before that carried the flag.
if [ -d /opt/mesa-zink ]; then
  ZINK="LD_LIBRARY_PATH=/opt/mesa-zink/lib64 LIBGL_DRIVERS_PATH=/opt/mesa-zink/lib64/dri __EGL_VENDOR_LIBRARY_DIRS=/opt/mesa-zink/share/glvnd/egl_vendor.d MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json"
else
  ZINK="MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json"
fi
# eglretrace ships in apitrace (dnf-installable on the RPM image; baked on dev-enh). gfxrecon-replay
# is not packaged — use a local build (~/gfxreconstruct) or the dev-enh /opt; skip vk-replay if absent.
command -v eglretrace >/dev/null || sudo dnf install -y -q apitrace >/dev/null 2>&1 || true
GFXR=$(command -v gfxrecon-replay 2>/dev/null || ls /opt/gfxreconstruct/bin/gfxrecon-replay ~/gfxreconstruct/build/tools/replay/gfxrecon-replay 2>/dev/null | head -1)

# Backend guard: refuse to record numbers from a silent llvmpipe fallback (the env trap).
env $BASE $ZINK glmark2-es2 -b build:duration=0.5 --size 128x128 2>&1 | grep -q "Virtio-GPU Venus" \
  || { echo "GUARD_FAIL: GL stack is not on venus" >&2; exit 1; }

# Provenance to stderr (stdout is the row stream). Records WHICH guest produced the numbers, so the
# notes column can name real versions instead of "the enhanced image".
echo "PROVENANCE: kernel=$(uname -r) pagesize=$(getconf PAGESIZE) mesa=$(rpm -q --qf %{VERSION}-%{RELEASE} mesa-dri-drivers 2>/dev/null) vnperf=${VN_PERF:-<unset>} vcpus=$(nproc) ram=$(awk "/MemTotal/{print int(\$2/1024)\"MiB\"}" /proc/meminfo) display=$(/tmp/set-guest-display.py --show 2>/dev/null | head -1)" >&2

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
