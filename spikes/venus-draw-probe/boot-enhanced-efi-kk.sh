#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# ★ THE DEFAULT way to boot/validate an image to the venus desktop. Reach for this FIRST. ★
# Boot an ENHANCED-tier image to the seated venus desktop via its OWN installed 16k kernel (EFI/GOP
# firmware -> GRUB -> installed kernel), with KosmicKrisp as the host Vulkan backend. It boots the
# real installed image ENFORCING — never stamps /.autorelabel (see memory limina-selinux-autorelabel)
# — and tests the image exactly as it would actually run. venus + EFI boot have worked for a long
# time; do NOT reach for the --kernel-inject scripts (boot-seated-kk.sh / run-venus-window.sh) or
# --gpu-software-2d for normal boots — those are fringe modes (see CLAUDE.md).
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
# LIMINA_KK_ICD pins a specific KosmicKrisp ICD json instead of the devenv build — use it to boot
# against the KK that a packaged limina.app actually ships, so a "works here, fails on the dogfood
# Mac" split can be tested without touching that Mac (the app's KK is a different build from the
# devenv one). Its library_path must be absolute, or relative to the json.
ICD="${LIMINA_KK_ICD:-$(ls /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.*.json 2>/dev/null | head -1)}"
[ -n "$ICD" ] || { echo "no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk"; exit 1; }
[ -f "$ICD" ] || { echo "KosmicKrisp ICD not found: $ICD"; exit 1; }
rm -f "$LOG"
# Host KK + zink-on-KK env (matches boot-seated-kk.sh): venus render server -> KK; the coexist
# device's vrend half gets host GL via zink-on-KK (needs the zink-kk Mesa libEGL by bare name on the
# DYLD path + the loader/gallium selectors). /bin/bash strips DYLD_* (SIP); the codesigned worker
# honors it (com.apple.security.cs.allow-dyld-environment-variables).
MESA_PREFIX="${MESA_PREFIX:-/Volumes/mesa-cs/zink-kk-prefix}"
export VK_ICD_FILENAMES="$ICD"
export VK_DRIVER_FILES="$ICD"
export DYLD_FALLBACK_LIBRARY_PATH="$MESA_PREFIX/lib:$ROOT/third_party/epoxy-egl-prefix/lib:/opt/homebrew/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
# Since the 2026-08-05 MTL4 rebase, mesa's zink dlopens "@rpath/libvulkan.1.dylib" and the
# installed libgallium carries no matching LC_RPATH (meson strips build rpaths at install).
# DYLD_LIBRARY_PATH intercepts by leaf name BEFORE rpath resolution — but pointing it at all
# of /opt/homebrew/lib would shadow every Homebrew leaf name for the whole process tree, so
# use a shim dir holding ONLY the Vulkan loader symlink.
mkdir -p "$MESA_PREFIX/vulkan-rpath"
ln -sf /opt/homebrew/lib/libvulkan.1.dylib "$MESA_PREFIX/vulkan-rpath/libvulkan.1.dylib"
export DYLD_LIBRARY_PATH="$MESA_PREFIX/vulkan-rpath${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
export MESA_LOADER_DRIVER_OVERRIDE=zink
export GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH="$MESA_PREFIX/lib"
export EGL_PLATFORM=surfaceless
export VIRGL_LOG_LEVEL=info
# Present config: since libkrun 0110 the windowed worker defaults to fence-accurate
# presents (the June round-27 "FENCE_PRESENT=1 COPY=0" config) — the round-21 copy
# mitigation is redundant with honest flip pacing, so this script no longer forces it.
# Both stay caller-overridable for A/B (LIMINA_PRESENT_COPY=1 / LIMINA_FENCE_PRESENT=0).
for v in LIMINA_PRESENT_COPY LIMINA_FENCE_PRESENT; do
  [ -n "$(eval echo "\${$v:-}")" ] && export "$v"
done
# Profiling passthrough (see memory limina-profiling-playbook): forward stats/capture/knob env to the
# worker so the KK dylib + scanout publisher see them. LIMINA_KK_STATS=1 → once/sec [LIMINA-KK-STATS]
# draws line in the worker log; LIMINA_GLOBAL_SCANOUT=1 → iosdump can read the scanout; MESA_KK_DEBUG,
# the LIMINA_KK_* knobs, and Metal-capture toggles pass through when set.
# LIMINA_GPU_MEM_BUDGET_CENSUS=<secs> is the host half of a leak hunt: it makes the GPU-memory
# ledger log a per-context breakdown plus the Metal refcount census (IOSurface/texture/registry
# alloc-vs-free deltas) on a timer. Pair it with a host footprint sample — a ledger that stays
# FLAT while the process grows means the leak is NOT in a charged allocator, which is itself the
# answer to where to look next. _MIB caps, _SOFT changes what a refusal does (see vkr_budget.h).
for v in LIMINA_KK_STATS LIMINA_KK_RTLOG LIMINA_GLOBAL_SCANOUT MESA_KK_DEBUG MESA_KK_GPU_CAPTURE LIMINA_KK_CAPTURE \
         LIMINA_KK_NOLISTRESTART LIMINA_KK_BOCACHE LIMINA_KK_SLIMPUSH LIMINA_KK_EARLYZ LIMINA_KK_SLIMROOT LIMINA_KK_FASTBIND \
         LIMINA_GPU_MEM_BUDGET_MIB LIMINA_GPU_MEM_BUDGET_CENSUS LIMINA_GPU_MEM_BUDGET_SOFT; do
  [ -n "$(eval echo "\${$v:-}")" ] && export "$v"
done
# Leak-hunt interposer (spikes/vrend-region-leak/iokit-trace). It has to be handed in under a
# NON-DYLD name and renamed here: /bin/bash is SIP-protected, so dyld strips DYLD_* from the
# environment every time a bash script STARTS. A caller that exports DYLD_INSERT_LIBRARIES and
# then execs this script loses it silently — the dylib simply never loads and the trace is empty,
# which reads exactly like "no allocations happened". Setting it here, in the process that execs
# the worker, is the same trick DYLD_FALLBACK_LIBRARY_PATH above relies on.
[ -n "${LIMINA_IOTRACE_DYLIB:-}" ] && export DYLD_INSERT_LIBRARIES="$LIMINA_IOTRACE_DYLIB"
for v in LIMINA_IOTRACE_DUMP LIMINA_IOTRACE_DEPTH LIMINA_IOTRACE_ALL; do
  [ -n "$(eval echo "\${$v:-}")" ] && export "$v"
done

NET_FLAG=--net
[ "${LIMINA_NET:-1}" = "0" ] && NET_FLAG=
EXTRA_ARGS=()
[ -n "${LIMINA_EXTRA_ARGS:-}" ] && read -ra EXTRA_ARGS <<<"$LIMINA_EXTRA_ARGS"
# LIMINA_BIN runs a different supervisor binary with this same env — in practice the one inside a
# built app bundle (target/limina.app/Contents/MacOS/limina). That matters for anything TCC-gated:
# Accessibility is keyed on the code hash, so a freshly-compiled `target/debug/limina` never has
# the grant and silently loses the capture tap (and with it edge resistance, edge pressure for the
# GNOME hot corner, and — before it moved to the local monitor — the `notch = extend` chrome
# reveal). The bundle keeps its grant across rebuilds because it is signed with a stable identity.
# It is also the only way to test anything that depends on Info.plist. Found the hard way,
# 2026-08-01.
BIN="${LIMINA_BIN:-target/debug/limina}"
"$BIN" --vmm-bin target/debug/limina-vmm \
  --firmware "$FW" \
  --disk "$WORK" --cpus "${LIMINA_CPUS:-6}" --ram-mib "${LIMINA_RAM_MIB:-8192}" $NET_FLAG --window \
  ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
  >"$LOG" 2>&1 &
echo "limina pid=$! (worker log $LOG, disk $WORK, firmware $FW, ICD $ICD)"
wait
