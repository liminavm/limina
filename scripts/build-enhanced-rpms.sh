#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the enhanced tier (16 KiB kernel RPM, venus mesa RPMs, the agents, the payload) in the
# unified `limina-build` container — the same image every other Linux build uses.
#
# It runs scripts/provision/f44/*.sh, UNCHANGED and unduplicated. Those scripts were written to
# run inside a booted F44 guest, and scripts/provision/f44/README.md gives four reasons for
# that: rpmbuild stamps `.fc44`, the binaries link F44's sonames, `dnf download --source`
# returns F44's own SRPMs, and it is aarch64-native. Every one of those is a property of being
# an F44 aarch64 *system* — none of them is a property of being a VM. The actual reason the
# enhanced tier could not use the container is in the same README: the image was pinned
# `FROM fedora:43` and `FEDORA_REL` only picked a tag, so there was no F44 environment to use.
# There is now (scripts/build-image.sh), so this is the same code in a second place rather than
# a second implementation — which is the point. The in-guest path stays supported and is still
# the better one when you want the dogfood signal of a guest building its own components.
#
# Usage: scripts/build-enhanced-rpms.sh [kernel|mesa|all]      (default: all)
#   FEDORA_REL=45   build for another Fedora (also rebuild the image: FORCE=1 build-image.sh)
#   JOBS=8 MEM=12g  container resources (the kernel build is the heavy one)
#   OUT=<dir>       host output dir (default target/enhanced-rpms)
# Outputs: $OUT/kernel/*.rpm, $OUT/mesa/*.rpm, and for `all` the install-ready $OUT/payload.
# Prereq: `container system start`. Network required (dnf, SRPM downloads, kernel source).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

WHAT="${1:-all}"
case "$WHAT" in kernel|mesa|all) ;; *) echo "usage: $0 [kernel|mesa|all]" >&2; exit 1 ;; esac

JOBS="${JOBS:-8}"
MEM="${MEM:-12g}"
OUT="${OUT:-$ROOT/target/enhanced-rpms}"
mkdir -p "$OUT"

# Sourced: ensures the image and exports $LIMINA_BUILD_IMAGE / $FEDORA_REL.
# shellcheck source=scripts/build-image.sh
. "$ROOT/scripts/build-image.sh"

# One persistent volume for /root, which is where the provision scripts put everything they
# would want to keep: ~/rpmbuild, the kernel source tree, the dnf-downloaded SRPMs. The
# in-guest path gets that persistence from the guest's own disk; here it has to be asked for,
# and without it every run re-downloads and re-compiles from scratch.
VOL="limina-enh-build-fc$FEDORA_REL"
container volume create -s 64g "$VOL" >/dev/null 2>&1 || true

case "$WHAT" in
  kernel) SCRIPTS=("build-kernel-rpm.sh kernel") ;;
  mesa)   SCRIPTS=("build-mesa-rpm.sh mesa") ;;
  all)    SCRIPTS=("build-all.sh payload") ;;
esac

echo "==> enhanced tier ($WHAT) in $LIMINA_BUILD_IMAGE — volume $VOL, -j$JOBS, $MEM"
echo "    output: $OUT"

for entry in "${SCRIPTS[@]}"; do
  set -- $entry
  script="$1"; sub="$2"
  mkdir -p "$OUT/$sub"
  # The repo goes in read-only (--mount, since -v has no readonly modifier): these scripts only
  # READ patches/ and guest/ out of it, and a container writing into the working tree is how a
  # build ends up in `git status`.
  # HOME=/root lands on the volume, so rpmbuild state and the kernel tree persist.
  container run --rm --cpus "$JOBS" --memory "$MEM" \
      --mount "type=bind,source=$ROOT,target=/repo,readonly" \
      -v "$OUT:/out" \
      -v "$VOL:/root" \
      "$LIMINA_BUILD_IMAGE" bash -euo pipefail -c "
          export HOME=/root
          # build-all.sh assembles a payload dir; the component builds take OUT directly.
          if [ '$script' = build-all.sh ]; then
              export PAYLOAD=/out/payload
          else
              export OUT=/out/$sub
          fi
          exec /repo/scripts/provision/f44/$script
      "
done

echo "==> enhanced tier built:"
find "$OUT" -name '*.rpm' -maxdepth 3 2>/dev/null | sed 's|^|    |'
