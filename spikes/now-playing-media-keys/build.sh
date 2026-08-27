#!/bin/sh
# Build both arms of the probe: a bare CLI binary and a minimal .app bundle.
#
# The bundle exists because "a bare binary has no CFBundleIdentifier" is a known
# confounder for Now Playing — a refusal in the bare arm must not be read as
# "macOS refuses a silent process" until the bundled arm has also been tried.
set -eu
cd "$(dirname "$0")"

swiftc -O nowplaying.swift -o nowplaying
echo "built: spikes/now-playing-media-keys/nowplaying (bare)"

APP=NowPlayingProbe.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp nowplaying "$APP/Contents/MacOS/nowplaying"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>nowplaying</string>
  <key>CFBundleIdentifier</key><string>dev.noronha.limina.nowplaying-probe</string>
  <key>CFBundleName</key><string>NowPlayingProbe</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
</dict>
</plist>
PLIST
codesign --force --sign - "$APP" >/dev/null 2>&1 || echo "warning: ad-hoc codesign failed"
echo "built: spikes/now-playing-media-keys/$APP (bundled)"
