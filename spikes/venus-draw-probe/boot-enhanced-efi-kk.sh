#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot an ENHANCED-tier image to the seated venus desktop via its OWN installed 16k kernel (EFI/GOP
# firmware -> GRUB -> installed kernel), with KosmicKrisp as the host Vulkan backend. Unlike
# boot-seated-kk.sh (which --kernel-boots an external Image-16k with selinux=0 for the dev image),
# this boots the real installed enhanced image ENFORCING — so it never stamps /.autorelabel
# (see memory limina-selinux-autorelabel) and tests the image as it would actually run.
#
# Use for: validating an enhanced.raw built by scripts/provision/f44/* + install-enhanced.sh.
#   LIMINA_DISK=Fedora-Workstation-44.enhanced.raw spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
set -u
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
LOG=/tmp/enhanced-efi-kk-worker.log
WORK="${LIMINA_DISK:?set LIMINA_DISK to the enhanced .raw}"
FW="${LIMINA_FIRMWARE:-target/krun-efi/KRUN_EFI.gop.fd}"
[ -f "$FW" ] || { echo "GOP firmware not found at $FW (build with GOP=1 scripts/build-krun-efi.sh)"; exit 1; }
ICD=$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)
[ -n "$ICD" ] || { echo "no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk"; exit 1; }
rm -f "$LOG"
# Host KK + zink-on-KK env (matches boot-seated-kk.sh): venus render server -> KK; the coexist
# device's vrend half gets host GL via zink-on-KK (needs the zink-kk Mesa libEGL by bare name on the
# DYLD path + the loader/gallium selectors). /bin/bash strips DYLD_* (SIP); the codesigned worker
# honors it (com.apple.security.cs.allow-dyld-environment-variables).
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
export VK_ICD_FILENAMES="$ICD"
export VK_DRIVER_FILES="$ICD"
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$ROOT/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
export VIRGL_LOG_LEVEL=info
export LIMINA_PRESENT_COPY=1   # round-21 flicker mitigation (private scanout copy), default on
# Profiling passthrough (see memory limina-profiling-playbook): forward stats/capture/knob env to the
# worker so the KK dylib + scanout publisher see them. LIMINA_KK_STATS=1 → once/sec [LIMINA-KK-STATS]
# draws line in the worker log; LIMINA_GLOBAL_SCANOUT=1 → iosdump can read the scanout; MESA_KK_DEBUG,
# the LIMINA_KK_* knobs, and Metal-capture toggles pass through when set.
for v in LIMINA_KK_STATS LIMINA_KK_RTLOG LIMINA_GLOBAL_SCANOUT MESA_KK_DEBUG MESA_KK_GPU_CAPTURE LIMINA_KK_CAPTURE \
         LIMINA_KK_NOLISTRESTART LIMINA_KK_BOCACHE LIMINA_KK_SLIMPUSH LIMINA_KK_EARLYZ LIMINA_KK_SLIMROOT LIMINA_KK_FASTBIND; do
  [ -n "$(eval echo "\${$v:-}")" ] && export "$v"
done
NET_FLAG=--net
[ "${LIMINA_NET:-1}" = "0" ] && NET_FLAG=
EXTRA_ARGS=()
[ -n "${LIMINA_EXTRA_ARGS:-}" ] && read -ra EXTRA_ARGS <<<"$LIMINA_EXTRA_ARGS"
target/debug/limina --vmm-bin target/debug/limina-vmm \
  --firmware "$FW" \
  --disk "$WORK" --cpus "${LIMINA_CPUS:-6}" --ram-mib "${LIMINA_RAM_MIB:-8192}" $NET_FLAG --window \
  ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
  >"$LOG" 2>&1 &
echo "limina pid=$! (worker log $LOG, disk $WORK, firmware $FW, ICD $ICD)"
wait
