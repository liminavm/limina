#!/bin/bash
# Builds KosmicKrisp for a Linux guest with the limina bridge (-Dkosmickrisp-bridge=limina) in the
# limina-build:fc44 container and copies libvulkan_kosmickrisp.so to spikes/kk-on-wire/out/.
#
#   MESA_TREE  Mesa checkout on the metal-native-context branch (default /Volumes/mesa-cs/mesa-metal-guest)
#   KK_VOLUME  container volume holding the build tree across runs (default limina-guest-kk-build)
#
# Prints only diagnostics; the full ninja log stays in the volume at /build/ninja.log.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tree=${MESA_TREE:-/Volumes/mesa-cs/mesa-metal-guest}
vol=${KK_VOLUME:-limina-guest-kk-build}
mkdir -p "$here/out"
gtimeout --kill-after=30 3000 container run --rm --cpus 8 --memory 12g \
  -v "$tree:/src" -v "$vol:/build" -v "$here/out:/out" limina-build:fc44 bash -c '
if [ ! -f /build/kk/build.ninja ]; then
  meson setup /build/kk /src -Dvulkan-drivers=kosmickrisp -Dkosmickrisp-bridge=limina \
    -Dgallium-drivers= -Dplatforms=x11,wayland -Dglx=disabled -Degl=disabled -Dopengl=false \
    -Dgles1=disabled -Dgles2=disabled -Dgbm=disabled -Dbuildtype=debugoptimized \
    -Dvulkan-layers= -Dtools= > /build/meson.log 2>&1 || { tail -n 40 /build/meson.log; exit 1; }
fi
cd /build/kk && ninja -k 0 > /build/ninja.log 2>&1
rc=$?
grep -E "(error|warning):|undefined reference|multiple definition|FAILED" /build/ninja.log |
  grep -v "^cc \|Compiler for" | sort -u
[ $rc = 0 ] && cp src/kosmickrisp/vulkan/libvulkan_kosmickrisp.so /out/
exit $rc
' > "$here/out/build.txt" 2>&1
rc=$?
grep -v '^\[' "$here/out/build.txt"
exit $rc
