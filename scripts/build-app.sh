#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Assemble a self-contained Limina.app.
#
# Bundles the entire host venus/GL dylib closure into Contents/Frameworks, relocated
# to @rpath so it resolves relative to the app (no /Volumes/mesa-cs, no third_party
# prefixes, no Homebrew). Generates a relative-path KosmicKrisp ICD, copies the GOP
# firmware and the gvproxy NAT helper (so `--net` works on a Mac without Homebrew),
# writes Info.plist, and ad-hoc codesigns inside-out. The supervisor sets the
# remaining bundle-relative venus env (VK_ICD_FILENAMES + the zink-on-KK Mesa selectors)
# when it detects it's running from the bundle — see crates/limina/src/venus_env.rs.
#
# Usage: scripts/build-app.sh [debug|release]
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
PROFILE="${1:-debug}"

APP="$ROOT/target/Limina.app"
MACOS="$APP/Contents/MacOS"
FW="$APP/Contents/Frameworks"
RES="$APP/Contents/Resources"

CARGO_FLAGS=()
[ "$PROFILE" = "release" ] && CARGO_FLAGS=(--release)

# ---- dev source locations (the things we vendor into the bundle) -----------------
VIRGL="$ROOT/third_party/virgl-prefix/lib/libvirglrenderer.1.dylib"
EPOXY="$ROOT/third_party/epoxy-egl-prefix/lib/libepoxy.0.dylib"
KK_BUILD="/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan"
ZINK="/Volumes/mesa-cs/zink-kk-prefix/lib"
KK_DRIVER="$KK_BUILD/libvulkan_kosmickrisp.dylib"
# dlopen'd-by-env roots (not in the worker's link closure):
DLOPEN_ROOTS=(
  "$KK_DRIVER"
  "$ZINK/libEGL.dylib"
  "$ZINK/libGLESv2.dylib"
  "$ZINK/libgallium-26.2.0-devel.dylib"
)
GOP_FD="$ROOT/target/krun-efi/KRUN_EFI.gop.fd"
# gvproxy (user-mode NAT for `--net`); vendored into the bundle so networking works on a
# Homebrew-less Mac. Override the source with LIMINA_GVPROXY_BIN; defaults to Homebrew's.
GVPROXY="${LIMINA_GVPROXY_BIN:-/opt/homebrew/bin/gvproxy}"

for f in "$VIRGL" "$EPOXY" "$KK_DRIVER" "${DLOPEN_ROOTS[@]}" "$GOP_FD" "$GVPROXY"; do
  [ -e "$f" ] || { echo "MISSING required input: $f" >&2; echo "(mount third_party/mesa-cs.sparseimage and build KK/zink, build the GOP firmware, or 'brew install gvproxy' / set LIMINA_GVPROXY_BIN)" >&2; exit 1; }
done

echo "==> building limina + limina-vmm ($PROFILE)"
cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} -p limina -p limina-vmm

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$MACOS" "$FW" "$RES/vulkan"
cp "$ROOT/target/$PROFILE/limina" "$MACOS/limina"
cp "$ROOT/target/$PROFILE/limina-vmm" "$MACOS/limina-vmm"
# gvproxy alongside the binaries (Contents/MacOS/gvproxy); gateway.rs resolves it bundle-relative.
cp "$GVPROXY" "$MACOS/gvproxy"
chmod u+w "$MACOS/gvproxy"

# ---- recursive dylib bundler -----------------------------------------------------
# Copy a dylib into Frameworks, set its id to @rpath/<leaf>, then for every non-system
# dependency: resolve it on disk, recurse, and rewrite the reference to @rpath/<leaf>.
bundle_dylib() {
  local src="$1" leaf
  leaf="$(basename "$src")"
  # Dedup (and break dependency cycles) by presence in Frameworks: we copy first,
  # then process, so a re-entry for an in-progress lib short-circuits here.
  [ -e "$FW/$leaf" ] && return
  cp -f "$src" "$FW/$leaf"
  chmod u+w "$FW/$leaf"
  install_name_tool -id "@rpath/$leaf" "$FW/$leaf"
  # @loader_path lets @rpath/<sibling> resolve within Frameworks.
  install_name_tool -add_rpath "@loader_path" "$FW/$leaf" 2>/dev/null || true

  local dep deppath depleaf
  while read -r dep; do
    case "$dep" in
      ""|/usr/lib/*|/System/*) continue ;;
      @rpath/*|@loader_path/*|@executable_path/*) continue ;;
    esac
    deppath="$dep"
    # Homebrew opt/ symlinks and friends resolve straight through.
    [ -e "$deppath" ] || { echo "  WARN unresolved dep $dep (of $leaf)"; continue; }
    depleaf="$(basename "$dep")"
    bundle_dylib "$deppath"
    install_name_tool -change "$dep" "@rpath/$depleaf" "$FW/$leaf"
  done < <(otool -L "$FW/$leaf" | tail -n +2 | awk '{print $1}')
}

echo "==> bundling dylib closure into Frameworks"
bundle_dylib "$VIRGL"
bundle_dylib "$EPOXY"
for r in "${DLOPEN_ROOTS[@]}"; do bundle_dylib "$r"; done

# ---- relocate the worker binary's own links --------------------------------------
echo "==> relocating limina-vmm links to @rpath"
install_name_tool -add_rpath "@loader_path/../Frameworks" "$MACOS/limina-vmm"
while read -r dep; do
  case "$dep" in
    ""|/usr/lib/*|/System/*|@*) continue ;;
  esac
  depleaf="$(basename "$dep")"
  if [ -e "$FW/$depleaf" ]; then
    install_name_tool -change "$dep" "@rpath/$depleaf" "$MACOS/limina-vmm"
  fi
done < <(otool -L "$MACOS/limina-vmm" | tail -n +2 | awk '{print $1}')

# ---- relative KosmicKrisp ICD ----------------------------------------------------
echo "==> writing relative KK ICD"
cat > "$RES/vulkan/kosmickrisp_icd.json" <<'JSON'
{
    "ICD": {
        "api_version": "1.3.353",
        "library_path": "../../Frameworks/libvulkan_kosmickrisp.dylib"
    },
    "file_format_version": "1.0.1"
}
JSON

# ---- GOP firmware (existing bundle-resolution pattern) ---------------------------
cp "$GOP_FD" "$RES/KRUN_EFI.gop.fd"

# ---- Info.plist ------------------------------------------------------------------
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Limina</string>
  <key>CFBundleDisplayName</key><string>Limina</string>
  <key>CFBundleIdentifier</key><string>eti.noronha.limina</string>
  <key>CFBundleExecutable</key><string>limina</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSArchitecturePriority</key><array><string>arm64</string></array>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# ---- codesign inside-out (ad-hoc) ------------------------------------------------
echo "==> ad-hoc codesigning (dylibs → worker → supervisor → app)"
find "$FW" -type f -name '*.dylib' -exec codesign -s - --force {} \;
codesign -s - --force --entitlements "$ROOT/crates/limina-vmm/hvf-entitlements.plist" "$MACOS/limina-vmm"
codesign -s - --force "$MACOS/gvproxy"
codesign -s - --force "$MACOS/limina"
codesign -s - --force "$APP"

echo "==> done: $APP"
du -sh "$APP" | awk '{print "    bundle size: " $1}'
echo "    verify venus link:"
otool -L "$MACOS/limina-vmm" | grep -iE 'virgl|epoxy|vulkan' | sed 's/^/      /'
