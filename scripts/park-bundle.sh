#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Park a built .dmg in the parking lot with its full provenance.
#
# THE POINT: a built bundle is opaque about what went into it. Nothing inside a .dmg says which
# virglrs, libkrun or mesa revision it carries, or whether the renderer was compiled optimized.
# Hand someone a bundle a week later and "which build is this?" has no answer. This records the
# answer at the one moment it is knowable — the moment the artifact exists.
#
# THE PIN IS A CLAIM; THE CHECKOUT HEAD IS THE FACT. This reads `git rev-parse HEAD` in each
# tree, never `third_party/manifest.toml`. Those two disagreed three times in two days: the
# mesa-cs checkout sat a commit ahead of its pin, the virglrs checkout was moved onto a local
# branch mid-session, and a staged pin named a rev that existed on no remote. A provenance file
# built from the manifest would have recorded all three wrongly and looked authoritative.
#
# It records rather than blocks. A bundle from a dirty tree is a normal, useful thing to hand
# someone mid-investigation, and refusing to park it would mean the interesting builds are
# exactly the ones missing from the index — a pile that looks complete and is not. So a dirty
# tree or an unpushed rev is annotated loudly and parked anyway.
#
# Usage: park-bundle.sh <dmg> [profile]
set -euo pipefail

DMG="${1:?usage: park-bundle.sh <dmg> [profile]}"
PROFILE="${2:-unknown}"
LOT="${LIMINA_PARKING_LOT:-$HOME/Projects/LiminaParkingLot}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)

[ -f "$DMG" ] || { echo "park-bundle: no such dmg: $DMG" >&2; exit 1; }
mkdir -p "$LOT"

# Next free ordinal for today, starting at 0. Scanning the directory rather than keeping a
# counter means a manually deleted or copied-in bundle cannot desynchronise it.
DATE=$(date +%Y-%m-%d)
n=0
while [ -e "$LOT/Limina-$DATE-$n.dmg" ]; do n=$((n + 1)); done
NAME="Limina-$DATE-$n.dmg"

# `cp -c` clones on APFS: instant, and it shares whatever blocks it can with the source.
cp -c "$DMG" "$LOT/$NAME" 2>/dev/null || cp "$DMG" "$LOT/$NAME"

# describe <label> <path> — one row of provenance for one source tree.
#
# Two things beyond the hash, each of which makes the hash alone a lie: a dirty tree is not
# reproducible from its hash, and a rev no remote carries cannot be fetched by anyone else.
#
# THE REACHABILITY CHECK IS LOCAL AND SAYS SO. `git branch -r --contains` knows only the
# remote-tracking refs this clone has actually fetched, and a vendored tree is typically cloned
# once at a pinned rev and never re-fetched — so its refs go stale and the check reports a
# perfectly well-pushed rev as missing. Measured here: imago's `origin/limina` sat at 5f2c0ad
# while HEAD was the pinned 16e9602, and edk2 had never fetched `origin/limina` at all. Calling
# those "UNPUSHED" would flag three rows of eight wrongly, and a file that cries wolf on most of
# its rows is one nobody reads when a genuinely unpushed rev appears. So this reports what it
# actually knows — found in a fetched ref, or not found in one — and never claims more.
describe() {
  local label="$1" path="$2" hash branch dirty reach
  if ! git -C "$path" rev-parse --git-dir >/dev/null 2>&1; then
    # Not vendored on this host. Record the pin and say plainly that nothing verified it —
    # a `heavy = true` dep (the kernel) is skipped by `cargo xtask vendor` unless asked for.
    local pin
    pin=$(awk -v s="[$label]" '$0==s{f=1;next} f&&/^rev *=/{gsub(/[",]/,"");print $3;exit}' \
          "$ROOT/third_party/manifest.toml" 2>/dev/null || true)
    printf '| %s | `%s` | — | — | not vendored here — **manifest pin, unverified** |\n' \
      "$label" "${pin:-<unknown>}"
    return
  fi
  hash=$(git -C "$path" rev-parse HEAD 2>/dev/null || echo '<unknown>')
  branch=$(git -C "$path" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')
  [ "$branch" = "HEAD" ] && branch='(detached)'
  if [ -n "$(git -C "$path" status --porcelain 2>/dev/null)" ]; then
    dirty='**DIRTY**'
  else
    dirty='clean'
  fi
  if [ -z "$(git -C "$path" for-each-ref --format='x' refs/remotes 2>/dev/null)" ]; then
    reach='no remote refs in this clone'
  elif [ -n "$(git -C "$path" branch -r --contains "$hash" 2>/dev/null)" ]; then
    reach='in a fetched remote ref'
  else
    reach='**not in any fetched ref** (may be unpushed, or refs stale — `git fetch` to tell)'
  fi
  printf '| %s | `%s` | %s | %s | %s |\n' "$label" "$hash" "$branch" "$dirty" "$reach"
}

INDEX="$LOT/INDEX.md"
if [ ! -f "$INDEX" ]; then
  cat > "$INDEX" <<'HEADER'
# Limina bundle parking lot

Every `.dmg` `cargo xtask app` produces, with the revisions it was actually built from.

**Hashes are read from each checkout's `HEAD` at build time, never from
`third_party/manifest.toml`** — the manifest records what a tree *should* be on, which is not
evidence of what it *was* on.

A row marked **DIRTY** was built from a tree with uncommitted changes and is **not reproducible
from its hash alone**. The reachability column is a *local* check against the remote-tracking
refs that clone happens to have fetched, so **not in any fetched ref** means "this clone cannot
see it on a remote" — which is either a genuinely unpushed rev or merely stale refs, and only
`git fetch` in that tree distinguishes them.

HEADER
fi

{
  echo "## $NAME"
  echo
  echo "- Built: $(date '+%Y-%m-%d %H:%M:%S %z')"
  echo "- Profile: **$PROFILE** (the renderer is compiled into the worker and inherits this;"
  echo "  a debug build still carries an optimized virglrs via the root Cargo.toml overrides)"
  # `codesign -dv` prints Identifier/TeamIdentifier for a disk image but no Authority line
  # without a deeper verbose level, so read the two fields it does emit.
  sig=$(codesign -dv "$LOT/$NAME" 2>&1 |
        awk -F= '/^TeamIdentifier=/{t=$2} /^Identifier=/{i=$2} END{
          if (i=="") print "<unsigned>"; else print i (t==""?"":" (team " t ")")}')
  echo "- Signed: $sig"
  echo "- Size: $(du -sh "$LOT/$NAME" | awk '{print $1}')"
  echo
  echo '| tree | rev | branch | tree state | reachable |'
  echo '|---|---|---|---|---|'
  describe limina "$ROOT"
  describe libkrun "$ROOT/third_party/libkrun"
  describe virglrs "$ROOT/third_party/virglrs"
  describe imago "$ROOT/third_party/imago"
  describe edk2 "$ROOT/third_party/edk2"
  describe linux "$ROOT/third_party/linux"
  # Neither mesa build lives under third_party/ — anything that walks that directory misses the
  # host renderer and the guest venus mesa entirely.
  describe kosmickrisp /Volumes/mesa-cs/mesa
  describe mesa-guest /Volumes/mesa-cs/mesa-guest
  echo
} >> "$INDEX"

echo "==> parked: $LOT/$NAME"
echo "    index:  $INDEX"
