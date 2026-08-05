#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Derive the committed guest-mesa patch series (patches/mesa-guest/) from the fork branch.
#
# The guest venus patches used to be a raw-diff POOL (patches/mesa/, three bases, per-consumer
# subsets — retired 2026-08-05, task #11). They now live as commits on liminavm/mesa's
# `limina-guest` branch (third_party/manifest.toml [mesa-guest] pins repo/branch/rev/base), and
# this script regenerates the series from `base..rev` — the fork branch is the source of truth,
# the patch files are a DERIVED, COMMITTED artifact.
#
# Why the artifact is committed (unlike the linux export, which goes to target/): the consumers
# are the guest-mesa RPM builds — scripts/provision/f44/build-mesa-rpm.sh runs INSIDE the F44
# build guest and scripts/build-mesa-rpm.sh inside the fc43 container — and neither environment
# has (or should need) the /Volumes/mesa-cs checkout. They apply whatever is committed under
# patches/mesa-guest/ via the spec (Patch9xxx lines derived from the directory listing, sorted).
#
# So the flow for changing the guest mesa is:
#   1. commit on the fork's limina-guest branch (worktree: /Volumes/mesa-cs/mesa-guest), push
#   2. update the [mesa-guest] rev in third_party/manifest.toml
#   3. run this script; commit the regenerated patches/mesa-guest/ together with the manifest
#   4. rebuild the RPM (LIMINA_REL bump!) and redeliver per docs/images.md
#
# Usage: scripts/export-mesa-guest-patches.sh
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="patches/mesa-guest"
TREE="/Volumes/mesa-cs/mesa-guest"
MANIFEST="third_party/manifest.toml"

# Minimal TOML read: the [mesa-guest] section's scalar string fields.
manifest_field() {
    awk -v key="$1" '
        /^\[/ { in_sec = ($0 ~ /^\[mesa-guest\]/) }
        in_sec && $1 == key { gsub(/^[^"]*"|"[^"]*$/, ""); print; exit }
    ' "$MANIFEST"
}
REV="$(manifest_field rev)"
BASE="$(manifest_field base)"
[ -n "$REV" ] && [ -n "$BASE" ] || { echo "could not read the [mesa-guest] pin from $MANIFEST" >&2; exit 1; }

if [ ! -d "$TREE" ]; then
    scripts/ensure-mesa-cs.sh
fi
[ -d "$TREE" ] || { echo "no worktree at $TREE — create it: git -C /Volumes/mesa-cs/mesa worktree add mesa-guest limina-guest" >&2; exit 1; }
git -C "$TREE" rev-parse --verify --quiet "$REV^{commit}" >/dev/null \
    || { echo "pinned rev $REV not present in $TREE — fetch/pull the fork first" >&2; exit 1; }

# Export from the PINNED REV (not the branch tip) so the committed series always matches the
# manifest. --no-signature/--zero-commit keep regeneration deterministic across git versions.
mkdir -p "$OUT"
rm -f "$OUT"/*.patch
git -C "$TREE" format-patch --no-signature --zero-commit --output-directory "$(pwd)/$OUT" "$BASE..$REV" >/dev/null

echo "==> exported $BASE..${REV:0:12} into $OUT/:"
ls -1 "$OUT"/*.patch
