#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot a STOCK Fedora 44 (4 KiB) guest on the coexist GPU with vrend/virgl enabled (zink-on-KK),
# headless (capture sink) + NAT, so we can SSH in and check the guest bound the virgl driver.
# Sets the worker runtime env so vrend's host GL resolves to our zink-on-KK Mesa (the worker is
# codesigned with allow-dyld-environment-variables, so DYLD_* survives). See README.md / step 2.
#
# Production will bundle these dylibs in the .app (@rpath) and won't need DYLD_* — this is the
# dev/test path. Logs the worker to spikes/virgl-zink-kk/worker.log.
set -euo pipefail
cd "$(dirname "$0")/../.."
REPO="$PWD"

MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
KK_ICD="$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)"
[ -n "$KK_ICD" ] || { echo "KK ICD not found" >&2; exit 1; }

SRC_IMG="${SRC_IMG:-$REPO/Fedora-Workstation-44.boot.raw}"
DISK="$(mktemp -d)/f44-virgl.raw"
cp -c "$SRC_IMG" "$DISK"
trap 'rm -rf "$(dirname "$DISK")"' EXIT

# Host-side GL provider for vrend = zink-on-KK. These configure the HOST Mesa/loader only.
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$REPO/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib"
export VK_ICD_FILENAMES="$KK_ICD"
export VK_DRIVER_FILES="$KK_ICD"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
export RUST_LOG="${RUST_LOG:-info}"

echo "==> booting stock F44 (coexist GPU: venus + vrend/virgl on zink-on-KK), headless+NAT"
echo "    KK ICD: $KK_ICD"
exec target/debug/limina \
  --firmware target/krun-efi/KRUN_EFI.gop.fd \
  --disk "$DISK" \
  --display-capture /tmp/virgl-f44-cap.png \
  --net --cpus 4 --ram-mib 6144
