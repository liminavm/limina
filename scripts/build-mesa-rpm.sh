#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier Mesa (zink + venus, OUR pinned version) as a stock-style
# Fedora RPM SET that REPLACES the distro's mesa at /usr.
#
# WHY an RPM (not a sysext, not an /opt prefix): our mesa is 26.2 but stock F43 is 25.3.6, so
# the megadriver soname differs (libgallium-26.2.0-devel.so vs libgallium-25.3.6.so). A sysext
# overlay can SHADOW but not REMOVE the stock lib, so the stock 25.3.6 lib survives underneath
# and blends into an ABI-inconsistent /usr/lib64/dri -> mutter's KMS-master EGL probe fails
# ("GPU /dev/dri/card0 ... not supported by EGL") -> gnome-shell crash-loop. An RPM REPLACES
# the stock package (the old soname is genuinely removed), so nothing blends. dnf versionlock
# (applied by the installer, not here) then pins our version so an update can't revert it.
# See memory limina-enh-delivery.
#
# APPROACH (a): rebuild from Fedora's SRPM. We base on RAWHIDE's mesa.spec (26.1.3 -- one point
# release off our 26.2, so the subpackage split / %files / build options already match 26.x),
# and feed it OUR exact verified source snapshot. We DON'T use the spec's patch machinery:
# instead we check out our commit, apply patches/mesa/* (MR!37115 zink nullDescriptor + the
# venus WSI present-fix 0009/0010), commit, and `git archive` a PRE-PATCHED mesa-<ver>.tar.xz.
# Fedora's own downstream Patch lines are stripped (they're against 26.1.3; our tree is 26.2.0).
#
# Built inside an aarch64 Fedora `container` matching the TARGET guest release (default F43), so
# the RPMs link the guest's runtime sonames. Output RPMs are same-named as stock at a higher
# version, so `dnf install ./mesa-*.rpm` swaps stock -> ours. Mesa is OUR-version-pinned (cf.
# mutter, which tracks the target distro -- see build-mutter-rpm.sh).
#
# Usage: scripts/build-mesa-rpm.sh                 # F43 target, our 26.2 snapshot
#        FEDORA_REL=44 scripts/build-mesa-rpm.sh   # build for an F44 guest
# Output: target/test-guest/mesa/rpm/*.aarch64.rpm  (gitignored)
set -euo pipefail
cd "$(dirname "$0")/.."

# Our pinned mesa: the EXACT snapshot verified to render zink+venus (commit 3515c52,
# 26.2.0-devel). Keep in sync with scripts/build-mesa-zink.sh.
MESA_VER="${MESA_VER:-26.2.0}"
MESA_COMMIT="${MESA_COMMIT:-3515c52e8cf31549b6068ef43c23c89830b6db46}"
FEDORA_REL="${FEDORA_REL:-43}"           # build + target guest release (RPMs link its sonames)
SPEC_REL="${SPEC_REL:-rawhide}"          # which Fedora's mesa.spec to base on (closest to 26.2)
# Drivers to %undefine: the TARGET Fedora can't satisfy their (rawhide-version) BuildRequires AND
# a VM guest has no such GPU. NOTE: must %undefine (not set 0) -- the spec keys %{?with_X:...} on
# DEFINED, not value. Why each:
#   d3d12   -> needs DirectX-Headers>=1.619.1 (not in F43); Windows D3D12-on-Vulkan, useless in a VM
#   nvk asahi panfrost opencl -> these four gate mesa's Rust path (%cargo_generate_buildrequires,
#     spec line 184): they pull crate(syn/paste/rustc-hash/...) which F43 lacks. None exist in a VM.
#   spirv_tools -> its pkgconfig(SPIRV-Tools) BR is ALSO under the line-184 Rust guard, but
#     with_spirv_tools=1 still forces -Dspirv-tools=enabled -> meson fails (no SPIRV-Tools installed).
#     Nothing we keep (venus/zink/llvmpipe) needs it; consistent to disable since its consumers are off.
#   teflon -> NPU (ethosu/rocket); useless in a VM, avoids its deps.
# zink (our GL) rides with_vulkan_hw, which stays on; radeonsi/radv/svga/etc. stay (build clean on
# F43, unused but harmless) to avoid %files surgery -- trim later if we want slimmer RPMs.
DISABLE_DRIVERS="${DISABLE_DRIVERS:-d3d12 nvk asahi panfrost opencl spirv_tools teflon}"
JOBS="${JOBS:-8}"
MEM="${MEM:-12g}"
OUT="target/test-guest/mesa"
RPMOUT="$OUT/rpm"
mkdir -p "$RPMOUT" "$OUT/cache"

command -v container >/dev/null || { echo "Apple 'container' not installed" >&2; exit 1; }

# Persistent build volume (mesa git clone + ccache survive across iterations).
VOL="limina-mesabuild"
container volume create -s 40g "$VOL" >/dev/null 2>&1 || true

echo "==> building limina mesa RPM set ($MESA_VER @ ${MESA_COMMIT:0:12}; spec=$SPEC_REL; target=fedora:$FEDORA_REL; -j$JOBS)"

container run --rm --cpus "$JOBS" --memory "$MEM" \
  -v "$(pwd)/$OUT:/out" \
  -v "$(pwd)/patches/mesa:/patches" \
  -v "$VOL:/build" \
  "docker.io/library/fedora:$FEDORA_REL" bash -euo pipefail -c '
    MESA_VER="'"$MESA_VER"'"; MESA_COMMIT="'"$MESA_COMMIT"'"; SPEC_REL="'"$SPEC_REL"'"
    DISABLE_DRIVERS="'"$DISABLE_DRIVERS"'"
    export HOME=/build

    echo "--- [1/6] toolchain + rpm tooling"
    dnf -y install rpm-build dnf-plugins-core git xz cpio rpmdevtools >/dev/null
    git config --global user.email limina@local
    git config --global user.name limina
    git config --global --add safe.directory "*"

    echo "--- [2/6] fetch base mesa.spec from fedora:$SPEC_REL SRPM"
    SRCDIR=/build/srpm; mkdir -p "$SRCDIR"; cd "$SRCDIR"
    if [ ! -f mesa.spec ]; then
      # pull the SRPM from the chosen release (rawhide repo is reachable via --releasever)
      dnf download --source mesa --releasever="$SPEC_REL" >/dev/null 2>&1 \
        || dnf download --source mesa >/dev/null 2>&1
      SRPM=$(ls mesa-*.src.rpm | head -1); echo "    base SRPM: $SRPM"
      rpm2cpio "$SRPM" | cpio -idmu --quiet
    fi

    echo "--- [3/6] our pre-patched source tree -> SOURCES/mesa-$MESA_VER.tar.xz"
    rpmdev-setuptree
    CACHE=/build/mesa.git
    # A FULL clone is required: git archive must read the patched tree+blob objects. A blobless
    # (--filter) clone leaves them as unfetched promisor objects -> "unable to read sha1 file".
    if [ -d "$CACHE/.git" ] && git -C "$CACHE" config --get remote.origin.partialclonefilter >/dev/null 2>&1; then
      echo "    removing stale blobless cache"; rm -rf "$CACHE"
    fi
    if [ ! -d "$CACHE/.git" ]; then
      echo "    full-cloning mesa (cached, one-time ~1.5GB)"
      git clone https://gitlab.freedesktop.org/mesa/mesa.git "$CACHE"
    fi
    cd "$CACHE"
    git fetch -q origin "$MESA_COMMIT" 2>/dev/null || git fetch -q origin 2>/dev/null || true
    git checkout -q -f "$MESA_COMMIT"
    git clean -qfdx                       # idempotent across the warm cache volume
    echo "    applying patches/mesa/* (tolerant: skip if already upstream)"
    shopt -s nullglob
    for p in /patches/0001*MR37115* /patches/0009*venus* /patches/0010*venus*; do
      [ -f "$p" ] || continue
      if git apply --check "$p" 2>/dev/null; then git apply "$p"; echo "      applied $(basename "$p")"
      else echo "      SKIP $(basename "$p") (does not apply -- likely already upstream)"; fi
    done
    shopt -u nullglob
    git add -A
    git commit -q -m "limina mesa patches" || true
    git archive --format=tar --prefix="mesa-$MESA_VER/" HEAD | xz -T0 > "$HOME/rpmbuild/SOURCES/mesa-$MESA_VER.tar.xz"
    echo "    tarball: $(du -h "$HOME/rpmbuild/SOURCES/mesa-$MESA_VER.tar.xz" | cut -f1)"

    echo "--- [4/6] rewrite the spec for our snapshot (version, release, strip downstream patches/autospec)"
    cd "$SRCDIR"
    cp -f *.spec "$HOME/rpmbuild/SPECS/mesa.spec"
    # copy any non-tarball Sources the spec needs (macros, configs) alongside ours
    find . -maxdepth 1 -type f ! -name "*.src.rpm" ! -name "mesa.spec" ! -name "mesa-*.tar.*" \
      -exec cp -f {} "$HOME/rpmbuild/SOURCES/" \;
    SPEC="$HOME/rpmbuild/SPECS/mesa.spec"
    # pin version; fixed Release that beats stock (25.3.6) AND rawhide (26.1.3) for dnf preference
    sed -i -E "s/^Version:.*/Version:        $MESA_VER/" "$SPEC"
    sed -i -E "s/^Release:.*/Release:        1.limina%{?dist}/" "$SPEC"
    # drop rpmautospec macros (no package git here)
    sed -i -E "/^%autochangelog/d" "$SPEC"
    sed -i -E "/%changelog/a * Wed Jun 25 2026 limina <limina@local> - $MESA_VER-1.limina\n- limina enhanced-tier: our pinned mesa (zink+venus) + patches" "$SPEC"
    # strip Fedora downstream patches (against 26.1.3; our tree is pre-patched at $MESA_VER)
    sed -i -E "/^Patch[0-9]*:/d" "$SPEC"
    # %autosetup must unpack our mesa-$MESA_VER/ and apply NO patches (-N)
    sed -i -E "s/^%autosetup.*/%autosetup -p1 -n mesa-$MESA_VER -N/" "$SPEC"
    # %undefine VM-irrelevant / dep-unavailable drivers. MUST land after the toggle defs but
    # BEFORE the first BuildRequires -- the dep-gating %if 0%{?with_X} guards (e.g. DirectX-Headers
    # under with_d3d12) live in the PREAMBLE, so undefining before %prep is too late for builddep.
    # Anchored before the first BuildRequires: lazy expansion then drops it from BR, %build AND %files.
    for d in $DISABLE_DRIVERS; do
      echo "    disabling driver: with_$d"
      sed -i "0,/^BuildRequires/s/^BuildRequires/%undefine with_$d\nBuildRequires/" "$SPEC"
    done
    # Slim the VULKAN driver set to what a VM needs AND F43 can build: lavapipe(swrast) + venus(virtio).
    # vulkan_drivers is an EAGERLY-expanded %global (it captured the full HW list at its definition,
    # before our undefines), so override it directly. The HW vulkan drivers (radv/broadcom/freedreno/
    # panfrost/powervr) sit UNCONDITIONALLY inside the `%if with_vulkan_hw` %files block (no per-driver
    # toggle) and panfrost needs Rust, so toggles cannot drop them -- delete their %files lines instead.
    # We KEEP with_vulkan_hw ON: that is what puts zink in -Dgallium-drivers and ships zink_dri.so.
    sed -i -E "s|^%global vulkan_drivers .*|%global vulkan_drivers swrast,virtio|" "$SPEC"
    sed -i -E "/libvulkan_radeon\.so/d; /00-radv-defaults/d; /radeon_icd/d; /libvulkan_broadcom\.so/d; /broadcom_icd/d; /libvulkan_freedreno\.so/d; /freedreno_icd/d; /libvulkan_panfrost\.so/d; /panfrost_icd/d; /libvulkan_powervr_mesa\.so/d; /powervr_mesa_icd/d" "$SPEC"
    # mesa 26.2 splits drirc into per-driver 00-<drv>-defaults.conf files (radeonsi/zink/virtio_gpu/...)
    # that the 26.1.3 spec lists only as 00-mesa-defaults.conf -> the rest land unpackaged. Glob ALL
    # built-driver drirc into dri-drivers (replaces the lone explicit entry, no double-listing).
    sed -i -E "s|drirc\.d/00-mesa-defaults\.conf|drirc.d/*.conf|" "$SPEC"
    grep -nE "^Version:|^Release:|^%autosetup|^%undefine|^%global vulkan_drivers" "$SPEC"

    echo "--- [5/6] dnf builddep"
    dnf -y builddep "$SPEC" 2>&1 | tail -3

    echo "--- [6/6] rpmbuild"
    rpmbuild -bb "$SPEC" 2>&1 | tail -40
    mkdir -p /out/rpm
    cp -f "$HOME"/rpmbuild/RPMS/aarch64/*.rpm /out/rpm/ 2>/dev/null || true
    cp -f "$HOME"/rpmbuild/RPMS/noarch/*.rpm  /out/rpm/ 2>/dev/null || true   # any noarch subpackages
    echo "=== built RPMs ==="; ls -1 /out/rpm/*.rpm 2>/dev/null | sed "s#/out/#target/test-guest/mesa/#"
  '
echo "==> mesa RPMs in: $RPMOUT/"
ls -la "$RPMOUT"/*.rpm 2>/dev/null || echo "(no RPMs produced -- check the build log above)"
