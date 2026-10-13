#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Install the guest half of the native-context GPU transport into images: the virtio-gpu module
# with limina's extension (guest/virtio-gpu-limina) and KosmicKrisp on the limina bridge, as a
# Vulkan driver beside the stock ones.
#
#   scripts/provision/deliver-metal-transport.sh Fedora-Workstation-44.metal.test.raw ...
#
# The module is built for ONE kernel release, so this refuses an image whose running kernel is not
# that release. The driver goes under /usr/local and its ICD is NOT put in the loader's search
# path: nothing picks it up unless asked, with VK_DRIVER_FILES=$ICD (below). Advertising it
# system-wide would put two devices of one GPU in front of every Vulkan client, venus's and ours,
# and is the enhanced tier's job (docs/design/metal-transport-delivery.md §1).
#
# Per image, in place: refuse if a live limina-vmm has it open; CoW backup
# `<image>.bak-pre-metal.raw` (LIMINA_DELIVER_BACKUP=0 skips it); boot through the default vehicle
# without the transport; install; rebuild the initramfs, so early boot binds the extended module
# and not the stock one it carries; verify the installed bytes and that the module resolves to the
# installed file; clean poweroff.
#
# Inputs (defaults are where the two builds leave them):
#   LIMINA_METAL_MODULE  scripts/build-virtio-gpu-limina.sh <KTAG> <KREL> ->
#                        target/virtio-gpu-limina/<KREL>/virtio-gpu.ko
#   LIMINA_METAL_KREL    the module's kernel release (default 6.19.10-300.fc44.aarch64)
#   LIMINA_METAL_KK      spikes/kk-on-wire/kk-build.sh -> spikes/kk-on-wire/out/libvulkan_kosmickrisp.so
# Env otherwise as deliver-payload.sh (LIMINA_GUEST_USER, LIMINA_DELIVER_LOGDIR,
# LIMINA_DELIVER_SSH_TIMEOUT, and what the boot vehicle honours).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

[ "$#" -ge 1 ] || { echo "usage: deliver-metal-transport.sh <image.raw>..." >&2; exit 2; }
for img in "$@"; do [ -f "$img" ] || { echo "no such image: $img" >&2; exit 2; }; done

KREL="${LIMINA_METAL_KREL:-6.19.10-300.fc44.aarch64}"
MODULE="${LIMINA_METAL_MODULE:-target/virtio-gpu-limina/$KREL/virtio-gpu.ko}"
KK="${LIMINA_METAL_KK:-spikes/kk-on-wire/out/libvulkan_kosmickrisp.so}"
[ -f "$MODULE" ] || { echo "no module at $MODULE (scripts/build-virtio-gpu-limina.sh)" >&2; exit 2; }
[ -f "$KK" ] || { echo "no driver at $KK (spikes/kk-on-wire/kk-build.sh)" >&2; exit 2; }

# Where they land in the guest. The L2 tests select the driver by $ICD.
LIBDIR=/usr/local/lib64/limina
ICD=/usr/local/share/limina/vulkan/kk_limina_icd.json
MODDIR="/lib/modules/$KREL/updates"

USER_="${LIMINA_GUEST_USER:-claude}"
LOGDIR="${LIMINA_DELIVER_LOGDIR:-/tmp}"
SSH_TIMEOUT="${LIMINA_DELIVER_SSH_TIMEOUT:-420}"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

WANT_MOD="$(shasum -a 256 "$MODULE" | cut -d' ' -f1)"
WANT_KK="$(shasum -a 256 "$KK" | cut -d' ' -f1)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
printf '{"ICD":{"api_version":"1.4.363","library_path":"%s/libvulkan_kosmickrisp.so"},"file_format_version":"1.0.1"}\n' \
  "$LIBDIR" > "$STAGE/kk_limina_icd.json"
echo "module $KREL ${WANT_MOD:0:12}…  driver ${WANT_KK:0:12}…"

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

  local running
  running="$("${g[@]}" 'uname -r')"
  if [ "$running" != "$KREL" ]; then
    echo "!!! $img runs $running; the module is built for $KREL" >&2
    off
    return 1
  fi

  scp -P "$port" "${SSH_OPTS[@]}" "$MODULE" "$KK" "$STAGE/kk_limina_icd.json" "$USER_@127.0.0.1:"
  if "${g[@]}" "set -e
      sudo install -D -m644 virtio-gpu.ko '$MODDIR/virtio-gpu.ko'
      sudo install -D -m755 libvulkan_kosmickrisp.so '$LIBDIR/libvulkan_kosmickrisp.so'
      sudo install -D -m644 kk_limina_icd.json '$ICD'
      sudo restorecon -R '$MODDIR' '$LIBDIR' '$(dirname "$ICD")' || true
      rm -f virtio-gpu.ko libvulkan_kosmickrisp.so kk_limina_icd.json
      sudo depmod -a '$KREL'
      sudo dracut -f --kver '$KREL'" >"$ilog" 2>&1; then
    echo "installed (log $ilog)"
  else
    echo "!!! install FAILED (log $ilog); tail:" >&2; tail -15 "$ilog" >&2
    off
    return 1
  fi

  local ok=1 got
  got="$("${g[@]}" "sha256sum '$MODDIR/virtio-gpu.ko' | cut -d' ' -f1")"
  [ "$got" = "$WANT_MOD" ] || { echo "!!! installed module is $got, want $WANT_MOD" >&2; ok=0; }
  got="$("${g[@]}" "sha256sum '$LIBDIR/libvulkan_kosmickrisp.so' | cut -d' ' -f1")"
  [ "$got" = "$WANT_KK" ] || { echo "!!! installed driver is $got, want $WANT_KK" >&2; ok=0; }
  got="$("${g[@]}" "modinfo -k '$KREL' -n virtio_gpu")"
  [ "$got" = "$MODDIR/virtio-gpu.ko" ] || { echo "!!! virtio_gpu resolves to $got" >&2; ok=0; }
  # Early boot binds virtio_gpu from the initramfs when it carries one, before the root is up, so
  # any copy it carries must be ours. Carrying none is fine: the root's then binds.
  local inird
  inird="$("${g[@]}" "sudo lsinitrd --kver '$KREL'" | grep -E '/virtio-gpu\.ko' || true)"
  if [ -z "$inird" ]; then
    echo "the initramfs carries no virtio-gpu module; the root's binds"
  elif grep -q 'updates/virtio-gpu\.ko' <<<"$inird" && ! grep -qv 'updates/virtio-gpu\.ko' <<<"$inird"; then
    echo "the initramfs carries the extended module"
  else
    echo "!!! the initramfs for $KREL carries a virtio-gpu module that is not ours: $inird" >&2; ok=0
  fi
  [ "$ok" = 1 ] && echo "verified: module and driver match, and the module resolves to ours"

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
echo "delivered the native-context transport to: $*"
