#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Assemble a self-contained Limina.app.
#
# Bundles the entire host venus/GL dylib closure into Contents/Frameworks, relocated
# to @rpath so it resolves relative to the app (no /Volumes/mesa-cs, no third_party
# prefixes, no Homebrew). Generates a relative-path KosmicKrisp ICD, copies the GOP
# firmware and the gvproxy NAT helper (so `--net` works on a Mac without Homebrew),
# writes Info.plist, and codesigns inside-out with an Apple Development identity from
# the keychain (keeps TCC grants stable across redeploys; ad-hoc only by explicit
# opt-in — LIMINA_SIGN_IDENTITY="-" or LIMINA_ALLOW_ADHOC=1 — otherwise abort). The
# supervisor sets the
# remaining bundle-relative venus env (VK_ICD_FILENAMES + the zink-on-KK Mesa selectors)
# when it detects it's running from the bundle — see crates/limina/src/venus_env.rs.
#
# Usage: scripts/build-app.sh [release|debug]
#
# DEFAULT IS RELEASE: the .app is the deployable artifact, and a debug default once
# shipped a debug build to the dogfood Mac (2026-07-20) — debug assertions + unoptimized
# lz4 made suspend/resume look 5-10x slower than the product actually is. Pass `debug`
# explicitly only when you need a debuggable bundle.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
PROFILE="${1:-release}"

APP="$ROOT/target/Limina.app"
MACOS="$APP/Contents/MacOS"
FW="$APP/Contents/Frameworks"
RES="$APP/Contents/Resources"

CARGO_FLAGS=()
[ "$PROFILE" = "release" ] && CARGO_FLAGS=(--release)

# ---- dev source locations (the things we vendor into the bundle) -----------------
VIRGL="$ROOT/third_party/virgl-prefix/lib/libvirglrenderer.1.dylib"
EPOXY="$ROOT/third_party/epoxy-egl-prefix/lib/libepoxy.0.dylib"
KK_BUILD="${LIMINA_KK_BUILD:-/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan}"
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

# KK/zink live on a case-sensitive sparse image whose mount macOS drops on reboot.
. "$ROOT/scripts/ensure-mesa-cs.sh"

for f in "$VIRGL" "$EPOXY" "$KK_DRIVER" "${DLOPEN_ROOTS[@]}" "$GOP_FD" "$GVPROXY"; do
  [ -e "$f" ] || { echo "MISSING required input: $f" >&2; echo "(build KK/zink on the mesa-cs volume, build the GOP firmware, or 'brew install gvproxy' / set LIMINA_GVPROXY_BIN)" >&2; exit 1; }
done

# ---- KK provenance tripwire (2026-07-29): a bundle silently shipped whatever ------
#      dylib sat in build-kk — once with a day of uncommitted instrumentation,
#      once with the unvalidated kk 0017 threaded submit (early-signaling fences
#      → dogfood tearing). Print what we ship, and refuse experimental KK unless
#      explicitly allowed.
kk_git="$(strings "$KK_DRIVER" | grep -m1 'git-' || true)"
echo "KK driver: $KK_DRIVER"
echo "KK version: ${kk_git:-<no git stamp>}"
if strings "$KK_DRIVER" | grep -q "LIMINA_KK_SUBMIT_THREAD" && [ "${LIMINA_ALLOW_THREADED_KK:-0}" != "1" ]; then
  echo "REFUSING to bundle: KK dylib carries LIMINA_KK_SUBMIT_THREAD (kk 0017 threaded submit," >&2
  echo "not validated for deployment). Rebuild KK at the deployable commit, or set" >&2
  echo "LIMINA_ALLOW_THREADED_KK=1 to ship it deliberately." >&2
  exit 1
fi
if strings "$KK_DRIVER" | grep -qE "LIMINA_KK_DRAWPROBE|KKTRACE"; then
  echo "REFUSING to bundle: KK dylib carries debug instrumentation (LIMINA_KK_DRAWPROBE/KKTRACE)." >&2
  exit 1
fi

# ---- signing identity (resolved FIRST so a locked keychain fails in seconds, ------
#      not after the whole build) ---------------------------------------------------
# Prefer a REAL identity over ad-hoc: TCC pins an ad-hoc app's Accessibility grant to
# the build's CDHash, so key-combo capture silently breaks on EVERY redeploy (System
# Settings keeps showing the checkbox ON). With an identity we additionally pin the
# app's designated requirement to identifier+team (subject.OU) instead of codesign's
# default leaf-CN pin, so grants also survive certificate reissue.
# LIMINA_SIGN_IDENTITY overrides autodetection; set it to "-" to force ad-hoc.
# Ad-hoc is NEVER a silent fallback: without an explicit "-" (or LIMINA_ALLOW_ADHOC=1)
# a missing/unusable identity ABORTS the build — a warning that scrolls past has
# already shipped an undeployable bundle once.
SIGN_ID="${LIMINA_SIGN_IDENTITY:-}"
if [ -z "$SIGN_ID" ]; then
  SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
             | sed -n 's/.*\([0-9A-F]\{40\}\) "Apple Development.*/\1/p' | head -1)"
fi
if [ -n "$SIGN_ID" ] && [ "$SIGN_ID" != "-" ]; then
  # Probe the identity before committing to it: keychain signing is context-dependent —
  # the usual failure is a LOCKED login keychain in this security session (fresh ssh
  # logins start locked; sessions descending from an unlocked one — GUI login, a
  # long-lived mux server — keep working). Session type alone doesn't matter: a
  # Background session with the keychain unlocked signs fine (verified 2026-07-27).
  PROBE="$(mktemp)" && cp /bin/ls "$PROBE"
  if ! codesign -s "$SIGN_ID" --force "$PROBE" >/dev/null 2>&1; then
    echo "==> WARNING: identity '$SIGN_ID' found but UNUSABLE from this shell —" >&2
    echo "    most likely the login keychain is LOCKED in this session. Try:" >&2
    echo "      security unlock-keychain ~/Library/Keychains/login.keychain-db" >&2
    echo "    (key-ACL denial is the rarer cause; see security set-key-partition-list)." >&2
    SIGN_ID=""
  else
    # Read the team (cert subject.OU) off the probe now — the worker signature needs
    # the team-pinned DR, and computing it here avoids a chicken-and-egg re-sign of
    # the sealed bundle.
    TEAM="$(codesign -dvv "$PROBE" 2>&1 | sed -n 's/^TeamIdentifier=//p')"
  fi
  rm -f "$PROBE"
fi

# The team-pinned designated requirement, shared by the app AND the worker so both satisfy
# the same TCC grants (see the codesign section below). Empty when signing ad-hoc.
DR=""
if [ "$SIGN_ID" != "-" ] && [ -n "${TEAM:-}" ] && [ "$TEAM" != "not set" ]; then
  DR="designated => identifier \"eti.noronha.limina\" and anchor apple generic and certificate leaf[subject.OU] = \"$TEAM\""
fi
if [ -z "$SIGN_ID" ]; then
  if [ "${LIMINA_ALLOW_ADHOC:-0}" != "1" ]; then
    echo "==> ERROR: no usable signing identity, refusing to build an AD-HOC bundle." >&2
    echo "    Ad-hoc TCC grants pin to the CDHash, so key-combo capture (and the mic" >&2
    echo "    grant) break on EVERY redeploy — an ad-hoc bundle must never reach the" >&2
    echo "    dogfood Mac. Fix the identity (unlock the keychain, or Xcode → Settings →" >&2
    echo "    Accounts → Manage Certificates), or explicitly opt in to ad-hoc with" >&2
    echo "    LIMINA_ALLOW_ADHOC=1 (or LIMINA_SIGN_IDENTITY='-')." >&2
    exit 1
  fi
  SIGN_ID="-"
  echo "==> WARNING: signing AD-HOC (LIMINA_ALLOW_ADHOC=1). The Accessibility grant" >&2
  echo "    (key-combo capture) will break on every redeploy (TCC pins the CDHash)." >&2
  echo "    DO NOT deploy this bundle to the dogfood Mac." >&2
fi

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

# The M14 FIDO Secure-Enclave shim: the supervisor links liblimina_sep.dylib (built by
# crates/limina/build.rs into its OUT_DIR). Dev/test bakes an rpath to that OUT_DIR; in the
# bundle we ship the dylib in Frameworks and add the Frameworks rpath to the supervisor
# below. Its Swift-runtime deps live in /usr/lib/swift (an rpath build.rs already baked).
SEP_DYLIB="$(ls -t "$ROOT"/target/$PROFILE/build/limina-*/out/liblimina_sep.dylib 2>/dev/null | head -1)"
if [ -n "$SEP_DYLIB" ] && [ -e "$SEP_DYLIB" ]; then
  bundle_dylib "$SEP_DYLIB"
else
  echo "==> WARNING: liblimina_sep.dylib not found under target/$PROFILE/build — FIDO/Touch ID" >&2
  echo "    will fail to load in the bundle. Did 'cargo build --release -p limina' run?" >&2
fi

# ---- relocate the supervisor's SEP-dylib link ------------------------------------
# The supervisor links @rpath/liblimina_sep.dylib (install_name set by build.rs). Add the
# Frameworks rpath so it resolves inside the bundle (the OUT_DIR rpath build.rs baked points
# at the build tree, absent on a user's Mac — harmless, dyld just tries the next rpath).
echo "==> adding Frameworks rpath to the supervisor (FIDO SEP dylib)"
install_name_tool -add_rpath "@loader_path/../Frameworks" "$MACOS/limina"
# Drop the dev-only OUT_DIR rpath build.rs baked (points into target/.../build/); it exists
# on THIS machine so dyld would resolve the SEP dylib from the build tree, not the bundle.
# Removing it makes the .app truly self-contained (Frameworks + /usr/lib/swift only).
otool -l "$MACOS/limina" | awk '/LC_RPATH/{g=1;next} g&&/ path /{print $2;g=0}' | while read -r rp; do
  case "$rp" in
    */target/*/build/*) install_name_tool -delete_rpath "$rp" "$MACOS/limina" || true ;;
  esac
done

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

# ---- app icon (source + prep tool in assets/icon/) --------------------------------
cp "$ROOT/assets/icon/Limina.icns" "$RES/Limina.icns"

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
  <key>CFBundleIconFile</key><string>Limina</string>
  <!-- Mic capture (opt-in `--mic` / `[hardware] mic`): the guest's virtio-snd input
       stream records the host microphone via CoreAudio. macOS gates that behind the
       mic TCC prompt, which only appears from an app-bundle (LaunchServices) context —
       this usage string is what the prompt shows. The worker inherits the grant as the
       app's child (responsible process). No prompt unless a guest actually opens the
       capture stream. -->
  <key>NSMicrophoneUsageDescription</key><string>Limina lets a virtual machine capture your microphone when you enable mic sharing for it.</string>
  <!-- .liminavm bundles: a package (the directory shows as one Finder item) owned
       by Limina; opening one routes to application:openURLs: in the control
       center, which starts the VM. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Limina Virtual Machine</string>
      <key>LSItemContentTypes</key><array><string>eti.noronha.limina.vm</string></array>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>CFBundleTypeIconFile</key><string>Limina</string>
      <key>LSHandlerRank</key><string>Owner</string>
      <key>LSTypeIsPackage</key><true/>
    </dict>
  </array>
  <key>UTExportedTypeDeclarations</key>
  <array>
    <dict>
      <key>UTTypeIdentifier</key><string>eti.noronha.limina.vm</string>
      <key>UTTypeDescription</key><string>Limina Virtual Machine</string>
      <key>UTTypeConformsTo</key>
      <array>
        <string>com.apple.package</string>
        <string>public.data</string>
      </array>
      <key>UTTypeTagSpecification</key>
      <dict>
        <key>public.filename-extension</key><array><string>liminavm</string></array>
      </dict>
    </dict>
  </array>
</dict>
</plist>
PLIST

# ---- codesign inside-out ----------------------------------------------------------
# $SIGN_ID and $DR were resolved (and the identity probed) at the top of the script,
# before the build — see "signing identity" up there for the TCC rationale.
echo "==> codesigning as '$SIGN_ID' (dylibs → worker → supervisor → app)"
find "$FW" -type f -name '*.dylib' -exec codesign -s "$SIGN_ID" --force {} \;
# The worker (limina-vmm), not the app main, is the process that opens CoreAudio for `--mic`.
# TCC records the mic grant under the responsible app (eti.noronha.limina) but validates the
# ACCESSING binary against the grant's csreq (identifier "eti.noronha.limina" + team OU), so
# the worker must sign with the app's identifier and the same team-pinned DR — otherwise its
# `.vmm` identity fails the csreq and CoreAudio delivers silence despite an allowed grant.
if [ -n "$DR" ]; then
  codesign -s "$SIGN_ID" --force -i "eti.noronha.limina" -r="$DR" \
    --entitlements "$ROOT/crates/limina-vmm/hvf-entitlements.plist" "$MACOS/limina-vmm"
else
  codesign -s "$SIGN_ID" --force \
    --entitlements "$ROOT/crates/limina-vmm/hvf-entitlements.plist" "$MACOS/limina-vmm"
fi
codesign -s "$SIGN_ID" --force "$MACOS/gvproxy"
codesign -s "$SIGN_ID" --force "$MACOS/limina"
# Seal the app (outer) LAST, after the nested worker is in its final form, with the same
# team-pinned DR so the app grant (Accessibility, etc.) also survives rebuilds/cert reissue.
if [ -n "$DR" ]; then
  codesign -s "$SIGN_ID" --force -r="$DR" "$APP"
  echo "    designated requirement pinned to team $TEAM (app + worker; TCC grants incl. mic reach the worker and survive rebuilds)"
else
  codesign -s "$SIGN_ID" --force "$APP"
fi

echo "==> done: $APP"
du -sh "$APP" | awk '{print "    bundle size: " $1}'
echo "    verify venus link:"
otool -L "$MACOS/limina-vmm" | grep -iE 'virgl|epoxy|vulkan' | sed 's/^/      /'
