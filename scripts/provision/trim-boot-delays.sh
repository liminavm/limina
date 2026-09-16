#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Set the GRUB menu timeout to 0 on a guest image, in place:
#
#   scripts/provision/trim-boot-delays.sh Fedora-Workstation-44.stock.test.raw \
#       Fedora-Workstation-44.enhanced.test.raw ...
#
# The frozen goldens boot with `boot_success=0` in grubenv, so Workstation's menu auto-hide
# never engages and every EFI boot sits 5 s in the menu. Nothing in the suite needs to reach
# the menu; a pristine vanilla clone is the vehicle for that.
#
# zram swap is deliberately LEFT ALONE. A kernel without the zram module (every `--kernel`-
# injected test kernel) waits the full 45 s dev-zram0 device-job timeout in sysinit, but the
# fix for that lives on the inject command line (`systemd.zram=0`, see
# `GuestConfig::enhanced_fedora_from_env`): a stock guest has zram swap, and the balloon
# guards (`balloon_burst`) are written against that reality — removing it from the image made
# the 3 GiB burst OOM at chunk 6 (2026-09-16).
#
# Per image: refuse if a live limina-vmm has it open; CoW backup `<image>.bak-pre-fastboot.raw`
# (LIMINA_TUNE_BACKUP=0 skips); boot through the default vehicle (EFI + venus,
# `boot-enhanced-efi-kk.sh`), wait for sshd with `scripts/wait-guest-ssh.sh`; apply; verify
# (`GRUB_TIMEOUT=0` and no `set timeout=5` left in the regenerated grub.cfg); fstrim; clean
# poweroff.
#
# Env: LIMINA_GUEST_USER (default claude), LIMINA_TUNE_SSH_TIMEOUT (default 420),
# LIMINA_TUNE_LOGDIR (default /tmp), plus everything the boot vehicle honours.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

[ "$#" -ge 1 ] || { echo "usage: trim-boot-delays.sh <image.raw>..." >&2; exit 2; }
for img in "$@"; do [ -f "$img" ] || { echo "no such image: $img" >&2; exit 2; }; done

USER_="${LIMINA_GUEST_USER:-claude}"
LOGDIR="${LIMINA_TUNE_LOGDIR:-/tmp}"
SSH_TIMEOUT="${LIMINA_TUNE_SSH_TIMEOUT:-420}"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

APPLY='set -e
if grep -q "^GRUB_TIMEOUT=" /etc/default/grub; then
  sudo sed -i "s/^GRUB_TIMEOUT=.*/GRUB_TIMEOUT=0/" /etc/default/grub
else
  echo "GRUB_TIMEOUT=0" | sudo tee -a /etc/default/grub >/dev/null
fi
sudo grub2-mkconfig -o /boot/grub2/grub.cfg
echo "--- verify"
grep -q "^GRUB_TIMEOUT=0$" /etc/default/grub || { echo "VERIFY FAIL: /etc/default/grub does not say GRUB_TIMEOUT=0"; exit 1; }
grep -n "^GRUB_TIMEOUT=" /etc/default/grub
# The stock grub.cfg ALREADY carries a `set timeout=0` inside its menu-auto-hide branch, so
# presence proves nothing; what must be gone is the 5 s the header block used to set.
if sudo grep -qE "^\s*set timeout=5\s*$" /boot/grub2/grub.cfg; then echo "VERIFY FAIL: grub.cfg still sets timeout=5"; exit 1; fi
sudo grep -nE "^\s*set timeout=" /boot/grub2/grub.cfg
echo "VERIFY OK"'

tune() {
  local img="$1" base name log boot port alog
  base="${img%.raw}"; name="$(basename "$base")"
  log="/tmp/limina-worker-$name.log"
  alog="$LOGDIR/limina-trim-$name.apply.log"
  echo "=== $img  $(date '+%F %T')"

  if pgrep -f "limina-vmm.*$(basename "$img")" >/dev/null; then
    echo "!!! a live limina-vmm has $img open — refusing" >&2
    return 1
  fi

  if [ "${LIMINA_TUNE_BACKUP:-1}" != "0" ]; then
    if [ -e "$base.bak-pre-fastboot.raw" ]; then
      echo "backup $base.bak-pre-fastboot.raw already exists; keeping it"
    else
      cp -c "$img" "$base.bak-pre-fastboot.raw"
      echo "backup: $base.bak-pre-fastboot.raw"
    fi
  fi

  # Empty worker log first: wait-guest-ssh reads the LAST forward line, and a stale one names a
  # port some other VM may be answering on (the wrong-VM trap deliver-payload.sh documents).
  rm -f "$log"
  LIMINA_DISK="$img" LIMINA_BOOT_LOG="$log" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh \
    >"$LOGDIR/limina-trim-$name.boot.log" 2>&1 &
  boot=$!
  port="$(scripts/wait-guest-ssh.sh "$log" "$SSH_TIMEOUT" "$boot")" || true
  if [ -z "$port" ]; then
    echo "!!! no SSH port for $img (see $log) — abandoning this image" >&2
    kill "$boot" 2>/dev/null || true
    wait "$boot" 2>/dev/null || true
    return 1
  fi
  echo "guest up: ssh -p $port $USER_@127.0.0.1 (worker log $log)"

  local ok=1
  # Capped: dnf in a seated guest can wait on PackageKit's rpm lock, and an uncapped hang here
  # would leave the guest up and this loop stuck on it.
  if ssh -p "$port" "${SSH_OPTS[@]}" -o ServerAliveInterval=15 "$USER_@127.0.0.1" "timeout 600 bash -c $(printf '%q' "$APPLY")" >"$alog" 2>&1; then
    grep -E "^(GRUB_TIMEOUT|VERIFY|[0-9]+:)" "$alog" | sed 's/^/   /'
  else
    ok=0
    echo "!!! apply FAILED (log $alog); tail:" >&2; tail -15 "$alog" >&2
  fi

  ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" 'sudo fstrim -av' 2>&1 | sed 's/^/   trim: /' || true
  # The autologin desktop holds a logind block inhibitor; -i ignores it (nothing to preserve).
  ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" 'sudo systemctl poweroff -i' || true
  wait "$boot" || true
  echo "=== done $img  $(date '+%F %T')"
  [ "$ok" = 1 ]
}

failed=()
for img in "$@"; do
  tune "$img" || failed+=("$img")
done
if [ "${#failed[@]}" -gt 0 ]; then
  echo "FAILED: ${failed[*]}" >&2
  exit 1
fi
echo "trimmed boot delays on: $*"
