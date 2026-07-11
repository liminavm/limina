#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build limina's patched mutter as a Fedora RPM, NATIVELY INSIDE a Fedora 44 guest.
#
# OPTIONAL TOOLING since 2026-07-11: patched mutter is NO LONGER part of the guest support
# payload (build-all.sh doesn't call this) — the GNOME clipboard tier is the clipboard@limina
# shell extension (guest/gnome-shell-extension/), so stock mutter stays stock and distro
# updates have nothing of ours to displace. Kept for experiments with the ext-data-control
# tier on GNOME.
#
# This is scripts/build-mutter-rpm.sh's recipe with the macOS `container` wrapper stripped:
# take THIS guest's own mutter SRPM (its exact 50.x version + Fedora packaging + Fedora patches),
# add OUR patches (whatever patches/mutter/*.patch exist), bump Release so dnf prefers ours over
# same-version stock. NOT versionlocked — mutter's libmutter-NN ABI is lock-stepped to
# gnome-shell, so it must track the distro; re-run on a distro bump and rebase patches/mutter/*
# if they no longer apply.
#
# OUR patches (see patches/mutter/README.md):
#   0003  ext-data-control-v1, so limina-agent does clipboard as a focusless Wayland client.
#         (0001/0002 retired to patches/mutter/retired/ — root causes fixed in other layers.)
#
# Usage (in the guest):  scripts/provision/f44/build-mutter-rpm.sh
# Output: $OUT/*.rpm  (default ~/limina-build/mutter)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PATCHES="$REPO/patches/mutter"
OUT="${OUT:-$HOME/limina-build/mutter}"
mkdir -p "$OUT"

command -v rpmbuild >/dev/null || sudo dnf install -y rpm-build rpmdevtools dnf-plugins-core 'dnf-command(builddep)'
rpmdev-setuptree

echo "==> [1/5] fetch THIS guest's mutter SRPM (the version we match)"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
dnf download --source mutter
SRPM=$(ls mutter-*.src.rpm | head -1); echo "    SRPM: $SRPM"
rpm2cpio "$SRPM" | cpio -idmu --quiet
MUTTER_VER=$(rpm -q --qf "%{VERSION}" -p "$SRPM"); echo "    matching mutter version: $MUTTER_VER"
cp -f ./* "$HOME/rpmbuild/SOURCES/" 2>/dev/null || true
cp -f mutter.spec "$HOME/rpmbuild/SPECS/mutter.spec"

echo "==> [2/5] add OUR patches + bump Release"
cp -f "$PATCHES"/*.patch "$HOME/rpmbuild/SOURCES/"
SPEC="$HOME/rpmbuild/SPECS/mutter.spec"
# Insert our Patch declarations after the LAST existing Patch line (high numbers avoid collisions);
# %autosetup applies all declared PatchN in order, so ours apply last (on top of Fedora patches).
LAST_PATCH_LINE=$( { grep -nE "^Patch[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
[ -n "$LAST_PATCH_LINE" ] || LAST_PATCH_LINE=$( { grep -nE "^Source[0-9]*:" "$SPEC" || true; } | tail -1 | cut -d: -f1)
# Declare a PatchNNNN line for EACH patches/mutter/*.patch present (numbered 9000 + the file's
# numeric prefix), built dynamically so adding/removing/skipping a patch is just adding/removing
# its file — no hardcoded list to keep in sync. %autosetup applies them in number order, last.
ins=""; nums=""
for p in $(ls "$PATCHES"/*.patch | sort); do
  bn=$(basename "$p"); pfx=$(echo "$bn" | grep -oE "^[0-9]+" || echo "000"); num=$((9000 + 10#$pfx))
  ins="${ins}Patch${num}: ${bn}\n"; nums="$nums $num"
done
sed -i "${LAST_PATCH_LINE}a ${ins%\\n}" "$SPEC"
# Fedora mutter uses `%autosetup -S git` (git am, needs mailbox patches); OUR patches are plain
# `git diff` output -> switch to `%autosetup -p1` (GNU patch) so both apply by context.
sed -i -E "s/^%autosetup -S git/%autosetup -p1/" "$SPEC"
if ! grep -qE "^%autosetup" "$SPEC"; then
  fb=""; for num in $nums; do fb="${fb}%patch -P ${num} -p1\n"; done
  sed -i "/^%setup/a ${fb%\\n}" "$SPEC"
fi
# Bump Release with a .limina tag so our same-version build outranks stock (l > f in rpm compare).
sed -i -E "s/^(Release:[[:space:]]*)([^%[:space:]]+)/\1\2.limina/" "$SPEC"
sed -i -E "/^%autochangelog/d" "$SPEC"
sed -i -E "s/^Release:.*%\{?\??autorelease\}?.*/Release:        1.limina%{?dist}/" "$SPEC"
grep -nE "^Version:|^Release:|^Patch9|^%autosetup|^%setup" "$SPEC" | head

echo "==> [3/5] builddep (live, against this guest's repos)"
sudo dnf -y builddep "$SPEC"

echo "==> [4/5] rpmbuild"
# NOTE: if %prep fails applying Patch9001 (cogl), patches/mutter/0001 needs a rebase onto mutter
# $MUTTER_VER — that is expected across the GNOME 49->50 bump. Rebase it and re-run.
rpmbuild -bb "$SPEC"

echo "==> [5/5] collect RPMs -> $OUT"
cp -f "$HOME"/rpmbuild/RPMS/aarch64/*.rpm "$OUT"/ 2>/dev/null || true
cp -f "$HOME"/rpmbuild/RPMS/noarch/*.rpm  "$OUT"/ 2>/dev/null || true   # mutter-common is noarch
ls -la "$OUT"/*.rpm 2>/dev/null || { echo "(no RPMs produced — check the build log above)"; exit 1; }
