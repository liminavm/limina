#!/usr/bin/env bash
# Build vrend-replay against OUR virglrenderer -- never Homebrew's.
#
# The prefix matters for the same reason it matters for the worker: Homebrew's virglrenderer has
# none of our instrumentation, and linking it would produce a replayer that runs and reports
# nothing. `otool -L` afterwards is the check, exactly as with limina-vmm.
set -eu
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"
PREFIX="$ROOT/third_party/virgl-prefix"
[ -f "$PREFIX/lib/libvirglrenderer.dylib" ] || { echo "build virglrenderer first: scripts/build-virglrenderer.sh"; exit 1; }
cc -O2 -Wall -Wextra -o vrend-replay vrend-replay.c \
   -I"$ROOT/third_party/virglrenderer/src" \
   -I"$ROOT/third_party/virglrenderer/build/src" \
   -I"$PREFIX/include/virgl" \
   -L"$PREFIX/lib" -lvirglrenderer -Wl,-rpath,"$PREFIX/lib"
echo "built: $PWD/vrend-replay"
otool -L vrend-replay | grep virgl
