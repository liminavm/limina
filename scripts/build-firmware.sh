#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the guest's UEFI firmware, Cardea (github.com/liminavm/cardea, pinned by [cardea] in
# third_party/manifest.toml, vendored at third_party/cardea), into target/firmware/: efi.fd is
# what the EFI boot path, the app and the test suite use. With DEBUG=1 it writes efi.debug.fd
# instead, a build that logs its progress on the serial line.
#
# Native: cargo for aarch64-unknown-none and the toolchain's llvm-objcopy; no container. The
# logo is limina's (assets/firmware/logo.bmp); Cardea embeds whatever its builder names.
#
# Usage: scripts/build-firmware.sh
#        DEBUG=1 scripts/build-firmware.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=scripts/lib/manifest.sh
. scripts/lib/manifest.sh
TREE="$(pin_tree cardea)"
[ -d "$TREE" ] || { echo "no Cardea tree at $TREE: run \`cargo xtask vendor\`" >&2; exit 1; }
pin_note cardea

OUT="target/firmware"
mkdir -p "$OUT"
if [ "${DEBUG:-}" = 1 ]; then OUT_NAME="efi.debug.fd"; else OUT_NAME="efi.fd"; fi

# The revision this firmware was built from, for scripts/test-boot.sh to check against the pin:
# the image itself does not say. A tree with uncommitted changes is not its HEAD, and says so, so
# such a build never passes for the pin.
REV="$(_manifest_git -C "$TREE" rev-parse HEAD)"
if [ -n "$(_manifest_git -C "$TREE" status --porcelain --untracked-files=no)" ]; then
    REV="$REV (modified)"
fi

echo "==> building Cardea ($OUT_NAME) from $TREE @ $REV"
env ${DEBUG:+CARDEA_DEBUG=1} CARDEA_LOGO="$(pwd)/assets/firmware/logo.bmp" \
    CARDEA_OUT="$(pwd)/$OUT/$OUT_NAME" "$TREE/scripts/build.sh"
echo "$REV" > "$OUT/$OUT_NAME.rev"
echo "==> firmware ready: $OUT/$OUT_NAME"
