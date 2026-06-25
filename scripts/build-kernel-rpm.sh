#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier 16 KiB-page kernel as a STOCK-STYLE Fedora kernel RPM.
#
# Unlike scripts/build-test-kernel.sh (which builds ONLY `make Image`, everything =y, NO
# modules/initramfs — a deliberate shortcut for the L1 *direct-boot* path where we pass
# --kernel + an explicit root=/dev/vda3), the PRODUCT kernel must install and boot exactly
# like a distro kernel: built WITH modules, packaged as an RPM, and on install it runs
# `kernel-install add` so **dracut generates the initramfs** and writes the BLS entry with
# the distro-standard `root=UUID=` options. A no-initramfs kernel given stock BLS options
# can't resolve the fs-UUID and panics on root mount ("subvol not a supported option") — see
# memory limina-enh-kernel-delivery. This RPM co-exists with the stock kernels (GRUB offers
# both; if 16k ever fails the user falls back to stock 4k = the two-tier guarantee).
#
# Builds inside an aarch64 Fedora `container` (host is macOS, can't build Linux); reuses the
# same per-(version,page) build volume as build-test-kernel.sh for incremental rebuilds.
#
# Usage: scripts/build-kernel-rpm.sh         # KVER=v6.12, fedora:43, 16k
#        KVER=v6.12 FEDORA_REL=43 scripts/build-kernel-rpm.sh
# Output: target/test-guest/kernel/rpm/limina-kernel-16k-<ver>-1.*.aarch64.rpm  (gitignored)
set -euo pipefail
cd "$(dirname "$0")/.."

KVER="${KVER:-v6.12}"
JOBS="${JOBS:-8}"
MEM="${MEM:-10g}"
FEDORA_REL="${FEDORA_REL:-43}"
LOCALVERSION="${LOCALVERSION:--limina16k}"   # uname -r becomes <base><LOCALVERSION>, e.g. 6.12.0-limina16k
OUT="target/test-guest/kernel"
RPMOUT="$OUT/rpm"
mkdir -p "$RPMOUT"

command -v container >/dev/null || { echo "Apple 'container' not installed" >&2; exit 1; }

# Enhanced-tier kernel config fragment (mirrors build-test-kernel.sh — same drivers the guest
# needs — PLUS a clean LOCALVERSION and AUTO=n so the RPM has a stable, non '-dirty' release).
# Root-critical drivers stay =y (always present, robust) while dracut still builds an initramfs
# that does the udev root=UUID resolution the BLS entries expect.
cat > "$OUT/limina-rpm.fragment" <<'FRAG'
CONFIG_VIRTIO=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_FUSE_FS=y
CONFIG_VIRTIO_FS=y
CONFIG_VSOCKETS=y
CONFIG_VIRTIO_VSOCKETS=y
CONFIG_BLK_DEV_INITRD=y
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y
CONFIG_SERIAL_AMBA_PL011=y
CONFIG_SERIAL_AMBA_PL011_CONSOLE=y
CONFIG_DEBUG_INFO_BTF=n
CONFIG_DRM=y
CONFIG_DRM_VIRTIO_GPU=y
CONFIG_DRM_FBDEV_EMULATION=y
CONFIG_FB=y
CONFIG_FRAMEBUFFER_CONSOLE=y
CONFIG_INPUT=y
CONFIG_INPUT_EVDEV=y
CONFIG_VIRTIO_INPUT=y
CONFIG_BTRFS_FS=y
CONFIG_VIRTIO_NET=y
CONFIG_OVERLAY_FS=y
CONFIG_AUDIT=y
CONFIG_SECURITY=y
CONFIG_SECURITY_NETWORK=y
CONFIG_SECURITY_SELINUX=y
CONFIG_SECURITY_SELINUX_BOOTPARAM=y
CONFIG_LSM="landlock,lockdown,yama,integrity,selinux,bpf"
CONFIG_ARM64_16K_PAGES=y
CONFIG_LOCALVERSION_AUTO=n
FRAG
echo "CONFIG_LOCALVERSION=\"$LOCALVERSION\"" >> "$OUT/limina-rpm.fragment"

VOL="limina-kbuild-${KVER}-16k"
container volume create -s 24g "$VOL" >/dev/null 2>&1 || true

DEPS="gcc make flex bison bc elfutils-libelf-devel openssl-devel perl findutils diffutils gzip xz cpio git rsync kmod python3 rpm-build"

echo "==> building limina-kernel-16k RPM ($KVER, 16k, fedora:$FEDORA_REL, -j$JOBS)"
container run --rm --cpus "$JOBS" --memory "$MEM" \
  -v "$(pwd)/$OUT:/out" \
  -v "$(pwd)/patches/linux:/patches" \
  -v "$VOL:/build" \
  "docker.io/library/fedora:$FEDORA_REL" bash -euo pipefail -c "
    echo '--- deps'; dnf -y install $DEPS >/dev/null
    CACHE=/out/cache/linux-$KVER.git
    if [ ! -d \"\$CACHE\" ]; then
      echo '--- shallow-cloning $KVER into cache'; mkdir -p /out/cache
      git clone --bare --depth 1 --branch '$KVER' https://github.com/torvalds/linux \"\$CACHE\"
    fi
    if [ ! -d /build/linux/.git ]; then
      echo '--- worktree (local clone)'; git clone \"\$CACHE\" /build/linux
    fi
    cd /build/linux
    echo '--- pristine base + limina patches, COMMITTED so the worktree is clean'
    # A clean worktree makes the kernel release a clean 6.12.0-limina16k (no -dirty). -dirty
    # has TWO dashes, which RPM rejects in version fields (Provides). Reset to the tag first so
    # this is idempotent across the warm build volume (prior runs leave a limina commit).
    git config --global user.email limina@local; git config --global user.name limina
    git config --global --add safe.directory /build/linux
    git reset --hard refs/tags/'$KVER' >/dev/null 2>&1 || git checkout -f -- .
    shopt -s nullglob
    for p in /patches/*.patch; do echo \"    applying \$(basename \"\$p\")\"; git apply \"\$p\"; done
    shopt -u nullglob
    git add -A && git commit -q -m 'limina enhanced-tier kernel patches'
    make ARCH=arm64 defconfig
    ./scripts/kconfig/merge_config.sh -m .config /out/limina-rpm.fragment
    make ARCH=arm64 olddefconfig
    grep -q '^CONFIG_ARM64_16K_PAGES=y' .config || { echo 'MISSING 16K' >&2; exit 1; }
    echo \"--- compiling Image + modules (-j\$(nproc))\"
    make ARCH=arm64 -j\"\$(nproc)\" Image modules
    # compute KREL AFTER the build: the build (re)generates include/config/kernel.release;
    # reading it earlier picks up a STALE value from a prior build-test-kernel.sh run in the
    # shared volume (cost a wrong-dir packaging bug — modules installed under the REAL release
    # dir while staging used the stale one). Must match what modules_install uses.
    KREL=\$(cat include/config/kernel.release)
    echo \"--- kernel release: \$KREL\"

    # Stage a buildroot exactly like a distro kernel package: modules under
    # /lib/modules/<KREL>/, the image as /lib/modules/<KREL>/vmlinuz (kernel-install copies it
    # to /boot), plus System.map/config. depmod runs via modules_install.
    STAGE=/build/stage; rm -rf \"\$STAGE\"; mkdir -p \"\$STAGE/lib/modules/\$KREL\"
    make ARCH=arm64 INSTALL_MOD_PATH=\"\$STAGE\" modules_install
    # drop the dangling build/source symlinks (point at the container builddir)
    rm -f \"\$STAGE/lib/modules/\$KREL/build\" \"\$STAGE/lib/modules/\$KREL/source\"
    cp -f arch/arm64/boot/Image \"\$STAGE/lib/modules/\$KREL/vmlinuz\"
    cp -f System.map \"\$STAGE/lib/modules/\$KREL/System.map\"
    cp -f .config \"\$STAGE/lib/modules/\$KREL/config\"

    # Minimal Fedora-style kernel RPM: ships /lib/modules/<KREL>, and on install runs
    # kernel-install (dracut initramfs + BLS entry, distro-standard root=UUID). %preun removes it.
    BASE=\$(make -s ARCH=arm64 kernelversion)      # e.g. 6.12.0
    mkdir -p /build/rpm/{BUILD,RPMS,SOURCES,SPECS,BUILDROOT}
    cat > /build/rpm/SPECS/limina-kernel.spec <<SPEC
%global krel \$KREL
%global debug_package %{nil}
%global __os_install_post %{nil}
%define _build_id_links none
Name:           limina-kernel-16k
Version:        \$BASE
Release:        1%{?dist}
Summary:        limina enhanced-tier 16 KiB-page Linux kernel ($KVER + limina patches)
License:        GPL-2.0-only
BuildArch:      aarch64
Provides:       kernel-uname-r = %{krel}
Requires(posttrans): systemd-udev
Requires(posttrans): dracut
Requires(preun): systemd-udev
%description
The limina enhanced tier kernel: 16 KiB pages (required for venus host-visible blobs on the
16 KiB-page macOS host) plus limina's virtio-gpu/scanout patches. Installs alongside the stock
kernel; GRUB offers both, falling back to stock 4 KiB if 16k ever fails.
%install
mkdir -p %{buildroot}/lib/modules
cp -a /build/stage/lib/modules/%{krel} %{buildroot}/lib/modules/
%files
/lib/modules/%{krel}
%posttrans
# Generate the initramfs (dracut) + BLS entry the distro-standard way.
kernel-install add %{krel} /lib/modules/%{krel}/vmlinuz || :
%preun
if [ \\\$1 -eq 0 ]; then kernel-install remove %{krel} || :; fi
SPEC
    echo '--- rpmbuild'
    rpmbuild --define '_topdir /build/rpm' -bb /build/rpm/SPECS/limina-kernel.spec
    mkdir -p /out/rpm
    cp -f /build/rpm/RPMS/aarch64/*.rpm /out/rpm/
    echo \"=== built: \$(ls /out/rpm/*.rpm) (KREL=\$KREL) ===\"
  "
echo "==> kernel RPM ready: $RPMOUT/"
ls -la "$RPMOUT"/*.rpm 2>/dev/null || true
