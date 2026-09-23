#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build (or ensure) the single `limina-build` container image that every Linux build-*.sh uses.
# See scripts/build-image/Containerfile for what's in it and why.
#
# Usage:
#   scripts/build-image.sh            # build only if missing (the no-op fast path build scripts call)
#   FORCE=1 scripts/build-image.sh    # rebuild from scratch (after editing the Containerfile)
#
# Callers `source` this and then use $LIMINA_BUILD_IMAGE, so the tag is named in exactly one
# place. (It used to be spelled `limina-build:fc43` literally in six scripts, which is how a
# Fedora bump would have been six edits and a silent miss.) Override the release with
# FEDORA_REL, or the whole tag with LIMINA_BUILD_IMAGE.
#
#   FEDORA_REL=45 FORCE=1 scripts/build-image.sh    # move the whole toolchain to F45
set -euo pipefail
# Resolve the repo root into a variable and use it absolutely, rather than `cd`-ing: this file
# is SOURCED, so a cd here would silently move the CALLER's working directory (and several
# callers build their bind mounts out of `$(pwd)`). BASH_SOURCE, not $0, for the same reason --
# under `source`, $0 is the caller's path.
_limina_build_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# F44 is the enhanced-tier target: the dogfood images are F44, and the RPMs must link the
# sonames of the release they install onto.
export FEDORA_REL="${FEDORA_REL:-44}"
export LIMINA_BUILD_IMAGE="${LIMINA_BUILD_IMAGE:-limina-build:fc$FEDORA_REL}"

command -v container >/dev/null || {
    echo "Apple 'container' not installed (brew install container)" >&2
    exit 1
}
# The build daemon must be up for build/run; harmless if already started.
container system start >/dev/null 2>&1 || true

if [ "${FORCE:-0}" != 1 ] && container image inspect "$LIMINA_BUILD_IMAGE" >/dev/null 2>&1; then
    # Already present — the common case; stay silent so callers don't get noise every build.
    return 0 2>/dev/null || exit 0
fi

echo "==> building $LIMINA_BUILD_IMAGE (scripts/build-image/Containerfile) — one-time, a few minutes"
container build --build-arg "FEDORA_REL=$FEDORA_REL" \
    -t "$LIMINA_BUILD_IMAGE" \
    -f "$_limina_build_root/scripts/build-image/Containerfile" \
    "$_limina_build_root/scripts/build-image"
echo "==> $LIMINA_BUILD_IMAGE ready"
