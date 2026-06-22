#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot a STOCK Fedora 44 (4 KiB) guest WINDOWED on the coexist GPU with vrend/virgl (zink-on-KK),
# + NAT, so a human can drive the desktop (Firefox/WebGL) and we can SSH in to instrument the guest.
# This is the interactive sibling of boot-virgl-guest.sh (which is headless+capture).
#
# Live present-path A/B (the flicker oracle, see crates/limina/src/window.rs ~315-337):
#   touch /tmp/limina-present-copy   -> copy the guest scanout into an immutable 3-deep ring (CA
#                                       can no longer sample a buffer the guest is repainting).
#   rm    /tmp/limina-present-copy   -> back to zero-copy (the default; reproduces the race).
# flicker-present -> touch -> gone -> rm -> returns  == the present-time surface-reuse race convicted.
#
# Sets the worker runtime env so vrend's host GL resolves to our zink-on-KK Mesa (the worker is
# codesigned with allow-dyld-environment-variables, so DYLD_* survives hardened runtime).
# Production will bundle these dylibs in the .app (@rpath) and won't need DYLD_* — this is dev/test.
set -euo pipefail
cd "$(dirname "$0")/../.."
REPO="$PWD"

MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
KK_ICD="$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)"
[ -n "$KK_ICD" ] || { echo "KK ICD not found" >&2; exit 1; }

SRC_IMG="${SRC_IMG:-$REPO/Fedora-Workstation-44.boot.raw}"
# Writable COW clone (APFS) so Firefox/profile writes don't touch the canonical baseline image.
DISK="${DISK:-/tmp/limina-virgl-windowed/f44.raw}"
mkdir -p "$(dirname "$DISK")"
[ -f "$DISK" ] || cp -c "$SRC_IMG" "$DISK"

# Host-side GL provider for vrend = zink-on-KK. These configure the HOST Mesa/loader only.
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$REPO/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib"
export VK_ICD_FILENAMES="$KK_ICD"
export VK_DRIVER_FILES="$KK_ICD"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
export RUST_LOG="${RUST_LOG:-info}"

# Firmware: default is the windowed GOP default (limina resolves it). Set FIRMWARE_FD to force a
# specific firmware — e.g. /opt/homebrew/share/krunkit/KRUN_EFI.silent.fd to skip the GOP
# VirtioGpuDxe early-draw (a diagnostic/workaround for the windowed-GOP firmware-BDS hang).
FW_ARGS=()
[ -n "${FIRMWARE_FD:-}" ] && FW_ARGS=(--firmware "$FIRMWARE_FD")
# CONSOLE_FILE=<path> captures the guest PL011 serial (EDK2/firmware debug + early kernel).
[ -n "${CONSOLE_FILE:-}" ] && FW_ARGS+=(--console "$CONSOLE_FILE")

echo "==> booting stock F44 WINDOWED (coexist GPU: venus + vrend/virgl on zink-on-KK), +NAT"
echo "    KK ICD: $KK_ICD"
echo "    disk:   $DISK (COW clone of $SRC_IMG)"
echo "    firmware: ${FIRMWARE_FD:-<windowed GOP default>}"
echo "    SSH:    ssh -p 2222 claude@127.0.0.1   (once the desktop is up)"
echo "    flicker A/B: touch /tmp/limina-present-copy  (copy ring)  |  rm  (zero-copy)"
exec target/debug/limina \
  --window \
  ${FW_ARGS[@]+"${FW_ARGS[@]}"} \
  --disk "$DISK" \
  --net --cpus "${CPUS:-6}" --ram-mib "${RAM_MIB:-8192}"
