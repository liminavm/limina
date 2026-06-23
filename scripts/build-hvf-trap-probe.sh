#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the hvf-trap-probe bare-metal arm64 Image (spikes/hvf-trap-probe/probe.S) used by
# crates/limina-test/tests/hvf_graceful.rs. Position-independent + single-section, so we just
# assemble and objcopy the .text out — no linker needed.
#
# Output (gitignored): target/hvf-trap-probe/Image
set -euo pipefail
cd "$(dirname "$0")/.."

LLVM="$(brew --prefix llvm 2>/dev/null)/bin"
CLANG="$LLVM/clang"
OBJCOPY="$LLVM/llvm-objcopy"
for t in "$CLANG" "$OBJCOPY"; do
  [ -x "$t" ] || { echo "missing $t (brew install llvm)" >&2; exit 1; }
done

SRC=spikes/hvf-trap-probe/probe.S
OUT=target/hvf-trap-probe
mkdir -p "$OUT"

"$CLANG" --target=aarch64-linux-gnu -nostdlib -c "$SRC" -o "$OUT/probe.o"
"$OBJCOPY" -O binary --only-section=.text "$OUT/probe.o" "$OUT/Image"

echo "==> built $OUT/Image ($(stat -f %z "$OUT/Image") bytes)"
