#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Park a built .dmg in the parking lot with its full provenance.
#
# THE POINT: a bundle says little about what went into it. It carries the revisions compiled
# into it and its own parking name, but not whether a tree was dirty, which branch it sat on, or
# the guest mesa beside it. Hand someone a bundle a week later and "which build is this?" needs
# this index to answer. It records the answer at the one moment it is knowable — the moment the
# artifact exists — under the name the bundle itself reports.
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
# THE NAME IS CHOSEN BEFORE THE BUNDLE IS BUILT. `scripts/build-app.sh` reserves it first and
# stamps it into the bundle (Contents/Resources/bundle-id), so the running app can say which dmg
# it came from (the identity's `limina_bundle`, crates/limina/src/bundle_id.rs). The stamp has to
# be in before the bundle is signed, which is long before the dmg exists to be parked.
#
# Usage:
#   park-bundle.sh --reserve          claim the next free name and print it (Limina-<date>-<n>.dmg)
#   park-bundle.sh --release <name>   give back a reservation whose bundle was never finished
#   park-bundle.sh <dmg> [profile] [name] [app]
#                                     park <dmg> as <name> (reserved here when omitted); <app> is
#                                     the bundle inside it, whose supervisor CDHash is recorded
set -euo pipefail

LOT="${LIMINA_PARKING_LOT:-$HOME/Projects/LiminaParkingLot}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)

# A reservation is a `<name>.reserved` marker beside where the dmg will go, and a name is taken
# while either exists. The marker is created under noclobber, which is O_EXCL: of two builds
# racing for one ordinal exactly one creates it and the other moves on to the next. A build
# spends minutes between reserving and parking, so a scan-then-copy would hand both the same
# name. Scanning the directory rather than keeping a counter means a manually deleted or
# copied-in bundle cannot desynchronise it.
reserve() {
  mkdir -p "$LOT" || return 1
  local date n=0 name
  date=$(date +%Y-%m-%d)
  while :; do
    name="Limina-$date-$n.dmg"
    if [ ! -e "$LOT/$name" ]; then
      if (set -C; : > "$LOT/$name.reserved") 2>/dev/null; then
        echo "$name"
        return
      fi
      # Lost a race only if the marker is now there; otherwise the lot refuses every create, and
      # trying the next ordinal would spin forever.
      [ -e "$LOT/$name.reserved" ] || { echo "park-bundle: cannot reserve in $LOT" >&2; return 1; }
    fi
    n=$((n + 1))
  done
}

# Only names of the lot's own shape: the argument becomes a path inside it.
lot_name() {
  case "$1" in
    Limina-*.dmg) [[ "$1" != */* ]] ;;
    *) false ;;
  esac
}

case "${1:-}" in
  --reserve) reserve; exit 0 ;;
  --release)
    name="${2:?usage: park-bundle.sh --release <name>}"
    lot_name "$name" || { echo "park-bundle: not a parking-lot name: $name" >&2; exit 1; }
    rm -f "$LOT/$name.reserved"
    exit 0
    ;;
esac

DMG="${1:?usage: park-bundle.sh <dmg> [profile] [name] [app]}"
PROFILE="${2:-unknown}"
NAME="${3:-}"
APP="${4:-}"
# shellcheck source=scripts/lib/manifest.sh
. "$ROOT/scripts/lib/manifest.sh"

[ -f "$DMG" ] || { echo "park-bundle: no such dmg: $DMG" >&2; exit 1; }
mkdir -p "$LOT"

# The lot is a git repo tracking INDEX.md and nothing else — the images are ~80-250 MB each and
# have no business in history, but the provenance is worth keeping and diffing. Set up here
# rather than by hand so a fresh machine gets it right without anyone remembering to.
if ! git -C "$LOT" rev-parse --git-dir >/dev/null 2>&1; then
  git -C "$LOT" init -q
  printf '# The images themselves: large, opaque, and recoverable by rebuilding at the\n# revisions INDEX.md records. Only the provenance is tracked.\n*.dmg\n*.app/\n' \
    > "$LOT/.gitignore"
  git -C "$LOT" add .gitignore
  git -C "$LOT" -c commit.gpgsign=false commit -q -m 'parking lot: track the index, never the images' || true
fi

# The name the bundle was stamped with, or a fresh one for a dmg nobody stamped. Never an
# existing image: a stamp naming one dmg must not end up on another.
[ -n "$NAME" ] || NAME=$(reserve)
lot_name "$NAME" || { echo "park-bundle: not a parking-lot name: $NAME" >&2; exit 1; }
[ -e "$LOT/$NAME" ] && { echo "park-bundle: $LOT/$NAME already exists" >&2; exit 1; }

# `cp -c` clones on APFS: instant, and it shares whatever blocks it can with the source.
cp -c "$DMG" "$LOT/$NAME" 2>/dev/null || cp "$DMG" "$LOT/$NAME"
rm -f "$LOT/$NAME.reserved"

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
    local rev
    rev=$(pin "$label" rev 2>/dev/null || true)
    printf '| %s | `%s` | — | — | not vendored here — **manifest pin, unverified** |\n' \
      "$label" "${rev:-<unknown>}"
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
  # The supervisor's CDHash, which the running app reports as `limina_cdhash`. The bundle's
  # signature seals Info.plist, Resources and the nested worker into it, so it names the whole
  # bundle, and a build that was never parked has one too.
  if [ -n "$APP" ]; then
    cdhash=$(codesign -dvvv "$APP" 2>&1 | sed -n 's/^CDHash=//p')
    echo "- Supervisor CDHash: \`${cdhash:-<none>}\`"
  fi
  echo "- Size: $(du -sh "$LOT/$NAME" | awk '{print $1}')"
  # A checkout override is a tree the manifest does not name at all; say so, with the path,
  # since the table below shows only its HEAD.
  for dep in libkrun virglrs imago edk2 linux kosmickrisp mesa-guest; do
    c="$(pin_override "$dep" checkout)"
    [ -n "$c" ] && echo "- **Local checkout override**: $dep built from \`$c\`, not the manifest's tree"
  done
  echo
  echo '| tree | rev | branch | tree state | reachable |'
  echo '|---|---|---|---|---|'
  describe limina "$ROOT"
  # pin_tree: the tree each dependency is on this checkout, overrides included. Neither mesa
  # build lives under third_party/ — anything that walks that directory misses the host renderer
  # and the guest venus mesa entirely.
  for dep in libkrun virglrs imago edk2 linux kosmickrisp mesa-guest; do
    describe "$dep" "$(pin_tree "$dep")"
  done
  echo
} >> "$INDEX"

# One commit per bundle, so the index has a history worth diffing rather than one ever-growing
# untracked file. Non-fatal: a build that succeeded must not be reported as failed because a
# bookkeeping commit did not land.
if git -C "$LOT" rev-parse --git-dir >/dev/null 2>&1; then
  git -C "$LOT" add INDEX.md .gitignore 2>/dev/null || true
  git -C "$LOT" -c commit.gpgsign=false commit -q -m "$NAME" 2>/dev/null ||
    echo "note: nothing committed to the parking lot index" >&2
fi

echo "==> parked: $LOT/$NAME"
echo "    index:  $INDEX"
