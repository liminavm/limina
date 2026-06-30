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
# Usage: scripts/apply-imago-patch.sh   (or `cargo xtask vendor`, which also handles libkrun)
#   Pristine source comes from the cargo registry cache, or is downloaded from crates.io if absent.
set -euo pipefail
cd "$(dirname "$0")/.."

# Pin the version limina's Cargo.lock resolves for imago (a ^0.2 dep of krun-devices). If the
# lock bumps imago, update VER here and re-export the series; a mismatch shows up loudly as
# "Patch `imago vX` was not used in the crate graph".
VER="0.2.2"
DEST="third_party/imago"
PATCHES="$PWD/patches/imago"

# Get the pristine source. Prefer the cargo registry cache; otherwise download the .crate
# directly from crates.io. The download path is what makes this work on a FRESH clone: limina's
# root Cargo.toml has `[patch.crates-io] imago = { path = "third_party/imago" }`, so until this
# vendored tree exists EVERY cargo command (including `cargo fetch`) fails to parse the manifest —
# we can't rely on cargo having populated the registry first.
PRISTINE=""
for d in "$HOME"/.cargo/registry/src/*/imago-"$VER"; do
    [ -d "$d" ] && { PRISTINE="$d"; break; }
done

rm -rf "$DEST"
if [ -n "$PRISTINE" ]; then
    echo "==> vendoring pristine imago-$VER from the cargo registry"
    cp -R "$PRISTINE" "$DEST"
else
    echo "==> imago-$VER not cached; downloading the .crate from crates.io"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    curl -fsSL -o "$TMP/imago.crate" \
        "https://static.crates.io/crates/imago/imago-$VER.crate" \
        || { echo "failed to download imago-$VER.crate from crates.io" >&2; exit 1; }
    tar -xzf "$TMP/imago.crate" -C "$TMP"
    mkdir -p "$(dirname "$DEST")"
    mv "$TMP/imago-$VER" "$DEST"
fi
rm -f "$DEST/.cargo-ok" "$DEST/.cargo_vcs_info.json"

git -C "$DEST" init -q
git -C "$DEST" add -A
git -C "$DEST" -c user.name=limina -c user.email=limina@localhost \
    commit -q -m "imago $VER — pristine vendored copy (crates.io)"

echo "==> applying $(ls "$PATCHES"/*.patch | wc -l | tr -d ' ') patch(es)"
git -C "$DEST" -c user.name=limina -c user.email=limina@localhost am "$PATCHES"/*.patch

echo "==> done: $DEST now at"
git -C "$DEST" log --oneline -2
