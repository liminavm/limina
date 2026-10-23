#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the enhanced tier's 16 KiB kernel RPM (and its -devel) in the unified `limina-build`
# container — the same image every other Linux build uses. It runs
# scripts/provision/f44/build-kernel-rpm.sh unchanged; the same script runs in a booted F44 guest
# too, which is the better place when you want the dogfood signal of a guest building its own
# kernel. The container is an F44 aarch64 system, which is all the RPM needs: rpmbuild stamps
# `.fc44`, and the base config is F44's kernel-core's.
#
# Usage: scripts/build-enhanced-kernel.sh
#   FEDORA_REL=45   build for another Fedora (also rebuild the image: FORCE=1 build-image.sh)
#   JOBS=8 MEM=12g  container resources
#   OUT=<dir>       host output dir (default target/enhanced-rpms/kernel)
#   RELEASE=<n>     the RPM Release, bumped when the content changes at the same kernel version
# Output: $OUT/limina-kernel-16k-*.rpm, its -devel and limina-kernel-16k-source.tar.gz, which
# scripts/package-enhanced.sh takes into the payload.
# Prereq: `container system start`. Network required (dnf, kernel source).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

JOBS="${JOBS:-8}"
MEM="${MEM:-12g}"
OUT="${OUT:-$ROOT/target/enhanced-rpms/kernel}"
mkdir -p "$OUT"

# Sourced: ensures the image and exports $LIMINA_BUILD_IMAGE / $FEDORA_REL.
# shellcheck source=scripts/build-image.sh
. "$ROOT/scripts/build-image.sh"

# One persistent volume for /root, which is where the kernel build keeps what it would want to
# keep: ~/rpmbuild and the kernel source tree. The in-guest path gets that persistence from the
# guest's own disk; here it has to be asked for, and without it every run re-downloads and
# re-compiles from scratch.
VOL="limina-enh-build-fc$FEDORA_REL"
container volume create -s 64g "$VOL" >/dev/null 2>&1 || true

echo "==> 16k kernel RPM in $LIMINA_BUILD_IMAGE — volume $VOL, -j$JOBS, $MEM"
echo "    output: $OUT"

# The kernel source comes from the [linux] pin's fork over the network, unless
# third_party/manifest.local.toml names a local repository for it. Overrides are resolved HERE,
# on the host: the provision scripts inside see only committed values, because the host paths an
# override names mean nothing in the container. The repository goes in read-only, as its git
# directory (a linked worktree's `.git` file names a host path).
LOCAL_MOUNT=()
KERNEL_SRC_ENV=""
# shellcheck source=scripts/lib/manifest.sh
. "$ROOT/scripts/lib/manifest.sh"
LOCAL_LINUX="$(pin_local_repo linux)"
if [ -n "$LOCAL_LINUX" ]; then
  pin_note linux
  LOCAL_MOUNT=(--mount "type=bind,source=$(pin_git_dir "$LOCAL_LINUX"),target=/src/linux.git,readonly")
  KERNEL_SRC_ENV="export FORK_URL=file:///src/linux.git KREV=$(pin_build_rev linux)
          git config --global --add safe.directory /src/linux.git"
fi

# The repo goes in read-only (--mount, since -v has no readonly modifier): the script only READS
# it, and a container writing into the working tree is how a build ends up in `git status`.
# HOME=/root lands on the volume, so rpmbuild state and the kernel tree persist.
container run --rm --cpus "$JOBS" --memory "$MEM" \
    --mount "type=bind,source=$ROOT,target=/repo,readonly" \
    -v "$OUT:/out" \
    -v "$VOL:/root" \
    ${LOCAL_MOUNT[@]+"${LOCAL_MOUNT[@]}"} \
    "$LIMINA_BUILD_IMAGE" bash -euo pipefail -c "
        export HOME=/root OUT=/out
        ${RELEASE:+export RELEASE='$RELEASE'}
        $KERNEL_SRC_ENV
        exec /repo/scripts/provision/f44/build-kernel-rpm.sh
    "

echo "==> 16k kernel built:"
ls -1 "$OUT" | sed 's|^|    |'
