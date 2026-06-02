#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build EVERYTHING the windowed demo needs and launch it.
#
# `cargo run -p limina` only rebuilds the supervisor — but the windowed VM also needs the
# (separately built + codesigned) limina-vmm worker and the L1 test guest. This script
# builds all three, signs the worker, then runs `limina --window` against the L1 guest in
# `limina.hold` mode (animated scrolling bands, so there's something live to see).
#
# Usage: scripts/run-window.sh [debug|release]
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-debug}"
CARGO_FLAGS=()
[ "$PROFILE" = "release" ] && CARGO_FLAGS=(--release)

echo "==> building limina + limina-vmm ($PROFILE)"
cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} -p limina -p limina-vmm

echo "==> codesigning the worker (hypervisor entitlement)"
crates/limina-vmm/sign.sh "$PROFILE"

echo "==> building the L1 test guest (kernel + rootfs + init)"
scripts/build-test-guest.sh >/dev/null

echo "==> launching limina --window (close the window or Ctrl-C to quit)"
exec "target/$PROFILE/limina" --window \
    --kernel target/test-guest/Image \
    --rootfs target/test-guest/rootfs \
    --cmdline "console=ttyAMA0 rootfstype=virtiofs rw init=/init limina.hold"
