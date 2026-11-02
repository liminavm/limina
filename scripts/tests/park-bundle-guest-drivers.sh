#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
#
# Test the guest-drivers row of scripts/park-bundle.sh's index.
#
# The bundle carries no guest drivers; the index names the ones packaged beside it, in this
# checkout's target/gpu-drivers, because those are what a guest paired with the build runs. The
# shared [kosmickrisp-guest] checkout is not evidence of them: it can sit anywhere while the
# packaged drivers come from another tree.
#
# Run it directly: scripts/tests/park-bundle-guest-drivers.sh
set -uo pipefail
# See park-bundle-names.sh: a hook's GIT_DIR would make park-bundle.sh re-init the real repository.
# shellcheck disable=SC2046
unset $(git rev-parse --local-env-vars 2>/dev/null)

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PARK="$REPO/scripts/park-bundle.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
export LIMINA_PARKING_LOT="$TMP/lot"
echo image > "$TMP/built.dmg"

fails=0
check() { # <name> <want> <got>
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: wanted [$2], got [$3]"; fails=$((fails + 1)); fi
}
row() { grep '^| kosmickrisp-guest' "$LIMINA_PARKING_LOT/INDEX.md" | tail -n 1; }

# Drivers packaged for wire 5 from a known rev: the row names that rev and the wire.
rev=0123456789abcdef0123456789abcdef01234567
mkdir -p "$TMP/gpu-drivers/limina-gpu-drivers/drivers/opt/limina/5"
echo "$rev" > "$TMP/gpu-drivers/limina-gpu-drivers/drivers/opt/limina/5/MESA_REV"
LIMINA_GPU_DRIVERS_DIR="$TMP/gpu-drivers" "$PARK" "$TMP/built.dmg" debug > "$TMP/park1.out" 2>&1
r=$(row)
case "$r" in *"\`$rev\`"*) got=named ;; *) got="not named: $r" ;; esac
check "the row names the packaged drivers' rev" named "$got"
case "$r" in *"wire 5"*) got=named ;; *) got="not named: $r" ;; esac
check "the row names the packaged drivers' wire" named "$got"

# Nothing packaged: the row says so instead of naming a tree.
LIMINA_GPU_DRIVERS_DIR="$TMP/none" "$PARK" "$TMP/built.dmg" debug > "$TMP/park2.out" 2>&1
r=$(row)
case "$r" in *"none packaged"*) got=said ;; *) got="not said: $r" ;; esac
check "with no drivers packaged the row says so" said "$got"

[ "$fails" -eq 0 ] && echo "park-bundle-guest-drivers: all cases pass" || { echo "park-bundle-guest-drivers: $fails case(s) FAILED"; exit 1; }
