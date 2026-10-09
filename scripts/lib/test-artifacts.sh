# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Sourced by scripts/test-boot.sh. The suite's test kernel and GOP firmware come from builds that
# only run when asked for (scripts/build-test-kernel.sh, scripts/build-krun-efi.sh), and
# `cargo xtask worktree new` copies them from main's target/. So a checkout can hold a stale one
# without anything saying so, and a suite run on it fails in places that read like regressions.

# test_artifacts_check <root> <edk2-rev> — refuse (non-zero, the reasons on stderr) when the test
# kernel or the GOP firmware under <root>/target is not the one the suite is meant to run.
#   - No custom test kernel: build-test-guest.sh would fall back to libkrunfw's stock kernel,
#     which has no xHCI and no EDID, so those tests fail rather than skip.
#     LIMINA_TEST_STOCK_KERNEL=1 asks for that fallback anyway.
#   - A GOP firmware whose recorded edk2 rev (KRUN_EFI.gop.fd.rev, written by build-krun-efi.sh)
#     is missing or is not <edk2-rev>. LIMINA_FIRMWARE / LIMINA_GOP_FIRMWARE name a firmware
#     explicitly and skip the check; no firmware at all is left to the tests, which say so.
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
  local fd="$root/target/krun-efi/KRUN_EFI.gop.fd"
  if [ -z "${LIMINA_FIRMWARE:-}${LIMINA_GOP_FIRMWARE:-}" ] && [ -f "$fd" ]; then
    local got
    got="$(cat "$fd.rev" 2>/dev/null)"
    if [ "$got" != "$want" ]; then
      cat >&2 <<EOF
error: target/krun-efi/KRUN_EFI.gop.fd was built from edk2 ${got:-at an unrecorded rev},
  and the pin is $want.
  Rebuild it with scripts/build-krun-efi.sh, or name a firmware with LIMINA_FIRMWARE.
EOF
      rc=1
    fi
  fi
  return "$rc"
}
