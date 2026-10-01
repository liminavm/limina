#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
#
# Test scripts/lib/manifest.sh against synthetic manifests.
#
# Every Linux and Mesa build reads its pin through this, so a misread is a build of the wrong
# revision that nothing downstream names. The cases that matter: an override never changes a
# committed value, a value is taken only from its own table, and comments and quoting do not
# leak into what is read.
#
# Run it directly: scripts/tests/manifest-reader.sh
set -uo pipefail
# Run from a git hook, GIT_DIR and GIT_INDEX_FILE point at the repository being committed to, and
# `git -C <dir>` does NOT override them: the scratch repo below would be a re-init of the real
# one (core.bare=true in the config every worktree shares) and its commit a commit on the real
# branch. Both happened. Clear them before any git runs.
# shellcheck disable=SC2046
unset $(git rev-parse --local-env-vars)

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/manifest.toml" <<'EOF'
# A comment naming [libkrun] and rev = "not-this".
[libkrun]
repo = "https://example.invalid/libkrun.git"
branch = "limina"
rev = "aaaa"   # trailing comment
heavy = true # bare value with a comment

[kosmickrisp]
rev = "bbbb"
tree = "/Volumes/mesa-cs/mesa"

[mesa-guest]
rev = "cccc"
base = "mesa-26.1.7"

[libkrun-extra]
rev = "dddd"
EOF

cat > "$TMP/manifest.local.toml" <<'EOF'
[libkrun]
source = "~/src/libkrun"

[kosmickrisp]
checkout = "elsewhere/mesa"
EOF

# A git repo for the checkout override, so pin_build_rev has a HEAD to read.
mkdir -p "$TMP/root/elsewhere/mesa"
git -C "$TMP/root/elsewhere/mesa" init -q
git -C "$TMP/root/elsewhere/mesa" -c user.name=t -c user.email=t@t commit -q --allow-empty -m x
HEAD_REV="$(git -C "$TMP/root/elsewhere/mesa" rev-parse HEAD)"

LIMINA_ROOT="$TMP/root" LIMINA_MANIFEST="$TMP/manifest.toml" \
LIMINA_MANIFEST_LOCAL="$TMP/manifest.local.toml"
export LIMINA_ROOT LIMINA_MANIFEST LIMINA_MANIFEST_LOCAL
# shellcheck source=scripts/lib/manifest.sh
. "$REPO/scripts/lib/manifest.sh"

pass=0
fail=0
check() {   # check <name> <expected> <actual>
    if [ "$2" = "$3" ]; then
        printf 'ok   %s\n' "$1"; pass=$((pass + 1))
    else
        printf 'FAIL %s: expected [%s], got [%s]\n' "$1" "$2" "$3"; fail=$((fail + 1))
    fi
}

check "string value, trailing comment"      aaaa "$(pin libkrun rev)"
check "bare value, trailing comment"        true "$(pin libkrun heavy)"
check "section match is exact, not prefix"  dddd "$(pin libkrun-extra rev)"
check "hyphenated section"                  mesa-26.1.7 "$(pin mesa-guest base)"
check "missing key is empty"                "" "$(pin mesa-guest branch)"
check "missing section is empty"            "" "$(pin nope rev)"
check "override leaves the pin alone"       aaaa "$(pin libkrun rev)"
check "source override, ~ expanded"         "$HOME/src/libkrun" "$(pin_override libkrun source)"
check "checkout override, relative"         "$TMP/root/elsewhere/mesa" "$(pin_override kosmickrisp checkout)"
check "no override is empty"                "" "$(pin_override mesa-guest checkout)"
check "tree: default under third_party"     "$TMP/root/third_party/libkrun" "$(pin_tree libkrun)"
check "tree: checkout beats manifest tree"  "$TMP/root/elsewhere/mesa" "$(pin_tree kosmickrisp)"
check "local repo: source"                  "$HOME/src/libkrun" "$(pin_local_repo libkrun)"
check "local repo: checkout"                "$TMP/root/elsewhere/mesa" "$(pin_local_repo kosmickrisp)"
check "local repo: none"                    "" "$(pin_local_repo mesa-guest)"
check "build rev: pin when no checkout"     aaaa "$(pin_build_rev libkrun)"
check "build rev: checkout HEAD"            "$HEAD_REV" "$(pin_build_rev kosmickrisp)"
check "git dir of a checkout"               "$(cd "$TMP/root/elsewhere/mesa/.git" && pwd)" \
                                            "$(pin_git_dir "$TMP/root/elsewhere/mesa")"

LIMINA_MANIFEST_LOCAL="$TMP/absent.toml"
check "absent local file: no override"      "" "$(pin_override libkrun source)"

# The real manifest still parses the way its consumers expect.
LIMINA_MANIFEST="$REPO/third_party/manifest.toml"
check "real manifest: mesa-guest tree"      /Volumes/mesa-cs/mesa-guest "$(pin mesa-guest tree)"
check "real manifest: linux is heavy"       true "$(pin linux heavy)"
[ -n "$(pin edk2 rev)" ] && check "real manifest: edk2 rev present" 1 1 \
    || check "real manifest: edk2 rev present" nonempty ""

echo
echo "manifest reader tests: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
