#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the notch probe as a minimal .app bundle.
#
# A bare (non-bundled) binary CANNOT enter Spaces fullscreen: `toggleFullScreen:` is a
# silent no-op there — the first two runs of this probe reported
# `window.styleMask.fullScreen false` after toggling, which reads like an AppKit bug and
# isn't one. LaunchServices only grants fullscreen to a bundled app, so the probe has to be
# one. Run it with `open -a ./NotchProbe.app --stdout /dev/stdout` (or ./run.sh).
set -euo pipefail
cd "$(dirname "$0")"

APP="NotchProbe.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"

swiftc -O probe.swift -o "$APP/Contents/MacOS/NotchProbe"

# SAFE_AREA_COMPAT=true|false injects NSPrefersDisplaySafeAreaCompatibilityMode; unset omits
# the key entirely (the shipping limina default).
COMPAT_KEY=""
if [[ -n "${SAFE_AREA_COMPAT:-}" ]]; then
  COMPAT_KEY="  <key>NSPrefersDisplaySafeAreaCompatibilityMode</key><${SAFE_AREA_COMPAT}/>"
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>NotchProbe</string>
  <key>CFBundleIdentifier</key><string>eti.noronha.limina.notchprobe</string>
  <key>CFBundleExecutable</key><string>NotchProbe</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
$COMPAT_KEY
</dict>
</plist>
PLIST

codesign -s - --force --deep "$APP" >/dev/null 2>&1 || true
echo "built $PWD/$APP"
