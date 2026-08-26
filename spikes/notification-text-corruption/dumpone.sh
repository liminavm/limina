#!/usr/bin/env bash
# Dump one global IOSurface to a downscaled PNG.  dumpone.sh <surface-id> <out.png>
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
[ -x "$HERE/iosdump" ] || "$HERE/build.sh" >/dev/null
"$HERE/iosdump" "$1" >/dev/null 2>&1
[ -f "/tmp/ios-$1.png" ] && sips -z 720 1280 "/tmp/ios-$1.png" --out "$2" >/dev/null 2>&1
