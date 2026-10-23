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
# The drivers drive every virtio-gpu, in place of the system Mesa's: zink on KosmicKrisp over a
# limina transport, virgl elsewhere, for GL; KosmicKrisp and venus for Vulkan; virgl for video.
# Each is found where the system's own loaders look (glvnd's vendor directory, the Vulkan loader's
# ICD directory, libgbm's backend directory) and a Vulkan layer hides the system's venus device
# behind ours. Only libva has no such directory: LIBVA_DRIVERS_PATH, set in environment.d, puts
# ours ahead of the system's. GLX clients and Flatpaks keep the system's drivers.
#
# Only the newest protocol's tree drives virgl, venus and video; an older tree beside it serves
# just its own transport's Vulkan.
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

# need_packages [user]: the prerequisites, by this distro's names: with "user", only what the
# vendor drivers need; otherwise also what DKMS needs, whose kernel headers must be the running
# kernel's (DKMS builds for later kernels as they are installed). Headers already in place are not
# asked for: a kernel that is not the distro's (limina's enhanced one) ships its own, under a name
# the distro does not have.
need_packages() {
  local kver; kver="$(uname -r)"
  local have_headers=""
  [ -e "/lib/modules/$kver/build/Makefile" ] && have_headers=1
  local user=() kernel=()
  case "$pm" in
    dnf)
      user=(vulkan-loader libglvnd-egl)
      kernel=(dkms gcc make)
      [ -n "$have_headers" ] || kernel+=("kernel-devel-$kver")
      # Fedora's dkms requires kernel-devel-matched whenever kernel-core is installed. Left to dnf
      # that is the newest one, which brings the newest kernel with it, half installed and booted
      # by default: name an installed kernel-core's.
      if rpm -q kernel-core >/dev/null 2>&1; then
        kernel+=("kernel-devel-matched-$(rpm -q --qf '%{VERSION}-%{RELEASE}.%{ARCH}\n' kernel-core \
          | sort -V | tail -n 1)")
      fi ;;
    apt-get)
      user=(libvulkan1 libegl1)
      kernel=(dkms gcc make)
      [ -n "$have_headers" ] || kernel+=("linux-headers-$kver") ;;
    pacman)
      user=(vulkan-icd-loader libglvnd)
      kernel=(dkms gcc make)
      # Arch names the headers after the kernel package, which uname does not say: take the one
      # owning the running kernel's modules.
      local kpkg
      kpkg="$(pacman -Qqo "/usr/lib/modules/$kver" 2>/dev/null | head -n 1)"
      [ -n "$have_headers" ] || kernel+=("${kpkg:-linux}-headers") ;;
    zypper)
      user=(libvulkan1 libglvnd)
      kernel=(dkms gcc make)
      [ -n "$have_headers" ] || kernel+=(kernel-default-devel) ;;
    *) die "no package manager this installer knows (dnf, apt-get, pacman, zypper)" ;;
  esac
  [ "${1:-}" = user ] && kernel=()
  case "$pm" in
    dnf) dnf install -y "${user[@]}" "${kernel[@]}" ;;
    apt-get) DEBIAN_FRONTEND=noninteractive apt-get install -y "${user[@]}" "${kernel[@]}" ;;
    pacman) pacman -S --needed --noconfirm "${user[@]}" "${kernel[@]}" ;;
    zypper) zypper --non-interactive install "${user[@]}" "${kernel[@]}" ;;
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

# Whether the initramfs of `kver` carries a virtio-gpu module; unknown (0) where it cannot be read.
initramfs_has_module() {
  local kver="$1" list
  if command -v lsinitrd >/dev/null; then list="$(lsinitrd --kver "$kver" 2>/dev/null)"
  elif command -v lsinitramfs >/dev/null; then list="$(lsinitramfs "/boot/initrd.img-$kver" 2>/dev/null)"
  else return 0
  fi
  grep -q '/virtio[-_]gpu\.ko' <<<"$list"
}

# initramfs-tools collects GPU drivers by their path under kernel/drivers/gpu, and DKMS moves the
# in-tree virtio-gpu out of there while its own copy lives under updates/: without this hook the
# initramfs carries no virtio-gpu at all, and nothing draws early boot, a LUKS prompt included.
# Asking for it by name finds whichever copy modprobe would load.
INITRAMFS_HOOK=/etc/initramfs-tools/hooks/limina-virtio-gpu
initramfs_hook() {
  [ -d /etc/initramfs-tools/hooks ] || return 0
  cat > "$INITRAMFS_HOOK" <<'HOOK'
#!/bin/sh
# Installed by limina's GPU drivers: carry the virtio-gpu module DKMS built (see their install.sh).
case "$1" in prereqs) exit 0 ;; esac
. /usr/share/initramfs-tools/hook-functions
manual_add_modules virtio_gpu
HOOK
  chmod 755 "$INITRAMFS_HOOK"
  record "file $INITRAMFS_HOOK"
}

# Where the system's libgbm looks for backends: the directory beside the libgbm it loads.
gbm_backend_dir() {
  local lib
  # awk reads to the end: an early exit fails ldconfig with SIGPIPE under pipefail.
  lib="$(ldconfig -p | awk '/libgbm\.so\.1 / && !f {print $NF; f=1}')"
  [ -n "$lib" ] || return 1
  printf '%s/gbm\n' "$(dirname "$(readlink -f "$lib")")"
}

# --- the manifest ---------------------------------------------------------------------------

record() { printf '%s\n' "$*" >> "$STATE/manifest.new"; }

# Where a file this edits is kept as it was. Never beside it: pam_env reads every file in
# /etc/environment.d, not only *.conf, so a backup there overrides the edit in every login that
# is not systemd's.
original() { printf '%s/originals/%s\n' "$STATE" "$(printf '%s' "$1" | tr / %)"; }

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
      # A manifest from before the drivers stopped editing the environment. A file removed since
      # was removed on purpose (install-enhanced.sh retires the enhanced tier's): it stays gone.
      envfile) [ -f "$what" ] && [ -f "$(original "$what")" ] && mv -f "$(original "$what")" "$what" ;;
      file) rm -f "$what" ;;
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

# A kernel with virtio-gpu built in cannot take the extended module: it either carries the
# extension itself or cannot offer the transport, and DKMS has nothing to do for it either way.
builtin=""
[ "$(modinfo -n virtio_gpu 2>/dev/null)" = "(builtin)" ] && builtin=1

say "prerequisites ($pm)"
if [ -n "$builtin" ]; then need_packages user; else need_packages; fi

# The vendor drivers: one tree per protocol version this payload carries.
for tree in "$HERE"/drivers/opt/limina/*/; do
  wire="$(basename "$tree")"
  say "drivers for protocol $wire -> /opt/limina/$wire"
  rm -rf "/opt/limina/$wire.new"
  mkdir -p /opt/limina
  cp -a "$tree" "/opt/limina/$wire.new"
  # `cp -a` keeps whoever unpacked the payload as owner, and the compositor loads these.
  chown -R root:root "/opt/limina/$wire.new"
  rm -rf "/opt/limina/$wire"
  mv "/opt/limina/$wire.new" "/opt/limina/$wire"
  record "tree /opt/limina/$wire"
  link "/opt/limina/$wire/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json" \
    "/etc/vulkan/icd.d/limina-$wire.json"
done
command -v restorecon >/dev/null && restorecon -R /opt/limina || true

# What a guest has one of: the newest protocol's takes it. libgbm loads one backend per DRM driver
# name, and glvnd asks its vendors in turn, the first that takes a display keeping it.
newest="$(ls /opt/limina | sort -n | tail -n 1)"
N="/opt/limina/$newest"
link "$N/share/glvnd/egl_vendor.d/10_limina-$newest.json" /etc/glvnd/egl_vendor.d/10_limina.json
# A wire-independent name, for a client that names its Vulkan drivers.
link "$N/share/vulkan/icd.d/virtio_icd.aarch64.json" /etc/vulkan/icd.d/limina-venus.json
link "$N/share/vulkan/implicit_layer.d/limina-$newest-transport-select.json" \
  /etc/vulkan/implicit_layer.d/limina-transport-select.json
if gbmdir="$(gbm_backend_dir)"; then
  link "$N/lib/gbm/dri_gbm.so" "$gbmdir/virtio_gpu_gbm.so"
else
  say "no libgbm on this system: the compositor keeps its stock driver"
fi

# libva searches one path list, which only the environment sets. The system's own directories
# follow ours, so a driver we do not carry is still found.
vadirs="$N/lib/dri"
libva="$(ldconfig -p | awk '/libva\.so\.2 / && !f {print $NF; f=1}')"
if [ -n "$libva" ]; then
  libdir="$(dirname "$(readlink -f "$libva")")"
  for d in "$libdir/dri-nonfree" "$libdir/dri-freeworld" "$libdir/dri"; do
    [ -d "$d" ] && vadirs="$vadirs:$d"
  done
fi
VA_ENV=/etc/environment.d/90-limina-va.conf
mkdir -p /etc/environment.d
printf 'LIBVA_DRIVERS_PATH=%s\n' "$vadirs" > "$VA_ENV"
record "file $VA_ENV"
# GStreamer caches its plugin scan per user and re-validates an entry only when the plugin file
# itself changes: a VA element blacklisted under the previous driver stays so. The cache is
# rebuildable; drop it, and the next GStreamer start rescans against these drivers.
for home in /home/* /root; do
  [ -d "$home/.cache/gstreamer-1.0" ] || continue
  rm -f "$home/.cache/gstreamer-1.0"/registry.*.bin
done

if [ -n "$builtin" ]; then
  say "virtio-gpu is built into $(uname -r), so no module is installed: the transport is there"
  say "only if that kernel carries the extension itself"
else
  # The module, through the distro's DKMS: it rebuilds for every kernel installed later.
  dkms_version="$(cat "$HERE/dkms/VERSION")"
  src="/usr/src/$DKMS_NAME-$dkms_version"
  rm -rf "$src"
  cp -a "$HERE/dkms/tree" "$src"
  chown -R root:root "$src"
  record "dkms $DKMS_NAME/$dkms_version"
  dkms add "$DKMS_NAME/$dkms_version"
  kver="$(uname -r)"
  initramfs_hook
  if dkms install "$DKMS_NAME/$dkms_version" -k "$kver"; then
    # Early boot binds virtio-gpu from the initramfs, which still holds the in-tree driver.
    rebuild_initramfs "$kver"
    initramfs_has_module "$kver" ||
      say "WARNING: the initramfs of $kver carries no virtio-gpu: early boot, a disk unlock prompt included, will not be shown"
    say "module installed for $kver"
  else
    say "the module did not build for $kver: the in-tree driver stays and the transport stays off"
  fi
fi

mv "$STATE/manifest.new" "$STATE/manifest"
say "installed; reboot to load the new drivers"
