#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build limina's venus/zink mesa as a Fedora RPM set, NATIVELY INSIDE a Fedora 44 guest.
#
# Source-of-truth = THIS guest's OWN mesa SRPM (F44 ships mesa 26.0.x), + our venus patches +
# a Release bump. Because we rebuild the SAME version Fedora ships (not a jump to upstream 26.2),
# the megadriver soname is unchanged, so `dnf install` swaps stock -> ours cleanly with NO ABI
# blend — that is the whole reason the F44 SRPM path is simpler than scripts/build-mesa-rpm.sh,
# which had to manage a 25.x->26.2 soname swap on F43. The installer (install-enhanced.sh) then
# `dnf versionlock`s it so an update can't revert venus.
#
# OUR patches (patches/mesa/, see its README):
#   0001  zink nullDescriptor emulation (MR!37115) — GL correctness on zink.
#   0009  venus WSI present-fix — THE black-screen fix; without it the venus desktop never paints.
#   0010  venus image physdev native modifier — advertises EXT_image_drm_format_modifier.
#   0012  venus: degrade to the stub instance when ring/version setup fails post-connect —
#         without it a 4 KiB-kernel boot (the stock-kernel GRUB fallback!) returns
#         OUT_OF_HOST_MEMORY from vkCreateInstance and the loader kills lavapipe with it.
#   0013  venus: pin the ICD with RTLD_NODELETE when the TLS key is created — without it any
#         thread that used venus SIGSEGVs at exit once the loader dlclose()s the driver
#         (spikes/egl-tsd-repro/).
#   0011  venus WSI: drop the 16-bit-unorm wayland swapchain format. Rgba16Unorm is a legal
#         Vulkan surface format but NOT a wgpu render attachment, so a conventional "first
#         non-sRGB" wgpu client (ghost-ui) that lands on it fails pipeline creation. Matches
#         lavapipe; loses nothing real (host scanout is 8-bit; 16-bit textures still work).
#   0014  zink: fix the multi-context unflushed-batch wait lost-wakeup deadlock — without it
#         a cross-context map (e.g. glReadPixels) can sleep forever on a cnd_broadcast that
#         fired between the unlocked predicate check and the cnd_wait
#         (spikes/venus-replay-zink-hang-2026-07-12/, live-confirmed; unfixed upstream).
# These were authored on mesa 26.1.0; F44 ships 26.0.x, so they may need a rebase. We add them via
# the spec (NOT a tolerant pre-apply) ON PURPOSE: a non-applying patch FAILS %prep loudly, rather
# than silently shipping a present-fix-less (black-screen) mesa.
#
# Usage (in the guest):  scripts/provision/f44/build-mesa-rpm.sh
# Output: $OUT/*.rpm  (default ~/limina-build/mesa)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PATCHES="$REPO/patches/mesa"
OUT="${OUT:-$HOME/limina-build/mesa}"
mkdir -p "$OUT"

command -v rpmbuild >/dev/null || sudo dnf install -y rpm-build rpmdevtools dnf-plugins-core 'dnf-command(builddep)'
rpmdev-setuptree

echo "==> [1/5] fetch THIS guest's mesa SRPM"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
# MESA_SRPM_URL pins the base to a specific SRPM (e.g. koji) instead of whatever version the
# repos serve today — needed when the distro moves the version under us and our patches would
# need a rebase (e.g. 26.1.4 broke 0009; the validated base is 26.1.3).
if [ -n "${MESA_SRPM_URL:-}" ]; then
  curl -fLO "$MESA_SRPM_URL"
else
  dnf download --source mesa
fi
SRPM=$(ls mesa-*.src.rpm | head -1); echo "    SRPM: $SRPM"
rpm2cpio "$SRPM" | cpio -idmu --quiet
MESA_VER=$(rpm -q --qf "%{VERSION}" -p "$SRPM"); echo "    mesa version: $MESA_VER"
cp -f ./* "$HOME/rpmbuild/SOURCES/" 2>/dev/null || true
cp -f mesa.spec "$HOME/rpmbuild/SPECS/mesa.spec"

echo "==> [2/5] add OUR venus/zink patches + bump Release"
cp -f "$PATCHES"/0001-zink-nullDescriptor-emulation-MR37115.diff \
      "$PATCHES"/0009-venus-wsi-present-fix.diff \
      "$PATCHES"/0010-venus-image-physdev-native-modifier.diff \
      "$PATCHES"/0011-venus-wsi-drop-16bit-unorm-swapchain.diff \
      "$PATCHES"/0012-venus-degrade-to-stub-instance-when-ring-setup-fails.diff \
      "$PATCHES"/0013-venus-pin-icd-for-tls-destructor.diff \
      "$PATCHES"/0014-zink-fix-unflushed-batch-wait-lost-wakeup.diff \
      "$HOME/rpmbuild/SOURCES/"
SPEC="$HOME/rpmbuild/SPECS/mesa.spec"
LAST_PATCH_LINE=$( { grep -nE "^Patch[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
[ -n "$LAST_PATCH_LINE" ] || LAST_PATCH_LINE=$( { grep -nE "^Source[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
ins="Patch9001: 0001-zink-nullDescriptor-emulation-MR37115.diff\nPatch9009: 0009-venus-wsi-present-fix.diff\nPatch9010: 0010-venus-image-physdev-native-modifier.diff\nPatch9011: 0011-venus-wsi-drop-16bit-unorm-swapchain.diff\nPatch9012: 0012-venus-degrade-to-stub-instance-when-ring-setup-fails.diff\nPatch9013: 0013-venus-pin-icd-for-tls-destructor.diff\nPatch9014: 0014-zink-fix-unflushed-batch-wait-lost-wakeup.diff"
sed -i "${LAST_PATCH_LINE}a ${ins}" "$SPEC"
# Our patches are plain `git diff` (no mailbox headers); ensure %autosetup uses GNU patch (-p1).
sed -i -E "s/^%autosetup -S git/%autosetup -p1/" "$SPEC"
if ! grep -qE "^%autosetup" "$SPEC"; then
  sed -i "/^%setup/a %patch -P 9001 -p1\n%patch -P 9009 -p1\n%patch -P 9010 -p1\n%patch -P 9011 -p1" "$SPEC"
fi
# Release: pin to a deterministic "<N>.limina" (N = LIMINA_REL, default 1) so (a) our build outranks
# stock, and (b) bumping LIMINA_REL yields a STRICTLY NEWER NEVRA than a prior enhanced build —
# required so `dnf` UPGRADES an already-installed enhanced mesa (a same-NEVRA rebuild would NOT
# upgrade, and install-enhanced.sh would have nothing to do). Drop rpmautospec macros (no package
# git here). The catch-all replaces whatever Release form the SRPM uses (%autorelease or literal).
LIMINA_REL="${LIMINA_REL:-1}"
sed -i -E "/^%autochangelog/d" "$SPEC"
sed -i -E "s/^Release:.*/Release:        ${LIMINA_REL}.limina%{?dist}/" "$SPEC"
grep -nE "^Version:|^Release:|^Patch9|^%autosetup|^%setup" "$SPEC" | head

echo "==> [3/5] builddep (live, against this guest's repos)"
sudo dnf -y builddep "$SPEC"
# mesa 26.x ships Rust components whose BuildRequires are DYNAMIC (cargo2rpm via
# %generate_buildrequires); `dnf builddep <spec>` can't see those, so rpmbuild -bb would fail on
# crate(...) deps (paste, rustc-hash, syn, …). Generate the dynamic buildreqs (rpmbuild -br emits a
# *.buildreqs.nosrc.rpm) and dnf-builddep that too. Loop a couple times in case resolving one tier
# reveals another.
for _ in 1 2 3; do
  rpmbuild -br "$SPEC" >/dev/null 2>&1 || true
  # `|| true`: when the dynamic crate buildreqs are already satisfied, rpmbuild -br emits NO
  # buildreqs.nosrc.rpm, so the glob matches nothing and `ls` exits non-zero — which under
  # set -e/pipefail would kill the script at this assignment (before [4/5]). Empty BR is fine:
  # the next line breaks the loop and proceeds straight to the build.
  BR=$(ls -t "$HOME"/rpmbuild/SRPMS/mesa-*.buildreqs.nosrc.rpm 2>/dev/null | head -1) || true
  [ -n "$BR" ] || break
  sudo dnf -y builddep "$BR" 2>&1 | tail -2
  rpmbuild -br "$SPEC" >/dev/null 2>&1 && break || true
done

echo "==> [4/5] rpmbuild"
# If %prep fails on Patch9009/9010, the venus present-fix needs rebasing onto mesa $MESA_VER —
# rebase it (it is the black-screen fix) and re-run. Do NOT ship without it.
rpmbuild -bb "$SPEC"

echo "==> [5/5] collect RPMs -> $OUT"
cp -f "$HOME"/rpmbuild/RPMS/aarch64/*.rpm "$OUT"/ 2>/dev/null || true
cp -f "$HOME"/rpmbuild/RPMS/noarch/*.rpm  "$OUT"/ 2>/dev/null || true
ls -la "$OUT"/*.rpm 2>/dev/null || { echo "(no RPMs produced — check the build log above)"; exit 1; }

# Venus sanity: the enhanced tier is pointless without the virtio (venus) Vulkan ICD. F44 mesa
# builds it by default; if this warns, add 'virtio' to the spec's `%global vulkan_drivers` line.
if ! rpm -qlp "$OUT"/mesa-vulkan-drivers-*.rpm 2>/dev/null | grep -q 'virtio_icd'; then
  echo "WARN: no virtio_icd (venus) in mesa-vulkan-drivers — add 'virtio' to %global vulkan_drivers and rebuild" >&2
fi
