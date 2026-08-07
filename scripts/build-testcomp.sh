#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
#
# Build limina-testcomp — the small Wayland compositor test vehicle (testcomp/, GPL-3.0-only;
# read testcomp/README.md before moving code across that boundary).
#
# Runs in the unified limina-build:fc43 container like every other Linux build: this emits an
# aarch64 glibc ELF that dlopens the guest's libvulkan, which the macOS host cannot produce and
# guest/'s musl+rust-lld toolchain cannot either.
#
# Output: target/testcomp/limina-testcomp (deploy to a guest with scp, or let the L2 harness
# push it the way it pushes the python probes).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${LIMINA_BUILD_IMAGE:-limina-build:fc43}"
OUT="$ROOT/target/testcomp"
# Cargo's registry + the build tree live in named volumes so a rebuild is incremental; the
# source is bind-mounted read-write because cargo insists on writing Cargo.lock.
CARGO_VOL="${LIMINA_TESTCOMP_CARGO_VOL:-limina-testcomp-cargo}"
TARGET_VOL="${LIMINA_TESTCOMP_TARGET_VOL:-limina-testcomp-target}"

"$ROOT/scripts/build-image.sh"

mkdir -p "$OUT"

# rustup rather than the distro rust: smithay's MSRV moves faster than Fedora's toolchain,
# and pinning here keeps the build reproducible independent of the base image's rust version.
# 8 GiB and 4 CPUs: at the default allocation rustc is SIGKILLed part-way through codegen for
# `ash` (a single enormous generated lib), which reads as a mysterious "could not compile ash"
# with no diagnostic — the signal is the only clue.
container run --rm --memory 8g --cpus 4 \
    -v "$ROOT/testcomp:/src" \
    -v "$CARGO_VOL:/cargo" \
    -v "$TARGET_VOL:/target" \
    -v "$OUT:/out" \
    -e CARGO_HOME=/cargo \
    -e RUSTUP_HOME=/cargo/rustup \
    -e CARGO_TARGET_DIR=/target \
    "$IMAGE" \
    bash -euo pipefail -c '
        # Build deps beyond the shared image: the compositor links libdrm/libgbm/libinput and
        # needs the Vulkan headers + loader. Installed here rather than baked into the image so
        # the shared image stays the firmware/kernel/mesa toolchain it was built to be.
        dnf -y install --setopt=install_weak_deps=False \
            libdrm-devel mesa-libgbm-devel vulkan-loader-devel vulkan-headers \
            libinput-devel libseat-devel systemd-devel libxkbcommon-devel pixman-devel \
            pkgconf-pkg-config >/dev/null

        # Gate on the TOOLCHAIN, not on /cargo/bin/cargo: that path is only a rustup shim, and
        # with RUSTUP_HOME unset the toolchain it dispatches to lands in the container-local
        # /root/.rustup and vanishes with the container. The shim then survives in the volume
        # and every later run dies with "rustup could not choose a version of cargo to run".
        if [ ! -d /cargo/rustup/toolchains ] || [ -z "$(ls -A /cargo/rustup/toolchains)" ]; then
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
                | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
        fi
        export PATH="/cargo/bin:$PATH"

        cd /src
        cargo build --release
        install -m0755 /target/release/limina-testcomp /out/limina-testcomp
    '

echo "==> built $OUT/limina-testcomp"
file "$OUT/limina-testcomp" || true
