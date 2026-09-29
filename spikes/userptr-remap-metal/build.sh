#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + codesign the userptr-remap-metal spike. Run with the Bash sandbox off
# (hv_vm_* is blocked inside it). Usage: ./build.sh [build-only | probe args...]
set -e
cd "$(dirname "$0")"

LLVM="$(brew --prefix llvm 2>/dev/null)/bin"
for t in "$LLVM/clang" "$LLVM/llvm-objcopy"; do
    [ -x "$t" ] || { echo "missing $t (brew install llvm)" >&2; exit 1; }
done

"$LLVM/clang" --target=aarch64-linux-gnu -nostdlib -c payload.S -o payload.o
"$LLVM/llvm-objcopy" -O binary --only-section=.text payload.o payload.bin
echo "==> payload.bin ($(stat -f %z payload.bin) bytes)"

clang -O1 -g -Wall -Wextra -fobjc-arc -o probe probe.m \
    -framework Hypervisor -framework Metal -framework Foundation
codesign --entitlements hv.entitlements -s - --force probe
echo "==> probe built + signed"

clang -O1 -g -Wall -Wextra -fobjc-arc -o fourk-probe fourk-probe.m \
    -framework Hypervisor -framework Metal -framework Foundation
codesign --entitlements hv.entitlements -s - --force fourk-probe
clang -arch x86_64 -O1 -g -Wall -Wextra -fobjc-arc -o fourk-probe-x86 fourk-probe.m \
    -framework Metal -framework Foundation
clang -O1 -Wall -o fourk-spawn fourk-spawn.c
echo "==> fourk-probe (arm64 + x86_64) and fourk-spawn built"

if [ "${1:-}" != "build-only" ]; then
    ./probe payload.bin "$@"
fi
