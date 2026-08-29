#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Regenerate crates/limina-test/assets/vkstill-spv.h from the GLSL beside it.
#
# The SPIR-V is committed so the guest needs only a C compiler to build vkstill; run this
# after editing either shader. Needs glslangValidator (brew install glslang).
set -euo pipefail
cd "$(dirname "$0")/../crates/limina-test/assets"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
glslangValidator -V -S vert -o "$tmp/v.spv" vkstill.vert
glslangValidator -V -S frag -o "$tmp/f.spv" vkstill.frag
python3 - "$tmp/v.spv" "$tmp/f.spv" > vkstill-spv.h <<'PY'
import sys, struct
print("// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception")
print("// Copyright © 2026 Gustavo Noronha Silva")
print("//")
print("// GENERATED from vkstill.vert / vkstill.frag by scripts/gen-vkstill-spv.sh — do not edit.")
print("// Embedded rather than compiled in the guest so the test needs no shader toolchain there.")
print("#include <stdint.h>")
for path, name in ((sys.argv[1], "VERT_SPV"), (sys.argv[2], "FRAG_SPV")):
    d = open(path, "rb").read()
    w = struct.unpack("<%dI" % (len(d) // 4), d)
    print()
    print("static const uint32_t %s[] = {" % name)
    for i in range(0, len(w), 6):
        print("    " + " ".join("0x%08x," % x for x in w[i:i + 6]))
    print("};")
PY
echo "wrote crates/limina-test/assets/vkstill-spv.h"
