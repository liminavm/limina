#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build + codesign the limina binaries, then run the HVF-gated boot tests.
#
# The boot tests drive Hypervisor.framework, so the worker must be codesigned with
# com.apple.security.hypervisor and the gate (LIMINA_HVF_TESTS) must be on. Plain
# `cargo test` skips them; this is the recipe that actually runs them.
#
# Usage: scripts/test-boot.sh [debug|release] [extra cargo test args...]
#   LIMINA_TEST_DISK=/path/to.raw  scripts/test-boot.sh        # override the guest image
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-debug}"
[ $# -gt 0 ] && shift || true

CARGO_PROFILE_FLAG=()
[ "$PROFILE" = "release" ] && CARGO_PROFILE_FLAG=(--release)

echo "==> building limina + limina-vmm ($PROFILE)"
cargo build ${CARGO_PROFILE_FLAG[@]+"${CARGO_PROFILE_FLAG[@]}"} -p limina -p limina-vmm

echo "==> codesigning the worker (hypervisor entitlement)"
crates/limina-vmm/sign.sh "$PROFILE"

echo "==> checking the worker links our virglrenderer (venus guard)"
scripts/check-virgl-link.sh "target/$PROFILE/limina-vmm"

echo "==> building the L1 test guest (kernel + rootfs)"
scripts/build-test-guest.sh >/dev/null

echo "==> running boot tests (LIMINA_HVF_TESTS=1)"
# Build the test crate with the same profile; limina-test doesn't depend on the worker so
# this won't rebuild/unsign it. --test-threads=1: one VM at a time.
# `venus` is the enhanced-tier (16 KiB kernel) 3D test; it SKIPs instantly unless
# `Image-16k` exists (build with `scripts/build-test-kernel.sh PAGESIZE=16k`), and when it
# does it runs a full Fedora-on-custom-kernel boot (~minutes) to confirm venus enumerates.
LIMINA_HVF_TESTS=1 cargo test ${CARGO_PROFILE_FLAG[@]+"${CARGO_PROFILE_FLAG[@]}"} -p limina-test \
    --test l1_boot --test l1_agent --test l1_display --test l1_console --test l1_serial --test boot --test net --test venus \
    -- --nocapture --test-threads=1 "$@"
