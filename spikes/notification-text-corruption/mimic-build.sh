#!/usr/bin/env bash
# Build glyphmimic for whichever side we are on.
#   host  (macOS): links the zink-on-KosmicKrisp Mesa in /Volumes/mesa-cs/zink-kk-prefix
#   guest (Linux): links the guest's own EGL/GLES, i.e. mesa's virgl driver
# The binary is not committed.
set -eu
cd "$(dirname "$0")"
if [ "$(uname -s)" = "Darwin" ]; then
    MESA_SRC="${MESA_SRC:-/Volumes/mesa-cs/mesa}"
    MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
    [ -d "$MESA_PREFIX/lib" ] || { echo "no zink-kk prefix at $MESA_PREFIX (scripts/ensure-mesa-cs.sh)"; exit 1; }
    cc -O2 -Wall -I"$MESA_SRC/include" glyphmimic.c \
       -L"$MESA_PREFIX/lib" -lEGL -lGLESv2 -o glyphmimic
else
    cc -O2 -Wall glyphmimic.c -lEGL -lGLESv2 -o glyphmimic
fi
echo "built: $(pwd)/glyphmimic"
