#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Apply limina's libkrun patch series onto the vendored checkout.
#
# third_party/libkrun is a from-source checkout (gitignored). Our changes live as a
# git format-patch series under patches/libkrun/ so they survive re-clone and stay
# reviewable. This script resets the checkout to the recorded upstream base and applies
# the series. Run it after (re)cloning libkrun, or to verify the series still applies.
#
# Usage: scripts/apply-libkrun-patches.sh
#   Requires third_party/libkrun to be a clean git checkout that contains UPSTREAM_BASE.
set -euo pipefail
cd "$(dirname "$0")/.."

LK="third_party/libkrun"
PATCHES="$PWD/patches/libkrun"
BASE="$(head -n1 "$PATCHES/UPSTREAM_BASE")"

[ -d "$LK/.git" ] || { echo "no git checkout at $LK" >&2; exit 1; }

if ! git -C "$LK" cat-file -e "$BASE^{commit}" 2>/dev/null; then
    echo "upstream base $BASE not present in $LK (wrong/old checkout?)" >&2
    exit 1
fi

if [ -n "$(git -C "$LK" status --porcelain)" ]; then
    echo "$LK has uncommitted changes; refusing to reset. Stash/commit them first." >&2
    exit 1
fi

echo "==> resetting $LK to $BASE"
git -C "$LK" checkout -q "$BASE"

echo "==> applying $(ls "$PATCHES"/*.patch | wc -l | tr -d ' ') patch(es)"
git -C "$LK" am "$PATCHES"/*.patch

echo "==> done: $LK now at"
git -C "$LK" log --oneline -1
