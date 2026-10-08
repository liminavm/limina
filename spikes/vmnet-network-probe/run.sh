#!/bin/sh
# Build miniguest, sign it ad-hoc with both entitlements, and run one mode under a hard cap.
# Usage: ./run.sh lease <shared|host|bridged> [ifname] [--vhdr] | ./run.sh netobj
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${MINIGUEST_OUT:-$here/miniguest}
xcrun clang -O1 -Wall -Wextra -Wno-unused-parameter -mmacosx-version-min=26.0 \
    -framework vmnet -o "$out" "$here/miniguest.c"
codesign -f -s - --entitlements "$here/both.entitlements" "$out" 2>/dev/null
exec gtimeout --kill-after=5 "${MINIGUEST_TIMEOUT:-90}" "$out" "$@"
