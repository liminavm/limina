#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Install the guest half of the native-context GPU transport into images, the way a guest gets
# it: the distro-agnostic vendor-driver payload (scripts/package-gpu-drivers.sh), run through its
# own installer (guest/gpu-drivers/install.sh). That puts the vendor drivers under
# /opt/limina/<wire>/ where the system's loaders find them, and the extended virtio-gpu module in
# through the distro's DKMS (docs/design/metal-transport-delivery.md §1).
#
#   scripts/provision/deliver-metal-transport.sh Fedora-Workstation-44.metal.test.raw ...
#
# Nothing selects the drivers but the device: booted with the transport the guest's compositor
# and clients run on it, booted without it the same guest is stock.
#
# Per image, in place: refuse if a live limina-vmm has it open; CoW backup
# `<image>.bak-pre-metal.raw` (LIMINA_DELIVER_BACKUP=0 skips it); boot through the default vehicle
# without the transport; install (an upgrade in place when a payload is already there); verify
# that the module resolves to DKMS's and that any copy in the initramfs is that one; clean
# poweroff.
#
# Inputs:
#   LIMINA_GPU_DRIVERS  the payload (default target/gpu-drivers/limina-gpu-drivers.tar.gz)
# Env otherwise as deliver-payload.sh (LIMINA_GUEST_USER, LIMINA_DELIVER_LOGDIR,
# LIMINA_DELIVER_SSH_TIMEOUT, and what the boot vehicle honours).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

[ "$#" -ge 1 ] || { echo "usage: deliver-metal-transport.sh <image.raw>..." >&2; exit 2; }
for img in "$@"; do [ -f "$img" ] || { echo "no such image: $img" >&2; exit 2; }; done

PAYLOAD="${LIMINA_GPU_DRIVERS:-target/gpu-drivers/limina-gpu-drivers.tar.gz}"
[ -f "$PAYLOAD" ] || { echo "no payload at $PAYLOAD (scripts/package-gpu-drivers.sh)" >&2; exit 2; }

USER_="${LIMINA_GUEST_USER:-claude}"
LOGDIR="${LIMINA_DELIVER_LOGDIR:-/tmp}"
SSH_TIMEOUT="${LIMINA_DELIVER_SSH_TIMEOUT:-420}"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
echo "payload $PAYLOAD ($(shasum -a 256 "$PAYLOAD" | cut -c1-12)…)"

deliver() {
  local img="$1" base name log boot port ilog
  base="${img%.raw}"; name="$(basename "$base")"
  log="/tmp/limina-worker-$name.log"
  ilog="$LOGDIR/limina-deliver-$name-metal.install.log"
  echo "=== $img  $(date '+%F %T')"

  if pgrep -f "limina-vmm.*$(basename "$img")" >/dev/null; then
    echo "!!! a live limina-vmm has $img open — refusing" >&2
    return 1
  fi
  if [ "${LIMINA_DELIVER_BACKUP:-1}" != "0" ]; then
    if [ -e "$base.bak-pre-metal.raw" ]; then
      echo "backup $base.bak-pre-metal.raw already exists; keeping it"
    else
      cp -c "$img" "$base.bak-pre-metal.raw"
      echo "backup: $base.bak-pre-metal.raw"
    fi
  fi

  # An empty worker log first: wait-guest-ssh reads the last forward line in it (see
  # deliver-payload.sh for the wrong-VM trap a stale one sets).
  rm -f "$log"
  LIMINA_DISK="$img" LIMINA_BOOT_LOG="$log" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh \
    >"$LOGDIR/limina-deliver-$name-metal.boot.log" 2>&1 &
  boot=$!
  port="$(scripts/wait-guest-ssh.sh "$log" "$SSH_TIMEOUT" "$boot")" || true
  if [ -z "$port" ]; then
    echo "!!! no SSH port for $img (see $log) — abandoning this image" >&2
    kill "$boot" 2>/dev/null || true
    wait "$boot" 2>/dev/null || true
    return 1
  fi
  echo "guest up: ssh -p $port $USER_@127.0.0.1 (worker log $log)"
  local g=(ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1")
  off() { "${g[@]}" 'sudo systemctl poweroff -i' || true; wait "$boot" || true; }

  scp -P "$port" "${SSH_OPTS[@]}" "$PAYLOAD" "$USER_@127.0.0.1:limina-gpu-drivers.tar.gz"
  if "${g[@]}" 'set -e
      rm -rf limina-gpu-drivers
      tar xzf limina-gpu-drivers.tar.gz
      sudo ./limina-gpu-drivers/install.sh
      rm -rf limina-gpu-drivers limina-gpu-drivers.tar.gz' >"$ilog" 2>&1; then
    echo "installed (log $ilog)"
  else
    echo "!!! install FAILED (log $ilog); tail:" >&2; tail -15 "$ilog" >&2
    off
    return 1
  fi

  local ok=1 krel got
  krel="$("${g[@]}" 'uname -r')"
  grep -q "module installed for $krel" "$ilog" ||
    { echo "!!! the module did not build for $krel (log $ilog)" >&2; ok=0; }
  # DKMS installs to extra/, which depmod ranks below updates/: a virtio-gpu left in updates/
  # (by hand, or by another package) would shadow ours.
  got="$("${g[@]}" "modinfo -k '$krel' -n virtio_gpu")"
  case "$got" in
    */extra/virtio-gpu.ko*) ;;
    *) echo "!!! virtio_gpu resolves to $got, not DKMS's" >&2; ok=0 ;;
  esac
  # Early boot binds virtio_gpu from the initramfs when it carries one, before the root is up, so
  # any copy it carries must be ours. Carrying none is fine: the root's then binds.
  local inird
  inird="$("${g[@]}" "sudo lsinitrd --kver '$krel'" | grep -E '/virtio-gpu\.ko' || true)"
  if [ -z "$inird" ]; then
    echo "the initramfs carries no virtio-gpu module; the root's binds"
  elif ! grep -qv '/extra/virtio-gpu\.ko' <<<"$inird"; then
    echo "the initramfs carries the extended module"
  else
    echo "!!! the initramfs for $krel carries a virtio-gpu module that is not ours: $inird" >&2; ok=0
  fi
  [ "$ok" = 1 ] && echo "verified: the module is DKMS's for $krel, in the root and the initramfs"

  "${g[@]}" 'sudo fstrim -av' 2>&1 | sed 's/^/   trim: /' || true
  off
  echo "=== done $img  $(date '+%F %T')"
  [ "$ok" = 1 ]
}

failed=()
for img in "$@"; do
  deliver "$img" || failed+=("$img")
done
if [ "${#failed[@]}" -gt 0 ]; then
  echo "FAILED: ${failed[*]}" >&2
  exit 1
fi
echo "delivered the vendor GPU drivers to: $*"
