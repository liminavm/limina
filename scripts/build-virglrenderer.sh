#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build our host virglrenderer for the limina worker: UPSTREAM virglrenderer 1.3.0 with
# in-process venus (Vulkan→KosmicKrisp, our one supported host driver) on macOS. This replaces
# the homebrew slp
# `0.10.4e-krunkit` bottle whose vkr only knows VK_KHR_maintenance1..4 and caps the guest at
# Vulkan 1.2 — too old for zink (which needs maintenance5). Upstream 1.3.0's vkr knows
# maintenance1..9 and the krunkit macOS work is fully upstreamed, so this builds with NO
# source patches; the only krun-side delta (the macOS blob `get_map_ptr`) is handled in
# rutabaga against upstream's resource_map API. See memory limina-tier2-venus / docs/roadmap M4.
#
# Output: a prefix at third_party/virgl-prefix whose virglrenderer.pc goes first on
# PKG_CONFIG_PATH when building limina-vmm (rutabaga_gfx/build.rs probes `virglrenderer`).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

SRC="${VIRGL_SRC:-$ROOT/third_party/virglrenderer}"   # upstream 1.3.0 checkout (gitignored)
BUILD="$SRC/build"
PREFIX="${VIRGL_PREFIX:-$ROOT/third_party/virgl-prefix}"

[ -f "$SRC/meson.build" ] || { echo "virglrenderer source missing at $SRC" >&2; exit 1; }

# EGL platform needs an epoxy WITH egl support (Homebrew's is CGL-only). Build it first.
EPOXY_PREFIX="$ROOT/third_party/epoxy-egl-prefix"
grep -qi epoxy_has_egl=1 "$EPOXY_PREFIX/lib/pkgconfig/epoxy.pc" 2>/dev/null || {
  echo "epoxy-with-EGL missing at $EPOXY_PREFIX — run spikes/virgl-zink-kk/build-epoxy-egl.sh first" >&2
  exit 1
}
# epoxy.pc has `Requires.private: egl` → pkg-config must also see the zink-on-KK Mesa's egl.pc,
# else "Could not generate cflags for epoxy". (This is the host GL provider for vrend — see
# docs/drivers/kosmickrisp.rst / spikes/virgl-zink-kk.)
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
[ -f "$MESA_PREFIX/lib/pkgconfig/egl.pc" ] || {
  echo "zink-on-KK Mesa egl.pc missing at $MESA_PREFIX — build it (spikes/virgl-zink-kk/build-mesa-zink-kk.sh)" >&2
  exit 1
}

# venus on darwin pulls in Metal/Foundation frameworks + dynamic Vulkan load (vulkan-dload);
# molten-vk on PKG_CONFIG_PATH is harmless and lets the loader find the ICD at runtime.
# Our epoxy-egl + zink-kk Mesa go FIRST so virglrenderer's EGL platform sees epoxy_has_egl=1.
export PKG_CONFIG_PATH="$EPOXY_PREFIX/lib/pkgconfig:$MESA_PREFIX/lib/pkgconfig:$(brew --prefix)/opt/molten-vk/lib/pkgconfig:$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

# thread render-server mode = the krunkit in-process model (ENABLE_SAME_PROCESS_RENDER_SERVER).
# platforms=egl = venus (Vulkan→KK) AND vrend (GL via zink-on-KK) — the coexist device. On Darwin
# virglrenderer enables EGL without GBM and uses the surfaceless virgl_egl_init path (needs the
# patch wiring it: spikes/virgl-zink-kk/patches/virglrenderer-vrend-egl-no-gbm-macos.patch).
# vulkan-dload=false: LINK the Vulkan lib at build time (absolute install_name) instead of
# bare-name dlopen("libvulkan.dylib") at runtime. The worker is codesigned (hardened runtime
# strips DYLD_*), and /opt/homebrew/lib isn't on the default dyld search path, so a runtime
# bare dlopen finds nothing and venus enumerates zero GPUs. Linking it (like the slp bottle
# did with MoltenVK) makes the worker load it by recorded absolute path. See limina-tier2-venus.
MESON_ARGS=(
  -Dvenus=true
  -Dvulkan-dload=false
  -Drender-server-mode=thread
  -Drender-server-worker=thread
  -Dplatforms=egl
  --prefix "$PREFIX"
  --buildtype release
)

if [ -d "$BUILD" ]; then
  meson setup --reconfigure "$BUILD" "$SRC" "${MESON_ARGS[@]}"
else
  meson setup "$BUILD" "$SRC" "${MESON_ARGS[@]}"
fi
ninja -C "$BUILD"
meson install -C "$BUILD"

echo "==> installed virglrenderer 1.3.0 (venus + vrend/EGL, in-process) to $PREFIX"
echo "    pkg-config: $PREFIX/lib/pkgconfig/virglrenderer.pc"
ls -la "$PREFIX"/lib/libvirglrenderer*.dylib 2>/dev/null || true
