#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Install RPM Fusion's mesa-va-drivers-freeworld on stock-tier images, in place.
#
#   scripts/provision/install-freeworld-va.sh Fedora-Workstation-44.accessible.raw \
#       Fedora-Workstation-44.stock.test.raw
#
# Why a stock image gets a third-party package: Fedora builds mesa
# `-Dvideo-codecs=all_free`, and the VA frontend refuses H.264/HEVC in vl_codec.c BEFORE the
# driver is consulted, so no host advertisement can reach a guest built that way. freeworld is
# the route a real Fedora user takes for the same reason, and libva already probes
# `/usr/lib64/dri-freeworld/` ahead of `/usr/lib64/dri/` — so this is what "stock" means for us.
# See docs/design/h264-hevc-decode.md.
#
# Per image, in place, mirroring deliver-payload.sh:
#   1. refuse if a live limina-vmm has the image open (other sessions run VMs on this host);
#   2. CoW backup `<image>.bak-pre-freeworld.raw` (skipped when it exists; LIMINA_BACKUP=0 skips);
#   3. boot through the default vehicle (EFI, the guest's own kernel) and wait for sshd with
#      scripts/wait-guest-ssh.sh — the one readiness oracle;
#   4. enable the RPM Fusion FREE repo and install mesa-va-drivers-freeworld;
#   5. verify the driver is installed AND that libva actually SELECTS it — an installed package
#      that libva does not load would leave the image no different from before;
#   6. clean poweroff, next image.
#
# NOT idempotent-hostile: re-running is safe, dnf makes both steps no-ops.
#
# Env: LIMINA_GUEST_USER (default claude), LIMINA_SSH_TIMEOUT (default 420),
# LIMINA_LOGDIR (default /tmp), plus everything the boot vehicle honours.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

[ "$#" -ge 1 ] || { echo "usage: install-freeworld-va.sh <image.raw>..." >&2; exit 2; }
for img in "$@"; do [ -f "$img" ] || { echo "no such image: $img" >&2; exit 2; }; done

USER_="${LIMINA_GUEST_USER:-claude}"
LOGDIR="${LIMINA_LOGDIR:-/tmp}"
SSH_TIMEOUT="${LIMINA_SSH_TIMEOUT:-420}"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

install_one() {
  local img="$1" base name log boot port ilog
  base="${img%.raw}"; name="$(basename "$base")"
  log="/tmp/limina-worker-$name.log"
  ilog="$LOGDIR/limina-freeworld-$name.log"
  echo "=== $img  $(date '+%F %T')"

  if pgrep -f "limina-vmm.*$(basename "$img")" >/dev/null; then
    echo "!!! a live limina-vmm has $img open — refusing" >&2
    return 1
  fi

  if [ "${LIMINA_BACKUP:-1}" != "0" ]; then
    if [ -e "$base.bak-pre-freeworld.raw" ]; then
      echo "backup $base.bak-pre-freeworld.raw already exists; keeping it"
    else
      cp -c "$img" "$base.bak-pre-freeworld.raw"
      echo "backup: $base.bak-pre-freeworld.raw"
    fi
  fi

  LIMINA_DISK="$img" LIMINA_BOOT_LOG="$log" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh \
    >"$LOGDIR/limina-freeworld-$name.boot.log" 2>&1 &
  boot=$!
  port="$(scripts/wait-guest-ssh.sh "$log" "$SSH_TIMEOUT" "$boot")" || true
  if [ -z "$port" ]; then
    # An empty port does not just fail the install: every ssh below dies with `Bad port ''`,
    # the poweroff included, so `wait "$boot"` never returns and the run hangs with the guest up.
    echo "!!! no SSH port for $img (see $log) — abandoning this image" >&2
    kill "$boot" 2>/dev/null || true
    wait "$boot" 2>/dev/null || true
    return 1
  fi
  echo "guest up: ssh -p $port $USER_@127.0.0.1 (worker log $log)"

  if ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" bash -s >"$ilog" 2>&1 <<'GUEST'
set -euxo pipefail
. /etc/os-release
sudo dnf install -y \
  "https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-${VERSION_ID}.noarch.rpm"
sudo dnf install -y mesa-va-drivers-freeworld
rpm -q mesa-va-drivers-freeworld
ls -l /usr/lib64/dri-freeworld/
GUEST
  then
    echo "install ok (log $ilog)"
  else
    echo "!!! install FAILED (log $ilog); tail:" >&2; tail -20 "$ilog" >&2
    ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" 'sudo systemctl poweroff' || true
    wait "$boot" || true
    return 1
  fi

  # The verification that matters. An installed freeworld package proves nothing on its own:
  # libva picks a driver by searching its path list, and only the one it actually OPENS decides
  # which codecs the guest can see. vainfo names the file it opened.
  local drv ok=1
  drv="$(ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" \
        'vainfo 2>&1 | sed -nE "s#.*Trying to open (/usr/lib64/dri[^ ]*virtio_gpu_drv_video\.so).*#\1#p" | tail -1' || true)"
  case "$drv" in
    */dri-freeworld/*) echo "verified: libva opens $drv" ;;
    "")  echo "!!! could not tell which VA driver libva opened (see $ilog)" >&2; ok=0 ;;
    *)   echo "!!! libva still opens $drv, not the freeworld one" >&2; ok=0 ;;
  esac
  # Recorded, not asserted: until the host backend serves H.264/HEVC these will NOT appear, and
  # that is expected — the guest-side gate is what this script removes.
  ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" \
    'vainfo 2>/dev/null | grep -E "VAProfile.*VLD" | sed "s/^/   /"' || true

  ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" 'sudo systemctl poweroff' || true
  wait "$boot" || true
  echo "=== done $img  $(date '+%F %T')"
  [ "$ok" = 1 ]
}

failed=()
for img in "$@"; do
  install_one "$img" || failed+=("$img")
done
if [ "${#failed[@]}" -gt 0 ]; then
  echo "FAILED: ${failed[*]}" >&2
  exit 1
fi
echo "freeworld VA drivers installed on: $*"
