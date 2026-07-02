#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + codesign + run M9.0 spike #2 (HVF vCPU+GIC state round-trip).
#
# Payload: flat bare-metal arm64 binary (brew llvm clang + llvm-objcopy, same pattern
# as scripts/build-hvf-trap-probe.sh). Driver: system clang + Hypervisor.framework,
# ad-hoc signed with com.apple.security.hypervisor.
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

clang -O1 -g -Wall -Wextra -o roundtrip roundtrip.c -framework Hypervisor
codesign --entitlements hv.entitlements -s - --force roundtrip
echo "==> roundtrip built + signed"

if [ "${1:-}" != "build-only" ]; then
    ./roundtrip payload.bin
fi
