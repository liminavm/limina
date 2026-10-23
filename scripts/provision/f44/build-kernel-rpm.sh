#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the limina enhanced-tier 16 KiB-page kernel as a Fedora RPM, NATIVELY INSIDE an F44 guest.
#
# Per the "Fedora config for the most part" goal, this does NOT use a bare upstream `make
# defconfig` (the retired F43 builder did). Instead it starts from the real
# Fedora config (the running kernel's, or the image's kernel-core in the build container) on a
# matching upstream source tree, and applies the
# single load-bearing delta — CONFIG_ARM64_16K_PAGES=y — plus a few build-hygiene flips so a
# Fedora config builds cleanly outside Fedora's kernel.spec (neutralize the Fedora signing-cert
# paths; keep the VM-critical drivers =y so boot never depends on initramfs contents). Packaged
# via the same minimal Fedora-style spec the F43 build proved (ships /lib/modules/<KREL>,
# %posttrans `kernel-install add` so dracut writes the initramfs + BLS entry, co-installs beside
# stock). The 16k kernel is distro-independent: the SAME RPM installs on F43 and F44.
#
# Source: the liminavm/linux fork at the rev pinned in third_party/manifest.toml (no patch
# series any more — see that file and guest/virtio-gpu-dkms/README.md for what moved where).
#
# THE INTRICATE ONE — most likely to need an in-guest tweak. Knobs: KREV (fork rev override),
# CONFIG_BASE (base config). If the Fedora-config build fights you, the validated fallback is
# the upstream-defconfig recipe of the retired F43 builder (`git log -- scripts/build-kernel-rpm.sh`).
#
# Usage (in the guest):  scripts/provision/f44/build-kernel-rpm.sh
# Output: $OUT/limina-kernel-16k-*.rpm and its -devel  (default ~/limina-build/kernel)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
OUT="${OUT:-$HOME/limina-build/kernel}"
BUILD="${BUILD:-$HOME/limina-build/linux}"
# RPM Release. Bump it (RELEASE=2) whenever the CONTENT changes but the kernel Version does not —
# e.g. a fork-branch change at the same base. Same NEVRA + different content is the classic trap:
# dnf/rpm see the installed package as identical and skip the upgrade.
RELEASE="${RELEASE:-1}"
# The release must ALSO land in KERNELRELEASE: the kernel package is install-only, so a -2 RPM
# with the same /lib/modules/<KREL> as the installed -1 hits file conflicts instead of upgrading
# (bit us 2026-08-19 delivering 7.1.8-2). Distinct KRELs is the designed behavior anyway — the
# new kernel coexists as its own GRUB entry and the previous one stays a bootable fallback.
if [ "$RELEASE" = 1 ]; then
    LOCALVERSION="${LOCALVERSION:--limina16k}"
else
    LOCALVERSION="${LOCALVERSION:--limina16k.$RELEASE}"
fi
# The kernel Makefile appends BOTH the make-visible $LOCALVERSION and CONFIG_LOCALVERSION to
# KERNELRELEASE. We write CONFIG_LOCALVERSION from this variable below, so if the caller EXPORTED
# it (`LOCALVERSION=-foo build-kernel-rpm.sh`) it lands twice — 7.1.6-foo-foo — and rpmbuild then
# rejects the double separator at the very end of a ~40-minute build. Un-export it so only the
# config fragment carries it. (Bit us 2026-08-03 building the no-fence probe kernel.)
export -n LOCALVERSION 2>/dev/null || true
# Base config: Fedora's own kernel config, because "Fedora config for the most part" is the
# goal. In a guest that is the RUNNING kernel's. In the unified build container it cannot be:
# `uname -r` there reports the container host's kernel, which is not a Fedora package and has
# no /boot/config — so fall back to the config of whatever kernel-core the image installed,
# which pins the base to a package version rather than to whatever a guest was booted on.
if [ -z "${CONFIG_BASE:-}" ]; then
    for c in "/boot/config-$(uname -r)" \
             "$(ls -1t /boot/config-* 2>/dev/null | head -1)" \
             "$(ls -1t /lib/modules/*/config 2>/dev/null | head -1)"; do
        [ -n "$c" ] && [ -r "$c" ] && { CONFIG_BASE="$c"; break; }
    done
fi
[ -n "${CONFIG_BASE:-}" ] && [ -r "$CONFIG_BASE" ] || {
    echo "no Fedora kernel config found (looked at /boot/config-*, /lib/modules/*/config)." >&2
    echo "In a guest that means no kernel-core; in a container, the image is missing it." >&2
    echo "Point CONFIG_BASE at one explicitly to override." >&2
    exit 1
}
echo "==> base config: $CONFIG_BASE"
# Source: the liminavm/linux fork's `limina` branch at the rev pinned in
# third_party/manifest.toml. There is no patch-apply stage any more — our kernel changes ARE
# the commits on that branch, so what gets built is exactly what the pin names.
# Committed values only (`pin`): a local override names a HOST path, so it is resolved by whoever
# can see one — scripts/build-enhanced-rpms.sh passes FORK_URL/KREV in for it.
# shellcheck source=scripts/lib/manifest.sh
LIMINA_ROOT="$REPO" . "$REPO/scripts/lib/manifest.sh"
FORK_URL="${FORK_URL:-$(pin linux repo)}"
KREV="${KREV:-$(pin linux rev)}"
KVER="${KVER:-$(pin linux base)}"     # informational: the upstream tag the branch sits on
[ -n "$FORK_URL" ] && [ -n "$KREV" ] || {
  echo "could not read the [linux] pin from $REPO/third_party/manifest.toml" >&2; exit 1; }
mkdir -p "$OUT"
[ -r "$CONFIG_BASE" ] || { echo "base config $CONFIG_BASE not readable; set CONFIG_BASE" >&2; exit 1; }

echo "==> [1/6] kernel build deps"
command -v rpmbuild >/dev/null || sudo dnf install -y rpm-build rpmdevtools
sudo dnf -y builddep kernel 2>/dev/null || \
  sudo dnf install -y gcc make flex bison bc elfutils-libelf-devel openssl-devel perl \
    rsync kmod gzip diffutils findutils git dwarves

echo "==> [2/6] source tree (fork rev ${KREV:0:12}, base $KVER, shallow)"
# Fetch the pinned rev EXACTLY (not the branch tip): a moved branch can never silently change
# what this builds, and the fetch stays a single shallow commit.
# A tree left over from the pre-fork recipe points `origin` at kernel.org (which does not have
# our revs), so re-init unless origin is already the fork — otherwise the fetch below fails with
# a confusing "not our commit" error.
if [ -d "$BUILD/.git" ] && \
   [ "$(git -C "$BUILD" remote get-url origin 2>/dev/null)" != "$FORK_URL" ]; then
  echo "    existing tree points elsewhere ($(git -C "$BUILD" remote get-url origin 2>/dev/null || echo none)) — re-initialising"
  rm -rf "$BUILD"
fi
if [ ! -d "$BUILD/.git" ]; then
  rm -rf "$BUILD"; mkdir -p "$BUILD"
  git -C "$BUILD" init -q
  git -C "$BUILD" remote add origin "$FORK_URL"
fi
cd "$BUILD"
git config --global --add safe.directory "$BUILD" 2>/dev/null || true
if ! git cat-file -e "${KREV}^{commit}" 2>/dev/null; then
  git fetch -q --depth 1 origin "$KREV" \
    || { echo "could not fetch $KREV from $FORK_URL — is the manifest pin pushed? (unpushed:" \
              "set [linux] source in third_party/manifest.local.toml)" >&2; exit 1; }
fi
git checkout -q --detach "$KREV"
git reset --hard -q "$KREV"
git clean -qfdx 2>/dev/null || true
echo "    at $(git log -1 --format='%h %s')"

echo "==> [3/6] (no patch stage — the fork branch IS the patch series)"
# setlocalversion (which in current kernels IGNORES an empty .scmversion) would append the
# fork's SCM info to the release — e.g. 7.1.6-limina16k-g8a5647a014f5, whose extra dash is an
# INVALID rpm EVR for `Provides: kernel-uname-r`, so packaging fails. Remove the git metadata
# so setlocalversion finds no SCM and emits NO suffix → clean 7.1.6-limina16k. (Re-runs
# re-fetch via the step-2 guard; a shallow fetch is cheap.)
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
# virtio-gpu is the exception: a module, so the vendor GPU drivers' extended one (DKMS, against
# the -devel package below) can take its place. Boot does not depend on it: the console is the
# PL011 and root is virtio-blk, and install-enhanced.sh has dracut carry it for the splash.
CONFIG_DRM_VIRTIO_GPU=m
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
# The headers and makefiles an out-of-tree module builds against, by the kernel's own recipe (the
# one its `make rpm-pkg` uses for kernel-devel). `build` points there, dangling until -devel is in.
DEVEL="$BUILD/devel/usr/src/kernels/$KREL"
rm -rf "$BUILD/devel"
make ARCH=arm64 run-command \
  KBUILD_RUN_COMMAND="\${srctree}/scripts/package/install-extmod-build $DEVEL"
ln -fns "/usr/src/kernels/$KREL" "$STAGE/lib/modules/$KREL/build"

echo "==> [6/6] package limina-kernel-16k RPMs"
rpmdev-setuptree
SPEC="$HOME/rpmbuild/SPECS/limina-kernel.spec"
cat > "$SPEC" <<SPEC
%global krel $KREL
%global debug_package %{nil}
%global __os_install_post %{nil}
%define _build_id_links none
Name:           limina-kernel-16k
Version:        $BASE
Release:        $RELEASE%{?dist}
Summary:        limina enhanced-tier 16 KiB-page Linux kernel ($KVER + limina fork, Fedora config + 16k)
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
%package devel
Summary:        Headers and makefiles to build modules for the limina 16 KiB-page kernel %{krel}
AutoReqProv:    no
Provides:       kernel-devel-uname-r = %{krel}
%description devel
What an out-of-tree module (DKMS) builds against for the limina enhanced-tier kernel %{krel}.
%install
mkdir -p %{buildroot}/lib/modules %{buildroot}/usr/src/kernels
cp -a $STAGE/lib/modules/%{krel} %{buildroot}/lib/modules/
cp -a $DEVEL %{buildroot}/usr/src/kernels/
%files
/lib/modules/%{krel}
%files devel
/usr/src/kernels/%{krel}
%posttrans
kernel-install add %{krel} /lib/modules/%{krel}/vmlinuz || :
%preun
if [ \$1 -eq 0 ]; then kernel-install remove %{krel} || :; fi
SPEC
rpmbuild -bb "$SPEC"
# Copy out ONLY the RPM this run produced. A glob over rpmbuild/RPMS/ sweeps in every kernel ever
# built in this guest, and install-enhanced.sh picks with `head -1` — which sorts to the OLDER
# version. That made a build "succeed" while the payload carried a stale kernel, twice (2026-08-03).
RPMS="$HOME/rpmbuild/RPMS/aarch64"
NVRA="$BASE-$RELEASE.fc$(rpm -E %{fedora}).aarch64"
RPM="$RPMS/limina-kernel-16k-$NVRA.rpm"
DEVEL_RPM="$RPMS/limina-kernel-16k-devel-$NVRA.rpm"
for r in "$RPM" "$DEVEL_RPM"; do
  [ -f "$r" ] || { echo "expected RPM not found: $r" >&2; exit 1; }
done
# ...and CLEAN $OUT first, or the same head -1 bug returns by the other door: copying only this
# run's RPM still leaves every PREVIOUS run's kernel sitting in $OUT, and build-all.sh globs the
# whole directory into the payload. $OUT is this run's output, not an archive of past ones.
rm -f "$OUT"/*.rpm
cp -f "$RPM" "$DEVEL_RPM" "$OUT"/
ls -la "$OUT"/*.rpm 2>/dev/null || { echo "(no kernel RPM produced)"; exit 1; }
echo "    KREL=$KREL"

# The source reference the payload ships beside the RPM: the kernel's corresponding source is the
# fork at this rev plus this config, so that is what it names.
tmpd=$(mktemp -d); kdst="$tmpd/limina-kernel-16k-source"; mkdir -p "$kdst"
cp -f "$BUILD/.config" "$kdst/config"
cp -f "$REPO/third_party/manifest.toml" "$kdst/manifest.toml"
cp -f "$HERE/build-kernel-rpm.sh" "$kdst/"
# The URL is always the fork's: a local override's file:// path means nothing to a reader.
PIN_URL="$(pin linux repo)"
cat > "$kdst/SOURCE.txt" <<TXT
limina-kernel-16k source reference
==================================
This kernel is NOT built from a Fedora SRPM. It is the limina kernel fork — upstream
stable $KVER plus our commits — with Fedora's config + CONFIG_ARM64_16K_PAGES=y, packaged
by build-kernel-rpm.sh. There is no patch series to apply: our changes ARE the commits on
the branch. Fully reproducible:

  source:    git init && git fetch --depth 1 $PIN_URL \\
             $KREV
  our delta: git log --oneline $KVER..$KREV
  config:    ./config   (also shipped inside the RPM at /lib/modules/<KREL>/config)
  pin:       ./manifest.toml  ([linux] section — repo/branch/rev/base)
  build:     ./build-kernel-rpm.sh

Upstream-facing status of each commit: docs/upstreaming/ledger/linux.md in the limina repo.
TXT
tar -czf "$OUT/limina-kernel-16k-source.tar.gz" -C "$tmpd" limina-kernel-16k-source
rm -rf "$tmpd"
echo "    source reference: $OUT/limina-kernel-16k-source.tar.gz"
