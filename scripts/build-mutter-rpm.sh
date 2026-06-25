#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build limina enhanced-tier patched **mutter** as a stock-style Fedora RPM that REPLACES the
# distro's mutter at /usr.
#
# UNLIKE mesa (build-mesa-rpm.sh), mutter is NOT pinned to our own version: mutter is lock-stepped
# to gnome-shell (same libmutter-NN ABI), so we MUST track the TARGET distro's mutter version and
# rebase OUR patches onto it -- pinning an old mutter while gnome-shell updates would wedge the
# desktop. So this builder takes the target Fedora's OWN mutter SRPM (its exact version + Fedora
# packaging + Fedora patches) and just ADDS our patches on top, then bumps Release so dnf prefers
# ours over same-version stock. It is therefore NOT dnf-versionlocked (cf. mesa, which is). When the
# distro bumps mutter, re-run this; if patches/mutter/* no longer apply, rebase them. See memory
# limina-enh-delivery.
#
# OUR patches (patches/mutter/*.patch, see patches/mutter/README.md):
#   0001  cogl stencil-clip degrade (#32) -- seated desktop on venus/KK renders without a stencil
#         buffer; without this the desktop corrupts/crashes.
#   0002  guard meta_x11_display_init_frames_client against a NULL launch (first X11 app SIGSEGVs
#         the whole compositor otherwise).
#   0003  ext-data-control-v1, so limina-agent does clipboard as a focusless Wayland client.
#
# FEDORA_REL MUST match the guest: mutter links the guest GLib/GTK/wayland sonames and the
# libmutter-NN ABI must match the guest gnome-shell.
#
# Usage: scripts/build-mutter-rpm.sh                 # F43 target, F43 mutter version
#        FEDORA_REL=44 scripts/build-mutter-rpm.sh   # build for an F44 guest (its mutter version)
# Output: target/test-guest/mutter/rpm/*.aarch64.rpm  (gitignored)
set -euo pipefail
cd "$(dirname "$0")/.."

FEDORA_REL="${FEDORA_REL:-43}"           # build + target guest release (mutter version = this release's)
JOBS="${JOBS:-8}"
MEM="${MEM:-8g}"
OUT="target/test-guest/mutter"
RPMOUT="$OUT/rpm"
mkdir -p "$RPMOUT"

command -v container >/dev/null || { echo "Apple 'container' not installed" >&2; exit 1; }

VOL="limina-mutterbuild"
container volume create -s 16g "$VOL" >/dev/null 2>&1 || true

echo "==> building limina patched-mutter RPM (target distro version, fedora:$FEDORA_REL, -j$JOBS)"

scripts/build-image.sh   # ensure the unified limina-build image (rpm tooling + builddep mutter baked)
container run --rm --cpus "$JOBS" --memory "$MEM" \
  -v "$(pwd)/$OUT:/out" \
  -v "$(pwd)/patches/mutter:/patches:ro" \
  -v "$VOL:/build" \
  limina-build:fc43 bash -euo pipefail -c '
    export HOME=/build
    rpmdev-setuptree

    echo "--- [2/5] fetch this Fedora release mutter SRPM (the version we MATCH)"
    cd /tmp
    rm -f mutter-*.src.rpm
    dnf download --source mutter >/dev/null 2>&1
    SRPM=$(ls mutter-*.src.rpm | head -1); echo "    SRPM: $SRPM"
    rpm2cpio "$SRPM" | cpio -idmu --quiet
    MUTTER_VER=$(rpm -q --qf "%{VERSION}" -p "$SRPM"); echo "    matching mutter version: $MUTTER_VER"
    cp -f ./* "$HOME/rpmbuild/SOURCES/" 2>/dev/null || true
    cp -f mutter.spec "$HOME/rpmbuild/SPECS/mutter.spec"

    echo "--- [3/5] add OUR patches + bump Release"
    cp -f /patches/*.patch "$HOME/rpmbuild/SOURCES/"
    SPEC="$HOME/rpmbuild/SPECS/mutter.spec"
    # Insert our Patch declarations after the LAST existing Patch line (high numbers avoid collisions);
    # %autosetup applies all declared PatchN in order, so ours apply last (on top of Fedora patches).
    # Fedora mutter has an UNNUMBERED "Patch:" (Patch0) -- match [0-9]* (zero or more) and guard the
    # pipe so a no-match grep does not trip set -e. Fall back to the last Source line if no Patch.
    LAST_PATCH_LINE=$( { grep -nE "^Patch[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
    [ -n "$LAST_PATCH_LINE" ] || LAST_PATCH_LINE=$( { grep -nE "^Source[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
    ins="Patch9001: 0001-32-stencil-clip-degrade-fix.patch\nPatch9002: 0002-x11-survive-frames-client-launch-failure.patch\nPatch9003: 0003-ext-data-control-v1.patch"
    sed -i "${LAST_PATCH_LINE}a ${ins}" "$SPEC"
    # Fedora mutter uses `%autosetup -S git` (git am, needs mailbox-format patches); OUR patches are
    # plain `git diff` output (no From/Subject) -> switch to `%autosetup -p1` (GNU patch) so both the
    # Fedora patch and ours apply by context.
    sed -i -E "s/^%autosetup -S git/%autosetup -p1/" "$SPEC"
    # If the spec uses %autosetup it auto-applies them; if it uses %setup, apply explicitly after it.
    if ! grep -qE "^%autosetup" "$SPEC"; then
      echo "    spec uses %setup (not %autosetup) -- injecting explicit %patch after %setup"
      sed -i "/^%setup/a %patch -P 9001 -p1\n%patch -P 9002 -p1\n%patch -P 9003 -p1" "$SPEC"
    fi
    # Bump Release with a .limina tag so our same-version build outranks stock (l > f in rpm compare).
    sed -i -E "s/^(Release:[[:space:]]*)([^%[:space:]]+)/\1\2.limina/" "$SPEC"
    # Drop rpmautospec macros if present (no package git here)
    sed -i -E "/^%autochangelog/d" "$SPEC"
    sed -i -E "s/^Release:.*%\{?\??autorelease\}?.*/Release:        1.limina%{?dist}/" "$SPEC"
    grep -nE "^Version:|^Release:|^Patch9|^%autosetup|^%setup" "$SPEC" | head

    echo "--- [4/5] builddep"
    dnf -y builddep "$SPEC" 2>&1 | tail -3

    echo "--- [5/5] rpmbuild"
    rpmbuild -bb "$SPEC" 2>&1 | tail -40
    mkdir -p /out/rpm
    cp -f "$HOME"/rpmbuild/RPMS/aarch64/*.rpm /out/rpm/ 2>/dev/null || true
    cp -f "$HOME"/rpmbuild/RPMS/noarch/*.rpm  /out/rpm/ 2>/dev/null || true   # mutter-common is noarch
    echo "=== built RPMs ==="; ls -1 /out/rpm/*.rpm 2>/dev/null | sed "s#/out/#target/test-guest/mutter/#"
  '
echo "==> mutter RPMs in: $RPMOUT/"
ls -la "$RPMOUT"/*.rpm 2>/dev/null || echo "(no RPMs produced -- check the build log above)"
