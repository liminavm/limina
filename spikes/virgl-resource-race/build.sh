#!/usr/bin/env bash
# Build the virgl_resource_table race harness against the fork's own sources, under TSan.
#
# The harness compiles the real virgl_resource.c (plus the hash table under it) rather than
# linking libvirglrenderer, because none of these symbols are exported from the dylib.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
vr=$here/../../third_party/virglrenderer
out=$here/race

[ -d "$vr/src" ] || { echo "no virglrenderer checkout: run 'cargo xtask vendor'" >&2; exit 1; }
[ -f "$vr/build/config.h" ] || { echo "no configured build at $vr/build (needs config.h + virgl-version.h)" >&2; exit 1; }

clang -fsanitize=thread -g -O1 -std=c11 -Wall \
   -DUTIL_ARCH_LITTLE_ENDIAN=1 -DUTIL_ARCH_BIG_ENDIAN=0 \
   -include "$vr/build/config.h" \
   -I"$here" \
   -I"$vr/build" \
   -I"$vr/build/src" \
   -I"$vr/src" \
   -I"$vr/src/gallium/include" \
   -I"$vr/src/gallium/auxiliary" \
   -I"$vr/src/mesa" \
   -I"$vr/src/mesa/compat" \
   -I"$vr/src/mesa/pipe" \
   -I"$vr/src/mesa/util" \
   -o "$out" \
   "$here/race.c" \
   "$vr/src/virgl_resource.c" \
   "$vr/src/gallium/auxiliary/util/u_hash_table.c" \
   "$vr/src/mesa/util/hash_table.c" \
   "$vr/src/mesa/util/ralloc.c" \
   "$vr/src/mesa/util/os_file.c" \
   "$here/stubs.c"

echo "built $out"
