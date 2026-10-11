#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
#
# Test scripts/park-bundle.sh's name allocation.
#
# The name is stamped into a bundle before it is signed, minutes before its dmg is parked, and
# the running app reports it as the build's identity. Two bundles carrying one name would make
# that identity point at the wrong dmg, so: concurrent reservations never collide, a parked
# image and a pending reservation both hold their name, a released one is free again, and a
# park under a reserved name lands exactly there.
#
# Run it directly: scripts/tests/park-bundle-names.sh
set -uo pipefail
# Run from a git hook, GIT_DIR and GIT_INDEX_FILE point at the repository being committed to, and
# `git -C <dir>` does NOT override them: park-bundle.sh's `git init` of the lot would re-init the
# real repository. Clear them before any git runs.
# shellcheck disable=SC2046
unset $(git rev-parse --local-env-vars 2>/dev/null)

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PARK="$REPO/scripts/park-bundle.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
export LIMINA_PARKING_LOT="$TMP/lot"
DATE=$(date +%Y-%m-%d)

fails=0
check() { # <name> <want> <got>
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: wanted [$2], got [$3]"; fails=$((fails + 1)); fi
}

check "first reservation of the day is ordinal 0" "Limina-$DATE-0.dmg" "$("$PARK" --reserve)"
check "a pending reservation holds its name" "Limina-$DATE-1.dmg" "$("$PARK" --reserve)"
: > "$LIMINA_PARKING_LOT/Limina-$DATE-2.dmg"
check "a parked image holds its name" "Limina-$DATE-3.dmg" "$("$PARK" --reserve)"
"$PARK" --release "Limina-$DATE-1.dmg"
check "a released name is free again" "Limina-$DATE-1.dmg" "$("$PARK" --reserve)"
if "$PARK" --release "../escape.dmg" 2>/dev/null; then got=accepted; else got=refused; fi
check "a release names only the lot's own files" refused "$got"

# Sixteen at once: every one gets a name of its own.
rm -rf "$LIMINA_PARKING_LOT"
for i in $(seq 1 16); do "$PARK" --reserve > "$TMP/r$i" & done
wait
check "concurrent reservations never share a name" 16 "$(cat "$TMP"/r* | sort -u | wc -l | tr -d ' ')"
check "concurrent reservations fill 0..15" "Limina-$DATE-15.dmg" "$(cat "$TMP"/r* | sort -t- -k5 -n | tail -n 1)"

# Parking under a reserved name: the image lands there, the marker goes, the index names it.
rm -rf "$LIMINA_PARKING_LOT"
name=$("$PARK" --reserve)
echo image > "$TMP/built.dmg"
"$PARK" "$TMP/built.dmg" debug "$name" > "$TMP/park.out" 2>&1
check "the image is parked under its reserved name" image "$(cat "$LIMINA_PARKING_LOT/$name" 2>/dev/null)"
if [ -e "$LIMINA_PARKING_LOT/$name.reserved" ]; then got=left; else got=gone; fi
check "parking clears the reservation" gone "$got"
check "the index heading is the name" 1 "$(grep -c "^## $name\$" "$LIMINA_PARKING_LOT/INDEX.md")"
if "$PARK" "$TMP/built.dmg" debug "$name" > "$TMP/park2.out" 2>&1; then got=accepted; else got=refused; fi
check "a name already parked is never reused" refused "$got"
"$PARK" "$TMP/built.dmg" debug > "$TMP/park3.out" 2>&1
check "an unnamed park takes the next free name" image "$(cat "$LIMINA_PARKING_LOT/Limina-$DATE-1.dmg" 2>/dev/null)"

[ "$fails" -eq 0 ] && echo "park-bundle-names: all cases pass" || { echo "park-bundle-names: $fails case(s) FAILED"; exit 1; }
