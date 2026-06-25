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
# The image TAG is exported as LIMINA_BUILD_IMAGE for callers that `source` this; build scripts may
# also just hardcode `limina-build:fc43`. Override the tag with LIMINA_BUILD_IMAGE in the environment.
set -euo pipefail
cd "$(dirname "$0")/.."

export LIMINA_BUILD_IMAGE="${LIMINA_BUILD_IMAGE:-limina-build:fc43}"

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
container build -t "$LIMINA_BUILD_IMAGE" -f scripts/build-image/Containerfile scripts/build-image
echo "==> $LIMINA_BUILD_IMAGE ready"
