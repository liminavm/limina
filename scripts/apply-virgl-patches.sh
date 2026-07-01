#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Apply limina's virglrenderer patch series onto the vendored checkout.
#
# third_party/virglrenderer is a from-source checkout (gitignored). Our changes — the whole
# macOS/venus enablement + zero-copy IOSurface scanout stack + the vrend/vkr fixes — live as a
# git format-patch series under patches/virglrenderer/ so they survive a re-clone and stay
# reviewable. This script resets the checkout to the recorded upstream base and applies the
# series. Run it after (re)cloning virglrenderer (cargo xtask vendor does the clone), or to
# verify the series still applies.
#
# Usage: scripts/apply-virgl-patches.sh
#   Requires third_party/virglrenderer to be a clean git checkout that contains UPSTREAM_BASE
#   (fetched from https://gitlab.freedesktop.org/virgl/virglrenderer.git).
set -euo pipefail
cd "$(dirname "$0")/.."

VG="third_party/virglrenderer"
PATCHES="$PWD/patches/virglrenderer"
BASE="$(head -n1 "$PATCHES/UPSTREAM_BASE")"

[ -d "$VG/.git" ] || { echo "no git checkout at $VG" >&2; exit 1; }

if ! git -C "$VG" cat-file -e "$BASE^{commit}" 2>/dev/null; then
    echo "upstream base $BASE not present in $VG (wrong/old checkout?)" >&2
    echo "  fetch it: git -C $VG fetch origin $BASE" >&2
    exit 1
fi

if [ -n "$(git -C "$VG" status --porcelain)" ]; then
    echo "$VG has uncommitted changes; refusing to reset. Stash/commit them first." >&2
    exit 1
fi

echo "==> resetting $VG to $BASE"
git -C "$VG" checkout -q "$BASE"

echo "==> applying $(ls "$PATCHES"/*.patch | wc -l | tr -d ' ') patch(es)"
git -C "$VG" am "$PATCHES"/*.patch

echo "==> done: $VG now at"
git -C "$VG" log --oneline -1
echo "    build it with scripts/build-virglrenderer.sh (needs epoxy-egl + zink-on-KK Mesa; see that script)"
