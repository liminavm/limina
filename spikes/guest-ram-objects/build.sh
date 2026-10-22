#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build and codesign the guest-ram-objects probe (docs/design/guest-ram-objects.md §4.1).
# Running it touches hv_vm_*: do that outside any tool sandbox, on an idle host.
set -e
cd "$(dirname "$0")"

LLVM="$(brew --prefix llvm 2>/dev/null)/bin"
for t in "$LLVM/clang" "$LLVM/llvm-objcopy"; do
    [ -x "$t" ] || { echo "missing $t (brew install llvm)" >&2; exit 1; }
done

"$LLVM/clang" --target=aarch64-linux-gnu -nostdlib -c payload.S -o payload.o
"$LLVM/llvm-objcopy" -O binary --only-section=.text payload.o payload.bin
clang -O1 -g -Wall -Wextra -o probe probe.c -framework Hypervisor
codesign --entitlements hv.entitlements -s - --force probe
echo "==> probe built and signed; run: ./probe <leg> [compressed] [ram-GiB]"
