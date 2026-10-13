#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + codesign the replayer-ipc spike and run it. Run with the Bash sandbox off
# (hv_vm_* is blocked inside it). Usage: ./build.sh [build-only]
set -e
cd "$(dirname "$0")"

LLVM="$(brew --prefix llvm 2>/dev/null)/bin"
for t in "$LLVM/clang" "$LLVM/llvm-objcopy"; do
    [ -x "$t" ] || { echo "missing $t (brew install llvm)" >&2; exit 1; }
done
"$LLVM/clang" --target=aarch64-linux-gnu -nostdlib -c payload.S -o payload.o
"$LLVM/llvm-objcopy" -O binary --only-section=.text payload.o payload.bin

clang -O2 -g -Wall -Wextra -fobjc-arc -o ipc ipc.m \
    -framework Hypervisor -framework Metal -framework Foundation
codesign --entitlements ../userptr-remap-metal/hv.entitlements -s - --force ipc
echo "==> ipc built + signed"

[ "${1:-}" = "build-only" ] || ./ipc payload.bin
