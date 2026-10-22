#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
#
# Test the Vulkan driver list scripts/provision/install-enhanced.sh pins.
#
# The installer pins VK_DRIVER_FILES to venus so clients do not pick lavapipe. The vendor GPU
# drivers (guest/gpu-drivers/install.sh) link their ICDs into /etc/vulkan/icd.d as limina-*.json
# and add them to that pin; an installer re-run that rewrites the pin with venus alone hides them
# from every client and from zink. So: the vendor ICDs stay in the list, ahead of venus as the
# GPU driver package puts them, and a guest without them pins venus alone.
#
# Run it directly: scripts/tests/install-enhanced-vk-drivers.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
VENUS=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json

fails=0
check() { # <name> <want> <got>
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: wanted [$2], got [$3]"; fails=$((fails + 1)); fi
}
# Sourced, the installer defines its helpers and stops before any install step. A subshell keeps
# an installer that does not stop from taking this shell with it.
list() { # <venus icd> <icd dir>
  (. "$REPO/scripts/provision/install-enhanced.sh" && vk_driver_files "$1" "$2") 2>/dev/null
}

mkdir -p "$TMP/none" "$TMP/vendor"
check "a guest without vendor drivers pins venus alone" "$VENUS" "$(list "$VENUS" "$TMP/none")"
: > "$TMP/vendor/limina-5.json"
: > "$TMP/vendor/limina-6.json"
: > "$TMP/vendor/other.json"
check "the vendor ICDs stay in the pin, ahead of venus" \
  "$TMP/vendor/limina-5.json:$TMP/vendor/limina-6.json:$VENUS" "$(list "$VENUS" "$TMP/vendor")"
check "without venus the vendor ICDs are the pin" \
  "$TMP/vendor/limina-5.json:$TMP/vendor/limina-6.json" "$(list "" "$TMP/vendor")"
check "with neither there is no pin" "" "$(list "" "$TMP/none")"

[ "$fails" = 0 ] || { echo "$fails check(s) failed"; exit 1; }
echo "all checks passed"
