#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + codesign + run the balloon-unmap-fault mechanism spike.
# Same pattern as spikes/m9-hvf-state-roundtrip/build.sh.
#
# NOTE: hv_vm_* is blocked by the repo Bash sandbox — run this with the sandbox off.
set -e
cd "$(dirname "$0")"

LLVM="$(brew --prefix llvm 2>/dev/null)/bin"
for t in "$LLVM/clang" "$LLVM/llvm-objcopy"; do
    [ -x "$t" ] || { echo "missing $t (brew install llvm)" >&2; exit 1; }
done

"$LLVM/clang" --target=aarch64-linux-gnu -nostdlib -c payload.S -o payload.o
"$LLVM/llvm-objcopy" -O binary --only-section=.text payload.o payload.bin
echo "==> payload.bin ($(stat -f %z payload.bin) bytes)"

clang -O1 -g -Wall -Wextra -o probe probe.c -framework Hypervisor
codesign --entitlements hv.entitlements -s - --force probe
echo "==> probe built + signed"

if [ "${1:-}" != "build-only" ]; then
    ./probe payload.bin
fi
