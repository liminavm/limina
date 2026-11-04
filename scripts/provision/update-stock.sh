#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Bring stock-tier images up to the distro's current packages, in place.
#
#   scripts/provision/update-stock.sh Fedora-Workstation-44.accessible.raw \
#       Fedora-Workstation-44.stock.test.raw
#
# A stock image is only a useful baseline while it runs what a Fedora user gets today: a kernel
# a release behind decides what the stock-tier tests can cover (free page reporting is gated on
# the guest kernel, for one).
#
# Per image, mirroring deliver-payload.sh:
#   1. refuse if a live limina-vmm has the image open (other sessions run VMs on this host);
#   2. CoW backup `<image>.bak-pre-update-<YYYY-MM-DD>.raw` (skipped when it exists;
#      LIMINA_BACKUP=0 skips it);
#   3. boot through the default vehicle (EFI, the guest's own kernel) and wait for sshd with
#      scripts/wait-guest-ssh.sh — the one readiness oracle;
#   4. `dnf upgrade --refresh`, drop the package cache, fstrim (an image that keeps the blocks an
#      upgrade freed absorbs `l2_qga_fstrim`'s write), clean poweroff;
#   5. boot again and verify the guest runs the newest installed kernel with no failed units, then
#      clean poweroff.
# Prints the kernel, Mesa, mutter and GNOME Shell versions before and after, for docs/images.md.
#
# Env: LIMINA_GUEST_USER (default claude), LIMINA_SSH_TIMEOUT (default 420),
# LIMINA_LOGDIR (default /tmp), plus everything the boot vehicle honours.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

[ "$#" -ge 1 ] || { echo "usage: update-stock.sh <image.raw>..." >&2; exit 2; }
for img in "$@"; do [ -f "$img" ] || { echo "no such image: $img" >&2; exit 2; }; done

USER_="${LIMINA_GUEST_USER:-claude}"
LOGDIR="${LIMINA_LOGDIR:-/tmp}"
SSH_TIMEOUT="${LIMINA_SSH_TIMEOUT:-420}"
STAMP="$(date +%F)"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
VERSIONS='uname -r; rpm -q kernel-core mesa-dri-drivers mesa-va-drivers-freeworld mutter gnome-shell 2>&1 | sort; echo "virtio_gpu: $(modinfo -n virtio_gpu)"; df -h / | tail -1'

# Boot $1; its ssh port lands in PORT (empty on failure) and the vehicle's pid in BOOT. Not run
# in a command substitution: the vehicle must stay this shell's child for `wait`.
boot() {
  local img="$1" log="$2" blog="$3"
  # wait-guest-ssh reads the last forward line in the log; a stale one names another VM's port.
  rm -f "$log"
  LIMINA_DISK="$img" LIMINA_BOOT_LOG="$log" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh \
    >"$blog" 2>&1 &
  BOOT=$!
  PORT="$(scripts/wait-guest-ssh.sh "$log" "$SSH_TIMEOUT" "$BOOT")" || PORT=
  if [ -z "$PORT" ]; then
    kill "$BOOT" 2>/dev/null || true
    wait "$BOOT" 2>/dev/null || true
  fi
}

poweroff() {
  # `-i`: the autologin desktop holds a block inhibitor that refuses a plain poweroff.
  ssh -p "$1" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" 'sudo systemctl poweroff -i' || true
  wait "$BOOT" || true
}

update_one() {
  local img="$1" base name log ulog port g newest running failed
  base="${img%.raw}"; name="$(basename "$base")"
  log="/tmp/limina-worker-$name.log"
  ulog="$LOGDIR/limina-update-$name-$STAMP.log"
  echo "=== $img  $(date '+%F %T')"

  if pgrep -f "[l]imina-vmm.*$(basename "$img")" >/dev/null; then
    echo "!!! a live limina-vmm has $img open — refusing" >&2
    return 1
  fi
  if [ "${LIMINA_BACKUP:-1}" != "0" ]; then
    if [ -e "$base.bak-pre-update-$STAMP.raw" ]; then
      echo "backup $base.bak-pre-update-$STAMP.raw already exists; keeping it"
    else
      cp -c "$img" "$base.bak-pre-update-$STAMP.raw"
      echo "backup: $base.bak-pre-update-$STAMP.raw"
    fi
  fi

  boot "$img" "$log" "$LOGDIR/limina-update-$name-$STAMP.boot1.log"; port="$PORT"
  [ -n "$port" ] || { echo "!!! no SSH port for $img (see $log)" >&2; return 1; }
  g=(ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1")
  echo "guest up: ssh -p $port $USER_@127.0.0.1 (worker log $log)"
  echo "--- before"; "${g[@]}" "$VERSIONS" | sed 's/^/   /'
  # GNOME Software downloads offline updates on every boot of a Workstation image: gigabytes in
  # /var/lib/dnf/offline that fill the 11 GiB root and leave btrfs no room for metadata, which
  # fails the transaction midway. Drop them first; rebalance after, so the next upgrade has
  # unallocated space again.
  if "${g[@]}" 'sudo dnf offline clean && sudo dnf -y upgrade --refresh && sudo dnf clean all &&
                sudo btrfs balance start -dusage=50 /' >"$ulog" 2>&1; then
    echo "upgrade ok (log $ulog)"
  else
    echo "!!! upgrade FAILED (log $ulog); tail:" >&2; tail -15 "$ulog" >&2
    poweroff "$port"
    return 1
  fi
  "${g[@]}" 'sudo fstrim -av' 2>&1 | sed 's/^/   trim: /' || true
  poweroff "$port"

  boot "$img" "$log" "$LOGDIR/limina-update-$name-$STAMP.boot2.log"; port="$PORT"
  [ -n "$port" ] || { echo "!!! $img did not come back after the upgrade (see $log)" >&2; return 1; }
  g=(ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1")
  echo "--- after"; "${g[@]}" "$VERSIONS" | sed 's/^/   /'
  newest="$("${g[@]}" "rpm -q --last kernel-core | head -1 | sed -E 's/^kernel-core-([^ ]+).*/\1/'")"
  running="$("${g[@]}" 'uname -r')"
  failed="$("${g[@]}" 'systemctl --failed --plain --no-legend' || true)"
  poweroff "$port"
  local ok=1
  [ "$running" = "$newest" ] || { echo "!!! booted $running, newest installed is $newest" >&2; ok=0; }
  [ -z "$failed" ] || { echo "!!! failed units:" >&2; printf '%s\n' "$failed" | sed 's/^/   /' >&2; ok=0; }
  [ "$ok" = 1 ] && echo "verified: boots $running with no failed units"
  echo "=== done $img  $(date '+%F %T')"
  [ "$ok" = 1 ]
}

failed=()
for img in "$@"; do
  update_one "$img" || failed+=("$img")
done
if [ "${#failed[@]}" -gt 0 ]; then
  echo "FAILED: ${failed[*]}" >&2
  exit 1
fi
echo "updated: $*"
