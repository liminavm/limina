#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier 16 KiB-page kernel as a Fedora RPM, NATIVELY INSIDE an F44 guest.
#
# Per the "Fedora config for the most part" goal, this does NOT use a bare upstream `make
# defconfig` (the old scripts/build-kernel-rpm.sh did). Instead it starts from THIS guest's real
# Fedora config (/boot/config-$(uname -r)) on a matching upstream source tree, and applies the
# single load-bearing delta — CONFIG_ARM64_16K_PAGES=y — plus a few build-hygiene flips so a
# Fedora config builds cleanly outside Fedora's kernel.spec (neutralize the Fedora signing-cert
# paths; keep the VM-critical drivers =y so boot never depends on initramfs contents). Packaged
# via the same minimal Fedora-style spec the F43 build proved (ships /lib/modules/<KREL>,
# %posttrans `kernel-install add` so dracut writes the initramfs + BLS entry, co-installs beside
# stock). The 16k kernel is distro-independent: the SAME RPM installs on F43 and F44.
#
# THE INTRICATE ONE — most likely to need an in-guest tweak. Knobs: KVER (source tag),
# CONFIG_BASE (base config). If the Fedora-config build fights you, the validated fallback is the
# old upstream-defconfig recipe in scripts/build-kernel-rpm.sh.
#
# Usage (in the guest):  scripts/provision/f44/build-kernel-rpm.sh
# Output: $OUT/limina-kernel-16k-*.rpm  (default ~/limina-build/kernel)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
PATCHES="$REPO/patches/linux"
OUT="${OUT:-$HOME/limina-build/kernel}"
BUILD="${BUILD:-$HOME/limina-build/linux}"
LOCALVERSION="${LOCALVERSION:--limina16k}"
# Source tag: default to the running kernel's upstream base version (e.g. 7.0.12 -> v7.0.12).
KVER="${KVER:-v$(uname -r | sed -E 's/-.*$//')}"
# Base config: the running guest's real Fedora config.
CONFIG_BASE="${CONFIG_BASE:-/boot/config-$(uname -r)}"
STABLE_URL="https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git"
mkdir -p "$OUT"
[ -r "$CONFIG_BASE" ] || { echo "base config $CONFIG_BASE not readable; set CONFIG_BASE" >&2; exit 1; }

echo "==> [1/6] kernel build deps"
command -v rpmbuild >/dev/null || sudo dnf install -y rpm-build rpmdevtools
sudo dnf -y builddep kernel 2>/dev/null || \
  sudo dnf install -y gcc make flex bison bc elfutils-libelf-devel openssl-devel perl \
    rsync kmod gzip diffutils findutils git dwarves

echo "==> [2/6] source tree ($KVER, shallow)"
if [ ! -d "$BUILD/.git" ]; then
  rm -rf "$BUILD"
  git clone --depth 1 --branch "$KVER" "$STABLE_URL" "$BUILD" \
    || { echo "could not clone $KVER from the stable tree; set KVER to a real tag" >&2; exit 1; }
fi
cd "$BUILD"
git config --global --add safe.directory "$BUILD" 2>/dev/null || true
git reset --hard HEAD >/dev/null 2>&1 || true
git clean -qfdx 2>/dev/null || true

echo "==> [3/6] limina-patch the source (tolerant — likely already upstream in $KVER)"
shopt -s nullglob
for p in "$PATCHES"/*.patch; do
  case "$(basename "$p")" in 0005-*) continue ;; esac  # mandatory, applied below
  if git apply --check "$p" 2>/dev/null; then git apply "$p"; echo "    applied $(basename "$p")"
  else echo "    SKIP $(basename "$p") (does not apply — likely already upstream)"; fi
done
shopt -u nullglob
# 0005 (virtio_balloon: stop page-reporting across suspend) is NOT upstream — a silent skip would
# ship the s2idle UAF back into the image, so apply it FAIL-LOUD (drop this once it lands upstream).
git apply "$PATCHES"/0005-virtio-balloon-stop-page-reporting-across-suspend.patch
echo "    applied 0005-virtio-balloon-stop-page-reporting-across-suspend.patch (mandatory)"
# We git-apply patches WITHOUT committing, so the tree is "dirty" and scripts/setlocalversion (which
# in current kernels IGNORES an empty .scmversion) appends `-dirty` to the release even with
# LOCALVERSION_AUTO=n — e.g. 6.19.10-limina16k-dirty, whose second dash is an INVALID rpm EVR for
# `Provides: kernel-uname-r`, so packaging fails. Remove the git metadata so setlocalversion finds
# no SCM and emits NO suffix → clean 6.19.10-limina16k. (Re-runs re-clone via the step-2 guard;
# a shallow clone is cheap.)
rm -rf "$BUILD/.git"

echo "==> [4/6] Fedora config + the 16k delta"
cp -f "$CONFIG_BASE" .config
cat > /tmp/limina-16k.fragment <<'FRAG'
# The one load-bearing change: 16 KiB pages (venus host-visible blobs need it on the 16k host).
CONFIG_ARM64_16K_PAGES=y
# Build hygiene when compiling a Fedora config OUTSIDE Fedora's kernel.spec: don't chase Fedora's
# signing/revocation cert files (absent here), and skip BTF (needs a matching pahole).
CONFIG_SYSTEM_TRUSTED_KEYS=""
CONFIG_SYSTEM_REVOCATION_KEYS=""
CONFIG_DEBUG_INFO_BTF=n
# Strip debug info to a Fedora-runtime-like size: a Fedora config selects CONFIG_DEBUG_INFO_DWARF5,
# which bloats vmlinux + every module with DWARF (the RPM balloons to GBs; modules_install copies
# hundreds of MB of .debug). DEBUG_INFO_NONE deselects the DWARF choice -> a lean kernel + modules
# matching the shipped Fedora kernel's on-disk size (Fedora keeps DWARF in -debuginfo subpackages).
CONFIG_DEBUG_INFO_NONE=y
# VM-critical drivers stay =y so boot never depends on initramfs contents (robustness; small delta
# over Fedora's =m). dracut still builds an initramfs for the BLS root=UUID resolution.
CONFIG_VIRTIO=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_VIRTIO_NET=y
CONFIG_VIRTIO_INPUT=y
CONFIG_DRM=y
CONFIG_DRM_VIRTIO_GPU=y
CONFIG_FUSE_FS=y
CONFIG_VIRTIO_FS=y
CONFIG_VSOCKETS=y
CONFIG_VIRTIO_VSOCKETS=y
CONFIG_BTRFS_FS=y
CONFIG_SERIAL_AMBA_PL011=y
CONFIG_SERIAL_AMBA_PL011_CONSOLE=y
CONFIG_LOCALVERSION_AUTO=n
FRAG
echo "CONFIG_LOCALVERSION=\"$LOCALVERSION\"" >> /tmp/limina-16k.fragment
./scripts/kconfig/merge_config.sh -m .config /tmp/limina-16k.fragment
make ARCH=arm64 olddefconfig
grep -q '^CONFIG_ARM64_16K_PAGES=y' .config || { echo "16K not set after merge" >&2; exit 1; }

echo "==> [5/6] compile Image + modules (-j$(nproc))"
make ARCH=arm64 -j"$(nproc)" Image modules
KREL=$(cat include/config/kernel.release)
BASE=$(make -s ARCH=arm64 kernelversion)
echo "    kernel release: $KREL (base $BASE)"

STAGE="$BUILD/stage"; rm -rf "$STAGE"; mkdir -p "$STAGE/lib/modules/$KREL"
make ARCH=arm64 INSTALL_MOD_PATH="$STAGE" modules_install
rm -f "$STAGE/lib/modules/$KREL/build" "$STAGE/lib/modules/$KREL/source"
cp -f arch/arm64/boot/Image "$STAGE/lib/modules/$KREL/vmlinuz"
cp -f System.map "$STAGE/lib/modules/$KREL/System.map"
cp -f .config "$STAGE/lib/modules/$KREL/config"

echo "==> [6/6] package limina-kernel-16k RPM"
rpmdev-setuptree
SPEC="$HOME/rpmbuild/SPECS/limina-kernel.spec"
cat > "$SPEC" <<SPEC
%global krel $KREL
%global debug_package %{nil}
%global __os_install_post %{nil}
%define _build_id_links none
Name:           limina-kernel-16k
Version:        $BASE
Release:        1%{?dist}
Summary:        limina enhanced-tier 16 KiB-page Linux kernel ($KVER, Fedora config + 16k)
License:        GPL-2.0-only
BuildArch:      aarch64
Provides:       kernel-uname-r = %{krel}
# installonlypkg(kernel): make dnf treat this like kernel-core — INSTALL a new version BESIDE the
# old one instead of replacing it, so the previously-installed enhanced kernel stays as a fallback.
# Without it, installing a newer limina-kernel-16k erased the prior one (no fallback if the new one
# failed to boot). dnf keeps up to installonly_limit (default 3) versions.
Provides:       installonlypkg(kernel)
Requires(posttrans): systemd-udev
Requires(posttrans): dracut
Requires(preun): systemd-udev
%description
The limina enhanced tier kernel: F44's config + 16 KiB pages (required for venus host-visible
blobs on the 16 KiB-page macOS host). Installs alongside stock; GRUB offers both, falling back to
stock 4 KiB if 16k ever fails.
%install
mkdir -p %{buildroot}/lib/modules
cp -a $STAGE/lib/modules/%{krel} %{buildroot}/lib/modules/
%files
/lib/modules/%{krel}
%posttrans
kernel-install add %{krel} /lib/modules/%{krel}/vmlinuz || :
%preun
if [ \$1 -eq 0 ]; then kernel-install remove %{krel} || :; fi
SPEC
rpmbuild -bb "$SPEC"
cp -f "$HOME"/rpmbuild/RPMS/aarch64/limina-kernel-16k-*.rpm "$OUT"/
ls -la "$OUT"/*.rpm 2>/dev/null || { echo "(no kernel RPM produced)"; exit 1; }
echo "    KREL=$KREL"
