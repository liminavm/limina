#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

#
# prepare-efi-image.sh — make a limina guest image boot cleanly + observably via EFI.
#
# Two one-time prep steps the EFI/stock-kernel boot path needs:
#   (A) a SELinux relabel (see below) so the enforcing stock kernel doesn't reboot-loop, and
#   (B) `console=tty0 console=ttyAMA0` on the stock-kernel GRUB args, so the EFI boot surfaces
#       the kernel log + a getty login on the PL011 serial (ttyAMA0) and keeps the framebuffer
#       console (tty0) live for the GOP window. Without it the stock kernel goes silent on
#       serial after GRUB (the cmdline is GRUB-owned on this path).
#
# (A) Why the relabel
# -------------------
# Our enhanced-tier development boots a *custom* kernel built from `arm64 defconfig`, which
# has **no CONFIG_SECURITY_SELINUX** (see scripts/build-test-kernel.sh). With no SELinux in
# the kernel, the guest never assigns `security.selinux` labels to anything we install — so
# every file written through months of `selinux=0` custom-kernel sessions (dnf packages,
# our mesa/mutter/agent overlay, python bytecode) ends up *unlabeled*.
#
# The stock Fedora kernel on the EFI/GRUB path, by contrast, comes up **enforcing**
# (`SELINUX=enforcing` in /etc/selinux/config). It sees the unlabeled tree + a stale
# `/.autorelabel` flag, tries to relabel the whole filesystem under enforcing, gets denied
# mid-relabel, and reboots — forever. That reboot loop is what wedges "boot the image via
# EFI to the desktop".
#
# The fix is a single **permissive** relabel: set SELINUX=permissive (so denials can't
# interrupt the relabel), drop /.autorelabel, boot the stock kernel once to let
# `fixfiles restore` label the whole tree, then it converges and EFI boots straight to
# userspace on every subsequent boot. We keep the image permissive (dev-friendly; the
# productized image will match the distro's enforcing config when we get there).
#
# This script orchestrates both end to end via the shipped `limina` binary:
#   1. prep    — custom-kernel boot (selinux=0): set permissive + /.autorelabel + the console
#                GRUB args, then poweroff
#   2. relabel — one EFI boot: fixfiles relabels, the guest reboots, and the supervisor relaunches
#                the worker (a guest reboot no longer tears the VM down) until it converges to sshd
#
# Usage: scripts/prepare-efi-image.sh [IMAGE]   (default: Fedora-Workstation-43.dev-enh.raw)
set -euo pipefail

cd "$(dirname "$0")/.."
IMAGE="${1:-Fedora-Workstation-43.dev-enh.raw}"
KERNEL="${LIMINA_TEST_KERNEL_16K:-target/test-guest/kernel/Image-16k}"
FIRMWARE="${LIMINA_FIRMWARE:-/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd}"
LIMINA="${LIMINA_BIN:-target/debug/limina}"
VMM="${LIMINA_VMM_BIN:-target/debug/limina-vmm}"
WORK="$(mktemp -d)/relabel"
mkdir -p "$WORK"

[[ -f "$IMAGE" ]]    || { echo "image not found: $IMAGE" >&2; exit 1; }
[[ -f "$KERNEL" ]]   || { echo "16k kernel not found: $KERNEL (scripts/build-test-kernel.sh PAGESIZE=16k)" >&2; exit 1; }
[[ -f "$FIRMWARE" ]] || { echo "EFI firmware not found: $FIRMWARE" >&2; exit 1; }

SSH=(ssh -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
     -o BatchMode=yes -o ConnectTimeout=4 claude@127.0.0.1)

kill_vm() { pkill -9 limina-vmm 2>/dev/null || true; lsof -ti tcp:2222 2>/dev/null | xargs -r kill -9 2>/dev/null || true; }
trap kill_vm EXIT

wait_ssh() {  # $1 = timeout seconds
  local deadline=$(( SECONDS + $1 ))
  while (( SECONDS < deadline )); do
    "${SSH[@]}" true 2>/dev/null && return 0
    sleep 3
  done
  return 1
}

wait_down() {  # wait for limina-vmm to exit (guest rebooted → VMM tears down)
  local deadline=$(( SECONDS + ${1:-90} ))
  while (( SECONDS < deadline )); do pgrep -x limina-vmm >/dev/null || return 0; sleep 3; done
  return 1
}

echo "==> [1/2] prep: set SELINUX=permissive + /.autorelabel + serial console on $IMAGE"
kill_vm
"$LIMINA" --vmm-bin "$VMM" --kernel "$KERNEL" \
  --cmdline "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 console=ttyAMA0" \
  --disk "$IMAGE" --cpus 4 --ram-mib 4096 --net --gpu-software-2d >"$WORK/prep.log" 2>&1 &
wait_ssh 90 || { echo "prep boot: no SSH" >&2; exit 1; }
"${SSH[@]}" 'sudo sed -i "s/^SELINUX=.*/SELINUX=permissive/" /etc/selinux/config \
   && sudo touch /.autorelabel \
   && sudo grubby --update-kernel=ALL --args="console=tty0 console=ttyAMA0" \
   && sudo sync'
"${SSH[@]}" 'sudo systemctl poweroff' 2>/dev/null || true
wait_down 30 || kill_vm

echo "==> [2/2] relabel + converge: one EFI boot — fixfiles relabels, the guest reboots, and"
echo "         the supervisor relaunches the worker until it converges to sshd (/.autorelabel gone)"
kill_vm
"$LIMINA" --vmm-bin "$VMM" --firmware "$FIRMWARE" \
  --disk "$IMAGE" --cpus 4 --ram-mib 4096 --net --gpu-software-2d \
  --console "$WORK/relabel.log" >"$WORK/relabel-worker.log" 2>&1 &
# The relabel reboot no longer tears the VM down — limina relaunches the worker on a guest reboot
# (PSCI SYSTEM_RESET), so a single boot relabels, reboots, and comes back converged.
wait_ssh 300 || { echo "did not converge to sshd within 300s (relabel loop?)" >&2; exit 1; }
grep -q "Relabeling / " "$WORK/relabel.log" || { echo "relabel did not run (see $WORK/relabel.log)" >&2; exit 1; }
if [[ -n "$("${SSH[@]}" 'test -e /.autorelabel && echo yes' 2>/dev/null)" ]]; then
  echo "/.autorelabel still present after relabel — NOT converged" >&2; exit 1
fi
echo "    converged: $("${SSH[@]}" 'echo kernel=$(uname -r) selinux=$(getenforce)')"
"${SSH[@]}" 'sudo systemctl poweroff' 2>/dev/null || true
wait_down 30 || kill_vm
echo "==> done: $IMAGE now EFI-boots clean to userspace (permissive, labeled)"
