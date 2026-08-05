#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier GUEST mesa for an F43 guest — from the SAME Fedora SRPM base
# the F44 family ships (the "F43 repoint", 2026-08-05; backlog in patches/mesa/README.md).
#
# HISTORY / WHY THE REPOINT: this script used to build a pinned Mesa *main* snapshot
# (26.2.0-devel @ 3515c52) against a rawhide spec, because at authoring time no released
# base carried what we needed. That defeated the F43 track's purpose — "how well does the
# enhanced tier support an older distro" — by routing around the old-distro friction and
# shipping F43 a *different patch subset* than F44 (no 0011/0012/0013, plus the 0016-pre
# backport whose only job was making the old base accept our patches). Now both families
# build from the SAME F44 SRPM (mesa 26.1.5) with the SAME venus-only patch set; what stays
# F43-specific is the BUILD ENVIRONMENT: this runs in an fc43 container against F43's repos,
# so the RPMs link the guest's runtime sonames and the old toolchain (llvm/rust/libdrm age)
# stays a stress we actually exercise.
#
# WHY an RPM (not a sysext, not an /opt prefix): our mesa (26.1.5) differs from stock F43
# (25.2.4), so the megadriver soname differs. A sysext overlay can SHADOW but not REMOVE the
# stock lib -> ABI blend -> mutter's KMS-master EGL probe fails -> gnome-shell crash-loop.
# An RPM REPLACES the stock package; dnf versionlock (applied by install-enhanced.sh) then
# pins it. See memory limina-enh-delivery.
#
# NOTE the version DOWNGRADE on already-enhanced F43 guests: the old track shipped
# 26.2.0-N.limina; this base is 26.1.5 — a lower NEVRA, on purpose, one time.
# install-enhanced.sh handles it (dnf downgrade branch); fresh provisioning from stock
# (25.2.4 -> 26.1.5) is a plain upgrade.
#
# OUR patches (the venus-only set, identical to F44's next respin — see patches/mesa/README.md):
#   0015  venus WSI present-fix (post-rect-clone) — THE black-screen fix. 0009 is the
#         pre-backport variant for old bases; this base (>= 26.1.4 stable) carries the
#         rect-clone upstream, so 0015, never both. [0001/0014 (zink) NOT applied: dead in
#         the guest since drop-guest-zink 2026-08-04 — guest GL rides virgl/vrend.]
#   0011  venus WSI: drop 16-bit-unorm wayland swapchain formats (wgpu ghost-UI).
#   0012  venus: degrade to the stub instance when ring setup fails (stock-kernel GRUB
#         fallback boot must not lose lavapipe).
#   0013  venus: pin the ICD for the TLS destructor (thread-exit SIGSEGV).
#   0016  venus: ring loss -> VK_ERROR_DEVICE_LOST, not abort() (snapshot-resume survival).
#   0017  venus: ring-submit free-list capacity fix (quadratic CPU creep). Superseded
#         upstream by !43229 but NOT yet in the Fedora 26.1.5 tarball (proven at the F44
#         26.1.5-6 respin: still applies clean). Its %prep apply-FAILURE at a future SRPM
#         bump is the retirement signal — drop it then, no MR.
#         [0016-pre NOT needed: the 26.1.4+ stable base already carries the free-list scan.]
# Patches go in via the spec (Patch9xxx), NOT a tolerant pre-apply: a non-applying patch
# fails %prep loudly rather than silently shipping a black-screen mesa.
#
# Usage: scripts/build-mesa-rpm.sh                  # F43 target, F44 26.1.5 SRPM base
#        LIMINA_REL=2 scripts/build-mesa-rpm.sh     # bump Release (REQUIRED for redelivery:
#                                                   # a same-NEVRA rebuild makes dnf silently no-op)
#        PREP_ONLY=1 scripts/build-mesa-rpm.sh      # stop after %prep (fast patch-apply test)
# Output: target/test-guest/mesa/rpm/*.aarch64.rpm  (gitignored)
set -euo pipefail
cd "$(dirname "$0")/.."

# The pinned SRPM base — the koji SRPM the F44 family's shipping mesa is built from.
# Keep in sync with the F44 track (scripts/provision/f44/build-mesa-rpm.sh + docs/images.md
# §Component versions) when that family respins onto a newer base.
MESA_SRPM_URL="${MESA_SRPM_URL:-https://kojipkgs.fedoraproject.org/packages/mesa/26.1.5/1.fc44/src/mesa-26.1.5-1.fc44.src.rpm}"
FEDORA_REL="${FEDORA_REL:-43}"           # build + target guest release (RPMs link its sonames)
# Drivers to %undefine: the TARGET Fedora can't satisfy their BuildRequires AND a VM guest
# has no such GPU. NOTE: must %undefine (not set 0) — the spec keys %{?with_X:...} on
# DEFINED, not value. Why each:
#   d3d12   -> needs DirectX-Headers the F43 repos may lack; Windows D3D12-on-Vulkan, useless in a VM
#   nvk asahi panfrost opencl -> these gate mesa's Rust path (%cargo_generate_buildrequires):
#     they pull crate(...) BuildRequires F43 lacks. None exist in a VM.
#   spirv_tools -> its consumers are the disabled drivers; keeping it forces -Dspirv-tools=enabled
#     with no SPIRV-Tools installed.
#   teflon -> NPU; useless in a VM.
DISABLE_DRIVERS="${DISABLE_DRIVERS:-d3d12 nvk asahi panfrost opencl spirv_tools teflon}"
LIMINA_REL="${LIMINA_REL:-1}"            # Release = <N>.limina; bump for any redelivery (NEVRA must move)
JOBS="${JOBS:-8}"
MEM="${MEM:-12g}"
OUT="target/test-guest/mesa"
RPMOUT="$OUT/rpm"
mkdir -p "$RPMOUT" "$OUT/cache"

command -v container >/dev/null || { echo "Apple 'container' not installed" >&2; exit 1; }

# Persistent build volume (SRPM download + rpmbuild tree survive across iterations).
VOL="limina-mesabuild"
container volume create -s 40g "$VOL" >/dev/null 2>&1 || true

echo "==> building limina F43 mesa RPM set (base SRPM: ${MESA_SRPM_URL##*/}; target=fedora:$FEDORA_REL; -j$JOBS)"

scripts/build-image.sh   # ensure the unified limina-build image (rpm tooling + builddep mesa baked)
container run --rm --cpus "$JOBS" --memory "$MEM" \
  -v "$(pwd)/$OUT:/out" \
  -v "$(pwd)/patches/mesa:/patches" \
  -v "$VOL:/build" \
  limina-build:fc43 bash -euo pipefail -c '
    MESA_SRPM_URL="'"$MESA_SRPM_URL"'"
    DISABLE_DRIVERS="'"$DISABLE_DRIVERS"'"; LIMINA_REL="'"$LIMINA_REL"'"
    PREP_ONLY="'"${PREP_ONLY:-}"'"
    export HOME=/build

    echo "--- [1/5] fetch the pinned F44 mesa SRPM"
    rpmdev-setuptree
    SRCDIR=/build/srpm-f44; mkdir -p "$SRCDIR"; cd "$SRCDIR"
    SRPM_FILE="${MESA_SRPM_URL##*/}"
    if [ ! -f "$SRPM_FILE" ]; then
      # different base than any cached one -> clear the dir so stale spec/sources cannot blend
      rm -f ./*; curl -fLO "$MESA_SRPM_URL"
    fi
    rpm2cpio "$SRPM_FILE" | cpio -idmu --quiet
    MESA_VER=$(rpm -q --qf "%{VERSION}" -p "$SRPM_FILE"); echo "    mesa version: $MESA_VER"
    cp -f ./* "$HOME/rpmbuild/SOURCES/" 2>/dev/null || true
    cp -f mesa.spec "$HOME/rpmbuild/SPECS/mesa.spec"

    echo "--- [2/5] add OUR venus patches + bump Release + F43-buildability surgery"
    cp -f /patches/0015-venus-wsi-present-fix-post-rect-clone.diff \
          /patches/0011-venus-wsi-drop-16bit-unorm-swapchain.diff \
          /patches/0012-venus-degrade-to-stub-instance-when-ring-setup-fails.diff \
          /patches/0013-venus-pin-icd-for-tls-destructor.diff \
          /patches/0016-venus-ring-loss-device-lost-not-abort.diff \
          /patches/0017-venus-fix-ring-submit-freelist-capacity.diff \
          "$HOME/rpmbuild/SOURCES/"
    SPEC="$HOME/rpmbuild/SPECS/mesa.spec"
    LAST_PATCH_LINE=$( { grep -nE "^Patch[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
    [ -n "$LAST_PATCH_LINE" ] || LAST_PATCH_LINE=$( { grep -nE "^Source[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
    ins="Patch9011: 0011-venus-wsi-drop-16bit-unorm-swapchain.diff\nPatch9012: 0012-venus-degrade-to-stub-instance-when-ring-setup-fails.diff\nPatch9013: 0013-venus-pin-icd-for-tls-destructor.diff\nPatch9015: 0015-venus-wsi-present-fix-post-rect-clone.diff\nPatch9016: 0016-venus-ring-loss-device-lost-not-abort.diff\nPatch9017: 0017-venus-fix-ring-submit-freelist-capacity.diff"
    sed -i "${LAST_PATCH_LINE}a ${ins}" "$SPEC"
    # Our patches are plain `git diff` (no mailbox headers); ensure %autosetup uses GNU patch (-p1).
    sed -i -E "s/^%autosetup -S git/%autosetup -p1/" "$SPEC"
    # Release: deterministic "<N>.limina" — outranks stock 25.2.4; bump LIMINA_REL for redelivery.
    sed -i -E "/^%autochangelog/d" "$SPEC"
    sed -i -E "s/^Release:.*/Release:        ${LIMINA_REL}.limina%{?dist}/" "$SPEC"
    # %undefine VM-irrelevant / dep-unavailable drivers. Must land BEFORE the first
    # BuildRequires — the dep-gating %if 0%{?with_X} guards live in the preamble.
    for d in $DISABLE_DRIVERS; do
      echo "    disabling driver: with_$d"
      sed -i "0,/^BuildRequires/s/^BuildRequires/%undefine with_$d\nBuildRequires/" "$SPEC"
    done
    # Slim the VULKAN driver set to what a VM needs AND F43 can build: lavapipe(swrast) +
    # venus(virtio). vulkan_drivers is an EAGERLY-expanded %global (captured the full HW list
    # before our undefines), so override it directly; the HW vulkan drivers sit unconditionally
    # inside the `%if with_vulkan_hw` %files block (panfrost needs Rust), so delete their
    # %files lines. with_vulkan_hw stays ON (it keeps zink in gallium — harmless, matches F44).
    sed -i -E "s|^%global vulkan_drivers .*|%global vulkan_drivers swrast,virtio|" "$SPEC"
    sed -i -E "/libvulkan_radeon\.so/d; /00-radv-defaults/d; /radeon_icd/d; /libvulkan_broadcom\.so/d; /broadcom_icd/d; /libvulkan_freedreno\.so/d; /freedreno_icd/d; /libvulkan_panfrost\.so/d; /panfrost_icd/d; /libvulkan_powervr_mesa\.so/d; /powervr_mesa_icd/d" "$SPEC"
    grep -nE "^Version:|^Release:|^Patch9|^%autosetup|^%undefine|^%global vulkan_drivers" "$SPEC" | head -20

    if [ -n "$PREP_ONLY" ]; then
      echo "--- PREP_ONLY: rpmbuild -bp --nodeps (patch-apply test)"
      rpmbuild -bp --nodeps "$SPEC"
      echo "--- PREP_ONLY: all patches applied cleanly on mesa $MESA_VER"
      exit 0
    fi

    echo "--- [3/5] dnf builddep (against the F43 repos — old-toolchain friction is the point)"
    dnf -y builddep "$SPEC" 2>&1 | tail -3

    echo "--- [4/5] rpmbuild"
    # Clear PRIOR builds from the persistent volume first — otherwise the copy below sweeps
    # stale NEVRAs into /out and the payload trips the installer's mixed-versions guard.
    rm -f "$HOME"/rpmbuild/RPMS/aarch64/*.rpm "$HOME"/rpmbuild/RPMS/noarch/*.rpm
    # If %prep fails on Patch9015, the venus present-fix needs rebasing onto mesa $MESA_VER —
    # rebase it (it is the black-screen fix) and re-run. Do NOT ship without it.
    # If %prep fails on Patch9017, upstream !43229 reached this base — retire 0017 (see README).
    rpmbuild -bb "$SPEC" 2>&1 | tail -40
    mkdir -p /out/rpm
    cp -f "$HOME"/rpmbuild/RPMS/aarch64/*.rpm /out/rpm/ 2>/dev/null || true
    cp -f "$HOME"/rpmbuild/RPMS/noarch/*.rpm  /out/rpm/ 2>/dev/null || true   # any noarch subpackages

    echo "--- [5/5] venus sanity"
    if ! rpm -qlp /out/rpm/mesa-vulkan-drivers-*.rpm 2>/dev/null | grep -q "virtio_icd"; then
      echo "WARN: no virtio_icd (venus) in mesa-vulkan-drivers — check %global vulkan_drivers" >&2
    fi
    echo "=== built RPMs ==="; ls -1 /out/rpm/*.rpm 2>/dev/null | sed "s#/out/#target/test-guest/mesa/#"
  '
echo "==> mesa RPMs in: $RPMOUT/"
ls -la "$RPMOUT"/*.rpm 2>/dev/null || echo "(no RPMs produced -- check the build log above)"
