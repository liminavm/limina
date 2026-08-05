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
# OUR patches: the COMMITTED series patches/mesa-guest/ — a derived artifact of the fork
# branch liminavm/mesa `limina-guest` (third_party/manifest.toml [mesa-guest] pins the rev;
# scripts/export-mesa-guest-patches.sh regenerates the series from base..rev). The fork branch
# is the source of truth; per-patch rationale lives in each patch's own commit message. To
# change the set: commit on the fork, push, bump the manifest rev, re-export, then re-run this
# with LIMINA_REL bumped. Current series (venus-only — guest GL rides virgl/vrend since
# drop-guest-zink 2026-08-04, so the old zink rows 0001/0014 are gone):
#   0001  venus/wsi linear-modifier fallback + 16F swapchain block — THE black-screen fix
#   0002  venus/wsi drop the 16-bit-unorm wayland swapchain format (wgpu ghost-UI)
#   0003  venus: degrade to the stub instance when ring setup fails (stock-4k GRUB fallback
#         boot must not lose lavapipe)
#   0004  venus: pin the ICD when creating the TLS-destructor key (thread-exit SIGSEGV)
#   0005  venus: ring loss -> VK_ERROR_DEVICE_LOST, not abort() (snapshot-resume survival)
#   0006  venus: track vn_ring_submit capacity in its own field (quadratic CPU creep;
#         upstream main has 09fb7ca8d82 but this release branch does NOT — its %prep
#         apply-FAILURE at a future base bump is the retirement signal)
# We add them via the spec (NOT a tolerant pre-apply) ON PURPOSE: a non-applying patch FAILS
# %prep loudly, rather than silently shipping a present-fix-less (black-screen) mesa.
#
# Usage (in the guest):  scripts/provision/f44/build-mesa-rpm.sh
# Output: $OUT/*.rpm  (default ~/limina-build/mesa)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PATCHES="$REPO/patches/mesa-guest"
OUT="${OUT:-$HOME/limina-build/mesa}"
mkdir -p "$OUT"

command -v rpmbuild >/dev/null || sudo dnf install -y rpm-build rpmdevtools dnf-plugins-core 'dnf-command(builddep)'
rpmdev-setuptree

echo "==> [1/5] fetch THIS guest's mesa SRPM"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
# MESA_SRPM_URL pins the base to a specific SRPM (e.g. koji) instead of whatever version the
# repos serve today — use it when the distro moves the version under us and our patches would
# need a rebase before the new base is validated. (History: 26.1.4 broke 0009 → rebased as
# 0015, validated 2026-07-20; 26.1.3 was the pinned base before that.)
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

echo "==> [2/5] add OUR venus patches (the exported limina-guest series) + bump Release"
ls "$PATCHES"/*.patch >/dev/null  # empty series = the export was never run; fail loudly
cp -f "$PATCHES"/*.patch "$HOME/rpmbuild/SOURCES/"
SPEC="$HOME/rpmbuild/SPECS/mesa.spec"
LAST_PATCH_LINE=$( { grep -nE "^Patch[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
[ -n "$LAST_PATCH_LINE" ] || LAST_PATCH_LINE=$( { grep -nE "^Source[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
# Patch9NNN lines + a %patch fallback, both derived from the series listing (sorted = apply order).
ins=""; fallback=""; n=9001
for p in "$PATCHES"/*.patch; do
  ins="${ins}Patch${n}: $(basename "$p")\n"
  fallback="${fallback}%patch -P ${n} -p1\n"
  n=$((n+1))
done
sed -i "${LAST_PATCH_LINE}a ${ins%\\n}" "$SPEC"
# The series is git format-patch mailbox output; GNU patch skips the mail headers fine, so a
# plain -p1 %autosetup applies it (no need for the spec's `-S git`).
sed -i -E "s/^%autosetup -S git/%autosetup -p1/" "$SPEC"
if ! grep -qE "^%autosetup" "$SPEC"; then
  sed -i "/^%setup/a ${fallback%\\n}" "$SPEC"
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

# PREP_ONLY=1: stop after proving %prep applies (Fedora patches + ours) — the fast
# iteration loop for rebasing a patch onto a new distro base, without the builddep +
# compile cost. --nodeps because %prep needs no BuildRequires.
if [ -n "${PREP_ONLY:-}" ]; then
  echo "==> PREP_ONLY: rpmbuild -bp --nodeps (patch-apply test)"
  rpmbuild -bp --nodeps "$SPEC"
  echo "==> PREP_ONLY: all patches applied cleanly on mesa $MESA_VER"
  exit 0
fi

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
# If %prep fails on Patch9001, the venus present-fix needs rebasing onto mesa $MESA_VER —
# rebase it on the fork branch (it is the black-screen fix) and re-run. Do NOT ship without it.
# If %prep fails on Patch9006 (freelist capacity), upstream 09fb7ca8d82 reached this base —
# drop that commit from the fork branch and re-export (planned retirement, not a break).
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
