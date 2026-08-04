#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Derive the guest-kernel patch series from the liminavm/linux fork.
#
# The kernel patches used to be a committed series (patches/linux/*.patch). They now live as
# commits on the fork's `limina` branch (third_party/manifest.toml pins repo/branch/rev/base),
# and `git format-patch base..branch` regenerates the series on demand — the fork branch is the
# source of truth, the patch files are a build artifact.
#
# Why the artifact still exists: the ENHANCED kernel builds straight from the fork rev and needs
# no patches (scripts/provision/f44/build-kernel-rpm.sh), but the *test* kernels are built at
# other upstream tags (v6.12 4k, v7.1 16k — scripts/build-test-kernel.sh), so they still need
# the changes as a series to apply onto a different base. Those applies are tolerant: a patch
# that doesn't fit its target tag is skipped (PATCHES_OPTIONAL), exactly as before.
#
# Usage: scripts/export-linux-patches.sh [OUTDIR]
#   OUTDIR defaults to target/linux-patches (gitignored).
# Clones the fork into third_party/linux if absent (multi-GB; `heavy = true` in the manifest
# means `cargo xtask vendor` does NOT do this for you).
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${1:-target/linux-patches}"
TREE="third_party/linux"
MANIFEST="third_party/manifest.toml"

# Minimal TOML read: the [linux] section's scalar string fields.
manifest_field() {
    awk -v key="$1" '
        /^\[/ { in_linux = ($0 ~ /^\[linux\]/) }
        in_linux && $1 == key { gsub(/^[^"]*"|"[^"]*$/, ""); print; exit }
    ' "$MANIFEST"
}
REPO_URL="$(manifest_field repo)"
BRANCH="$(manifest_field branch)"
REV="$(manifest_field rev)"
BASE="$(manifest_field base)"
[ -n "$REV" ] && [ -n "$BASE" ] || { echo "could not read the [linux] pin from $MANIFEST" >&2; exit 1; }

if [ ! -d "$TREE/.git" ]; then
    echo "==> cloning the linux fork ($REPO_URL) — third_party/linux is absent"
    echo "    (blobless: history without file contents, fetched on demand)"
    git clone --filter=blob:none --no-checkout "$REPO_URL" "$TREE"
    git -C "$TREE" sparse-checkout set --cone drivers/gpu/drm/virtio mm
fi

if ! git -C "$TREE" cat-file -e "${REV}^{commit}" 2>/dev/null; then
    echo "==> fetching (pinned rev $REV not present)"
    git -C "$TREE" fetch origin --tags
fi

# Verify the pin really sits on top of the recorded base — a mismatch means the manifest and the
# fork have drifted, and every consumer of this series would silently build something else.
git -C "$TREE" merge-base --is-ancestor "$BASE" "$REV" 2>/dev/null \
    || { echo "ERROR: manifest base $BASE is not an ancestor of rev $REV — fix $MANIFEST" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT"
git -C "$TREE" format-patch --no-signature --zero-commit --output-directory \
    "$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")" "$BASE..$REV" >/dev/null

n=$(find "$OUT" -name '*.patch' | wc -l | tr -d ' ')
echo "==> exported $n patches from $BRANCH ($BASE..${REV:0:12}) to $OUT"
ls "$OUT"
