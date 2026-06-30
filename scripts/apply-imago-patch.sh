#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Vendor + patch imago under third_party/imago.
#
# imago is a crates.io dependency of libkrun's block device (krun-devices). We need a small
# behavioral change (discard must not truncate the backing file — see patches/imago/README.md),
# so we vendor the pristine 0.2.2 source as a gitignored from-source checkout and carry our
# change as a git format-patch series, exactly like third_party/libkrun. limina's root
# Cargo.toml then overrides the registry crate with this path via [patch.crates.io].
#
# Usage: scripts/apply-imago-patch.sh
#   Requires the pristine imago-0.2.2 in the local cargo registry (run `cargo fetch` if absent).
set -euo pipefail
cd "$(dirname "$0")/.."

# Pin the version limina's Cargo.lock resolves for imago (a ^0.2 dep of krun-devices). If the
# lock bumps imago, update VER here and re-export the series; a mismatch shows up loudly as
# "Patch `imago vX` was not used in the crate graph".
VER="0.2.2"
DEST="third_party/imago"
PATCHES="$PWD/patches/imago"

# Find the pristine source in the cargo registry cache.
PRISTINE=""
for d in "$HOME"/.cargo/registry/src/*/imago-"$VER"; do
    [ -d "$d" ] && { PRISTINE="$d"; break; }
done
[ -n "$PRISTINE" ] || {
    echo "pristine imago-$VER not found in the cargo registry; run 'cargo fetch' first" >&2
    exit 1
}

echo "==> vendoring pristine imago-$VER from $PRISTINE"
rm -rf "$DEST"
cp -R "$PRISTINE" "$DEST"
rm -f "$DEST/.cargo-ok" "$DEST/.cargo_vcs_info.json"

git -C "$DEST" init -q
git -C "$DEST" add -A
git -C "$DEST" -c user.name=limina -c user.email=limina@localhost \
    commit -q -m "imago $VER — pristine vendored copy (crates.io)"

echo "==> applying $(ls "$PATCHES"/*.patch | wc -l | tr -d ' ') patch(es)"
git -C "$DEST" -c user.name=limina -c user.email=limina@localhost am "$PATCHES"/*.patch

echo "==> done: $DEST now at"
git -C "$DEST" log --oneline -2
