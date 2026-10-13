#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
# Build limina's guest GPU vendor drivers, once, for every distro: KosmicKrisp on the limina
# bridge (Vulkan) and zink over it as a glvnd EGL vendor plus a GBM backend, from the
# [kosmickrisp-guest] Mesa with -Dlimina-vendor=true (docs/design/metal-transport-delivery.md §1).
#
# Everything lands under /opt/limina/<wire version>/, the protocol the drivers speak to the host,
# so drivers for several hosts can sit side by side and each declines a host it does not speak
# to. Nothing is installed into the system's own paths here: the installer links the vendor
# files into them.
#
# Portability comes from the build base (guest/gpu-drivers/Containerfile: the oldest glibc we
# support) and from linking the C++ runtime statically. The build refuses an artifact that needs a
# newer glibc than the base's, or a library outside the short list every desktop distro has.
#
# Usage: scripts/build-gpu-drivers.sh
#   MESA_TREE  the [kosmickrisp-guest] checkout (default: its manifest `tree`)
#   VOLUME     container volume holding the build tree across runs (default limina-gpu-drivers-build)
#   RECONFIGURE=1  wipe the build tree's meson configuration first
# Output: target/gpu-drivers/opt/limina/<v>/ and target/gpu-drivers/limina-gpu-drivers-<v>.tar.gz
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=scripts/lib/manifest.sh
. "$ROOT/scripts/lib/manifest.sh"
TREE="${MESA_TREE:-$(pin_tree kosmickrisp-guest)}"
VOLUME="${VOLUME:-limina-gpu-drivers-build}"
IMAGE="limina-gpu-drivers-build:ubuntu24.04"
OUT="$ROOT/target/gpu-drivers"

WIRE="$(sed -n 's/^#define LMN_WIRE_VERSION \([0-9]*\)u$/\1/p' \
  "$TREE/src/kosmickrisp/bridge/limina/limina_gpu_wire.h")"
[ -n "$WIRE" ] || { echo "no LMN_WIRE_VERSION in $TREE" >&2; exit 1; }
PREFIX="/opt/limina/$WIRE"

if ! container image inspect "$IMAGE" >/dev/null 2>&1; then
  container build -t "$IMAGE" -f "$ROOT/guest/gpu-drivers/Containerfile" "$ROOT/guest/gpu-drivers"
fi

rm -rf "$OUT/opt"
mkdir -p "$OUT"
echo "==> building $PREFIX from $TREE ($(git -C "$TREE" rev-parse --short HEAD))"
container run --rm --cpus "${JOBS:-8}" --memory 12g \
  --mount "type=bind,source=$TREE,target=/src,readonly" \
  -v "$VOLUME:/build" -v "$OUT:/out" "$IMAGE" bash -euo pipefail -c "
[ '${RECONFIGURE:-0}' = 1 ] && rm -rf /build/vendor
if [ ! -f /build/vendor/build.ninja ]; then
  meson setup /build/vendor /src --prefix=$PREFIX --libdir=lib \
    -Dlimina-vendor=true \
    -Dvulkan-drivers=kosmickrisp -Dkosmickrisp-bridge=limina \
    -Dgallium-drivers=zink -Dllvm=enabled -Ddraw-use-llvm=false \
    -Dplatforms=x11,wayland -Degl=enabled -Dgbm=enabled -Dglx=disabled \
    -Dopengl=true -Dgles1=disabled -Dgles2=enabled \
    -Dglvnd=enabled -Dglvnd-vendor-name=limina \
    -Dbuildtype=debugoptimized -Db_ndebug=true \
    -Dcpp_link_args=-static-libstdc++ \
    -Dlmsensors=disabled -Dlibunwind=disabled -Dvalgrind=disabled \
    -Dvideo-codecs= -Dtools= -Dvulkan-layers= -Dbuild-tests=false \
    > /build/meson.log 2>&1 || { tail -n 40 /build/meson.log; exit 1; }
fi
ninja -C /build/vendor > /build/ninja.log 2>&1 || {
  grep -E '(error|FAILED)' /build/ninja.log | head -n 40; exit 1; }
rm -rf /tmp/stage
DESTDIR=/tmp/stage ninja -C /build/vendor install > /build/install.log 2>&1
cd /tmp/stage$PREFIX
# What only a system Mesa's own users need: GBM stays the system's (one libgbm per process, which
# loads our backend), the dri/ stubs serve an X server's GLX loader, and nothing builds against us.
rm -rf include lib/pkgconfig lib/dri lib/libgbm.so*
# The private libgallium sits beside the libraries that need it.
patchelf --set-rpath '\$ORIGIN' lib/libEGL_limina.so.0.0.0
patchelf --set-rpath '\$ORIGIN/..' lib/gbm/dri_gbm.so
cp -a /tmp/stage/opt /out/
"

D="$OUT/opt/limina/$WIRE"
echo "==> checking $D"
container run --rm -v "$OUT:/out" "$IMAGE" bash -euo pipefail -c "
cd /out/opt/limina/$WIRE
# What a vendor tree may need from the system: glibc, and the window-system and DRM libraries
# every desktop distro ships with a stable soname. Anything else is a library some distro lacks.
allowed='^(libc|libm|libdl|libpthread|librt|libgcc_s|ld-linux-aarch64)\\.so|^lib(drm|expat|z|zstd|udev|wayland-client|wayland-server|xcb[a-z0-9-]*|X11|X11-xcb|Xext|Xfixes|Xrandr|Xxf86vm|xshmfence|gbm|vulkan|GLdispatch|EGL)\\.so|^libgallium-.*\\.so\$'
bad=0
for f in \$(find . -name '*.so*' -type f); do
  glibc=\$(objdump -T \$f | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n 1)
  if [ -n \"\$glibc\" ] && [ \"\$(printf '%s\\nGLIBC_2.39\\n' \$glibc | sort -V | tail -n 1)\" != GLIBC_2.39 ]; then
    echo \"!!! \$f needs \$glibc, newer than the base's 2.39\"; bad=1
  fi
  for n in \$(objdump -p \$f | awk '/NEEDED/ {print \$2}'); do
    echo \"\$n\" | grep -Eq \"\$allowed\" || { echo \"!!! \$f needs \$n\"; bad=1; }
  done
  echo \"    \$f  \${glibc:-no glibc}\"
done
exit \$bad
"

# The vendor files point into the tree by absolute path, so the installer can link them into the
# system's search paths unchanged.
mkdir -p "$D/share/glvnd/egl_vendor.d"
rm -f "$D/share/glvnd/egl_vendor.d/"*.json
printf '{"file_format_version":"1.0.0","ICD":{"library_path":"%s/lib/libEGL_limina.so.0"}}\n' \
  "$PREFIX" > "$D/share/glvnd/egl_vendor.d/10_limina-$WIRE.json"
echo "$WIRE" > "$D/WIRE_VERSION"
git -C "$TREE" rev-parse HEAD > "$D/MESA_REV"

tar -C "$OUT" -czf "$OUT/limina-gpu-drivers-$WIRE.tar.gz" "opt/limina/$WIRE"
echo "==> $OUT/limina-gpu-drivers-$WIRE.tar.gz"
