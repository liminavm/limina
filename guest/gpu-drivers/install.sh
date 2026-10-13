#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
# Install limina's guest GPU drivers on any distro: the vendor drivers under /opt/limina/<wire>/
# (docs/design/metal-transport-delivery.md §1) and the virtio-gpu module through the distro's
# DKMS. Run as root from the unpacked payload:
#
#   sudo ./install.sh              install, or upgrade in place
#   sudo ./install.sh --uninstall  remove everything this installed
#
# Nothing here selects a driver with the environment. Each driver is found where the system's own
# loaders look (glvnd's vendor directory, the Vulkan loader's ICD directory, libgbm's backend
# directory) and declines a device that is not a limina transport speaking its protocol, so a
# guest without the transport, or on a host that speaks another protocol, keeps its stock drivers.
#
# What the distro differs in is confined to two places: the package names the prerequisites go by
# (need_packages) and the tool that rebuilds the initramfs (rebuild_initramfs). Everything this
# installs is recorded in $STATE/manifest, which --uninstall reads.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
STATE=/var/lib/limina-gpu-drivers
DKMS_NAME=limina-virtio-gpu

say() { printf '==> %s\n' "$*"; }
die() { printf '!!! %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root"

# --- the distro -----------------------------------------------------------------------------

pm=""
for p in dnf apt-get pacman zypper; do
  if command -v "$p" >/dev/null; then pm="$p"; break; fi
done

# need_packages: the prerequisites, by this distro's names. The kernel headers must be the running
# kernel's; DKMS builds for later kernels as they are installed.
need_packages() {
  local kver; kver="$(uname -r)"
  case "$pm" in
    # Fedora's dkms requires kernel-devel-matched, which left to itself is the newest one and
    # brings the newest kernel with it, half installed and booted by default: name the running
    # kernel's.
    dnf)
      dnf install -y dkms "kernel-devel-$kver" "kernel-devel-matched-$kver" gcc make \
        vulkan-loader libglvnd-egl ;;
    apt-get)
      DEBIAN_FRONTEND=noninteractive apt-get install -y dkms "linux-headers-$kver" gcc make \
        libvulkan1 libegl1 ;;
    pacman)
      # Arch names the headers after the kernel package, which uname does not say: take the one
      # owning the running kernel's modules.
      local kpkg
      kpkg="$(pacman -Qqo "/usr/lib/modules/$kver" 2>/dev/null | head -n 1)"
      pacman -S --needed --noconfirm dkms "${kpkg:-linux}-headers" gcc make vulkan-icd-loader \
        libglvnd ;;
    zypper) zypper --non-interactive install dkms kernel-default-devel gcc make libvulkan1 libglvnd ;;
    *) die "no package manager this installer knows (dnf, apt-get, pacman, zypper)" ;;
  esac
}

rebuild_initramfs() {
  local kver="$1"
  if command -v dracut >/dev/null; then dracut -f --kver "$kver"
  elif command -v update-initramfs >/dev/null; then update-initramfs -u -k "$kver"
  elif command -v mkinitcpio >/dev/null; then mkinitcpio -P
  else say "no initramfs tool found: the extended module loads after the switch to the root fs"
  fi
}

# Where the system's libgbm looks for backends: the directory beside the libgbm it loads.
gbm_backend_dir() {
  local lib
  lib="$(ldconfig -p | awk '/libgbm\.so\.1 / {print $NF; exit}')"
  [ -n "$lib" ] || return 1
  printf '%s/gbm\n' "$(dirname "$(readlink -f "$lib")")"
}

# --- the manifest ---------------------------------------------------------------------------

record() { printf '%s\n' "$*" >> "$STATE/manifest.new"; }

link() {
  local target="$1" at="$2"
  mkdir -p "$(dirname "$at")"
  ln -sfn "$target" "$at"
  record "link $at"
}

uninstall() {
  [ -f "$STATE/manifest" ] || { say "nothing installed"; return 0; }
  local kind what rest
  while read -r kind what rest; do
    case "$kind" in
      link) [ -L "$what" ] && rm -f "$what" ;;
      tree) rm -rf "$what" ;;
      dkms) dkms remove "$what" --all || true; rm -rf "/usr/src/${what/\//-}" ;;
      envfile) [ -f "$what.limina-orig" ] && mv -f "$what.limina-orig" "$what" ;;
    esac
  done < "$STATE/manifest"
  rmdir /opt/limina 2>/dev/null || true
  rm -rf "$STATE"
  rebuild_initramfs "$(uname -r)"
  say "uninstalled; the stock drivers take over at the next boot"
}

if [ "${1:-}" = "--uninstall" ]; then
  uninstall
  exit 0
fi

# --- install --------------------------------------------------------------------------------

# A previous install goes first, so an upgrade leaves exactly what this payload carries.
if [ -f "$STATE/manifest" ]; then
  say "removing the previous install"
  uninstall
fi
mkdir -p "$STATE"
: > "$STATE/manifest.new"

say "prerequisites ($pm)"
need_packages

# The vendor drivers: one tree per protocol version this payload carries.
for tree in "$HERE"/drivers/opt/limina/*/; do
  wire="$(basename "$tree")"
  say "drivers for protocol $wire -> /opt/limina/$wire"
  rm -rf "/opt/limina/$wire.new"
  mkdir -p /opt/limina
  cp -a "$tree" "/opt/limina/$wire.new"
  rm -rf "/opt/limina/$wire"
  mv "/opt/limina/$wire.new" "/opt/limina/$wire"
  record "tree /opt/limina/$wire"
  link "/opt/limina/$wire/share/glvnd/egl_vendor.d/10_limina-$wire.json" \
    "/etc/glvnd/egl_vendor.d/10_limina-$wire.json"
  link "/opt/limina/$wire/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json" \
    "/etc/vulkan/icd.d/limina-$wire.json"
done
command -v restorecon >/dev/null && restorecon -R /opt/limina || true

# libgbm loads one backend per DRM driver name, so the newest protocol's takes the name.
newest="$(ls /opt/limina | sort -n | tail -n 1)"
if gbmdir="$(gbm_backend_dir)"; then
  link "/opt/limina/$newest/lib/gbm/dri_gbm.so" "$gbmdir/virtio_gpu_gbm.so"
else
  say "no libgbm on this system: the compositor keeps its stock driver"
fi

# A Vulkan driver list pinned in the environment (the enhanced tier's venus pin) would hide ours
# from every client and from zink itself; ours joins it rather than replacing it.
for f in /etc/environment.d/*.conf /etc/environment; do
  [ -f "$f" ] && grep -q '^VK_DRIVER_FILES=' "$f" || continue
  cp -p "$f" "$f.limina-orig"
  icds="$(ls /etc/vulkan/icd.d/limina-*.json | tr '\n' ':')"
  sed -i "s|^VK_DRIVER_FILES=|VK_DRIVER_FILES=${icds}|" "$f"
  record "envfile $f"
  say "added the limina ICDs to the Vulkan driver list in $f"
done

# The module, through the distro's DKMS: it rebuilds for every kernel installed later.
dkms_version="$(cat "$HERE/dkms/VERSION")"
src="/usr/src/$DKMS_NAME-$dkms_version"
rm -rf "$src"
cp -a "$HERE/dkms/tree" "$src"
record "dkms $DKMS_NAME/$dkms_version"
dkms add "$DKMS_NAME/$dkms_version"
kver="$(uname -r)"
if dkms install "$DKMS_NAME/$dkms_version" -k "$kver"; then
  # Early boot binds virtio-gpu from the initramfs, which still holds the in-tree driver.
  rebuild_initramfs "$kver"
  say "module installed for $kver"
else
  say "the module did not build for $kver: the in-tree driver stays and the transport stays off"
fi

mv "$STATE/manifest.new" "$STATE/manifest"
say "installed; reboot to load the extended module"
