#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Extract a self-contained dbus-daemon (aarch64, musl) for the L1 test guest rootfs.
#
# The L1 guest needs a session bus to host more complex tests (first user: the
# limina-agent-session clipboard test against a mock mutter). Alpine's dbus package is
# musl-linked with a tiny closure (musl loader + libdbus + libexpat), so we pull the
# binary + libs + config out of a pinned Alpine container instead of maintaining a
# static build. Build tool only — Apple `container`, same as the kernel build.
#
# Output (gitignored): target/test-guest/dbus.tar  (paths relative to the rootfs)
# Usage: scripts/build-dbus-guest.sh   (skips if the tar already exists; rm to refresh)
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="target/test-guest"
TAR="$OUT/dbus.tar"
ALPINE="alpine:3.22"

if [ -f "$TAR" ]; then
    echo "==> $TAR already present (rm it to rebuild)"
    exit 0
fi
mkdir -p "$OUT"

echo "==> extracting dbus-daemon + closure from $ALPINE"
container run --rm -v "$PWD/$OUT:/out" "$ALPINE" sh -c '
set -e
apk add --no-cache dbus >/dev/null
# The closure: the daemon, the musl loader, the two libs, and the config tree.
tar -cf /out/dbus.tar.tmp \
    /usr/bin/dbus-daemon \
    /lib/ld-musl-aarch64.so.1 \
    /usr/lib/libdbus-1.so.3* \
    /usr/lib/libexpat.so.1* \
    /usr/share/dbus-1
mv /out/dbus.tar.tmp /out/dbus.tar
'
echo "    -> $TAR ($(wc -c < "$TAR") bytes)"
tar -tf "$TAR" | head -8