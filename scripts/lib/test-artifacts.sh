# shellcheck shell=bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Sourced by scripts/test-boot.sh. The suite's test kernel and firmware come from builds that
# only run when asked for (scripts/build-test-kernel.sh, scripts/build-firmware.sh), and
# `cargo xtask worktree new` copies them from main's target/. So a checkout can hold a stale one
# without anything saying so, and a suite run on it fails in places that read like regressions.

# test_artifacts_check <root> <firmware-rev> — refuse (non-zero, the reasons on stderr) when the test
# kernel or the firmware under <root>/target is not the one the suite is meant to run.
#   - No custom test kernel: build-test-guest.sh would fall back to libkrunfw's stock kernel,
#     which has no xHCI and no EDID, so those tests fail rather than skip.
#     LIMINA_TEST_STOCK_KERNEL=1 asks for that fallback anyway.
#   - A firmware whose recorded Cardea rev (efi.fd.rev, written by build-firmware.sh) is missing
#     or is not <firmware-rev>; an edk2 build left from before Cardea records an edk2 rev. LIMINA_FIRMWARE names a firmware explicitly and skips the check; no
#     firmware at all is left to the tests, which say so.
#   - Only an edk2 firmware at its old path (target/krun-efi/KRUN_EFI.gop.fd): the tests would
#     fall back to krunkit's DEBUG blob.
test_artifacts_check() {
  local root="$1" want="$2" rc=0
  if [ ! -f "$root/target/test-guest/kernel/Image" ] && [ "${LIMINA_TEST_STOCK_KERNEL:-}" != 1 ]; then
    cat >&2 <<EOF
error: no custom test kernel at target/test-guest/kernel/Image. Without it the L1 guest boots
  libkrunfw's stock kernel, which has no xHCI or EDID, and those tests fail. Build it with
  scripts/build-test-kernel.sh, or \`cp -c\` the Image* files from another checkout's
  target/test-guest/kernel/. LIMINA_TEST_STOCK_KERNEL=1 runs on the stock kernel anyway.
EOF
    rc=1
  fi
  local fd="$root/target/firmware/efi.fd" old="$root/target/krun-efi/KRUN_EFI.gop.fd"
  if [ -z "${LIMINA_FIRMWARE:-}" ] && [ ! -f "$fd" ] && [ -f "$old" ]; then
    cat >&2 <<EOF
error: the only firmware is an edk2 build at its old path, target/krun-efi/KRUN_EFI.gop.fd.
  The firmware is now Cardea, at target/firmware/efi.fd: build it with scripts/build-firmware.sh.
EOF
    rc=1
  fi
  if [ -z "${LIMINA_FIRMWARE:-}" ] && [ -f "$fd" ]; then
    local got
    got="$(cat "$fd.rev" 2>/dev/null)"
    if [ "$got" != "$want" ]; then
      cat >&2 <<EOF
error: target/firmware/efi.fd was built from ${got:-an unrecorded rev},
  and the Cardea pin is $want.
  Rebuild it with scripts/build-firmware.sh, or name a firmware with LIMINA_FIRMWARE.
EOF
      rc=1
    fi
  fi
  return "$rc"
}
