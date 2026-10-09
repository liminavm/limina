#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
#
# Test scripts/lib/test-artifacts.sh: the suite refuses a test kernel or firmware it was not meant
# to run on.
#
# Neither the custom test kernel nor the GOP firmware comes from a normal build, so a checkout can
# hold a stale one without anything saying so: the stock-kernel fallback failed 13 xHCI/EDID/display
# tests, and a pre-vTPM firmware failed the TPM event-log test, in a run that read like a regression.
#
# Run it directly: scripts/tests/test-artifacts.sh
set -uo pipefail
# shellcheck disable=SC2046
unset $(git rev-parse --local-env-vars 2>/dev/null)
unset LIMINA_FIRMWARE LIMINA_GOP_FIRMWARE LIMINA_TEST_STOCK_KERNEL

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=scripts/lib/test-artifacts.sh
. "$REPO/scripts/lib/test-artifacts.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PIN=4ab2a7b1267f2a8133eae54a0ddc2b7552533aaa
fails=0
expect() { # <pass|refuse> <name> [VAR=value...]: run the check on $TMP/root, in a subshell
  local want=$1 name=$2; shift 2
  if ([ $# -eq 0 ] || export "$@"; test_artifacts_check "$TMP/root" "$PIN") 2>"$TMP/err"; then got=pass; else got=refuse; fi
  if [ "$got" = "$want" ]; then echo "ok   $name"; else echo "FAIL $name: wanted $want, got $got"; cat "$TMP/err"; fails=$((fails + 1)); fi
}
fresh() { rm -rf "$TMP/root"; mkdir -p "$TMP/root/target/test-guest/kernel" "$TMP/root/target/krun-efi"; }
kernel() { : > "$TMP/root/target/test-guest/kernel/Image"; }
fd() { : > "$TMP/root/target/krun-efi/KRUN_EFI.gop.fd"; }
stamp() { echo "$1" > "$TMP/root/target/krun-efi/KRUN_EFI.gop.fd.rev"; }

fresh; kernel; fd; stamp "$PIN"
expect pass "custom kernel and firmware built at the pin"

fresh; fd; stamp "$PIN"
expect refuse "no custom test kernel"
expect pass "no custom test kernel, stock kernel asked for" LIMINA_TEST_STOCK_KERNEL=1

fresh; kernel; fd
expect refuse "firmware with no recorded rev"
expect pass "firmware with no recorded rev, firmware named" LIMINA_FIRMWARE=/elsewhere.fd
expect pass "firmware with no recorded rev, GOP firmware named" LIMINA_GOP_FIRMWARE=/elsewhere.fd

fresh; kernel; fd; stamp 056015b20795bab0fa0f266fbc2759f0
expect refuse "firmware built at another rev"

fresh; kernel
expect pass "no firmware at all (the tests' own fallback says so)"

fresh; kernel; fd; stamp "$PIN"; : > "$TMP/root/target/krun-efi/KRUN_EFI.gop.fd.rev"
expect refuse "an empty recorded rev"

[ "$fails" -eq 0 ] && echo "test-artifacts: all cases pass" || { echo "test-artifacts: $fails case(s) FAILED"; exit 1; }
