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

echo "==> building the hvf-trap-probe (bare-metal PSCI probe for hvf_graceful)"
scripts/build-hvf-trap-probe.sh >/dev/null

echo "==> running boot tests (LIMINA_HVF_TESTS=1)"
# Build the test crate with the same profile; limina-test doesn't depend on the worker so
# this won't rebuild/unsign it. --test-threads=1: one VM at a time.
# `venus` is the enhanced-tier (16 KiB kernel) 3D test; it SKIPs instantly unless
# `Image-16k` exists (build with `scripts/build-test-kernel.sh PAGESIZE=16k`), and when it
# does it runs a full Fedora-on-custom-kernel boot (~minutes) to confirm venus enumerates.
# `venus_replay` is the tier-2 RENDERING test (seated dev-enh boot + GL trace replay,
# venus vs llvmpipe pixel compare); it SKIPs without the dev-enh golden, the KK ICD, or
# the trace fixture (fixtures/traces/ — regenerate via spikes/trace-replay/).
# `l2_share_71` is the ≥7.1-kernel virtiofs --share guard (libkrun 0090); it SKIPs unless a
# ≥7.1 16 KiB test kernel exists (build with
# `KVER=v7.1 PAGESIZE=16k KIMAGE_NAME=Image-16k-71 PATCHES_OPTIONAL=1 scripts/build-test-kernel.sh`).
# --no-fail-fast is load-bearing: cargo test fail-fasts ACROSS test binaries, so without it the
# first failing binary (e.g. boot) silently stops the run and every later binary (net, venus,
# venus_replay, …) never executes — masking their status. With it, every binary runs and reports.
LIMINA_HVF_TESTS=1 cargo test --no-fail-fast ${CARGO_PROFILE_FLAG[@]+"${CARGO_PROFILE_FLAG[@]}"} -p limina-test \
    --test l1_boot --test l1_agent --test l1_shutdown --test l1_real_agent --test l1_multi_agent --test l1_clipboard --test l1_session_helper --test l1_share --test l1_liveness --test l1_display --test l1_blob_map --test l1_console --test l1_serial --test l1_command --test l1_resize --test l1_snapshot --test boot --test net --test reboot --test disks --test vmdef --test inplace_s2idle --test l2_share_71 --test l2_vcpu_hotplug --test venus --test venus_reset --test venus_replay --test venus_fallback --test venus_fd_census --test venus_session_preserved --test venus_clear_rect --test virgl --test virgl_fence --test balloon --test balloon_inflate --test balloon_psi --test balloon_burst --test usb --test battery --test hvf_graceful --test venus_fence_present --test venus_fence_lost --test venus_park_on_busy_reset \
    -- --nocapture --test-threads=1 "$@"
