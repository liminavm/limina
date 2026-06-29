#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# limina enhanced-tier in-guest installer.
#
# Runs INSIDE a basic (stock) Fedora guest and upgrades it to the enhanced tier. Per the
# two-tier guarantee (CLAUDE.md) this is the bootstrap substrate: it must depend ONLY on what
# a stock F43 already has (dnf, grubby, systemctl, virtiofs) — NOT on the 16k kernel / venus /
# mutter it installs. It is delivered via a virtio-fs share (the "attached volume") and run
# here, so the host never mutates the guest image directly.
#
# DELIVERY = RPMs that REPLACE stock at /usr (see memory limina-enh-delivery). We do NOT use
# systemd-sysext for mesa: enhanced mesa is 26.2 but stock F43 is 25.3.6, so the libgallium
# soname differs; an overlay can only SHADOW (not remove) the stock lib, leaving a 25.3.6/26.2
# ABI blend that breaks mutter's KMS EGL. An RPM REPLACES stock (old soname removed) so nothing
# blends; `dnf versionlock` then pins it so an update cannot drag mesa back to a venus-breaking
# stock version.
#
# Installs, each additive (a guest may already have some — partial states are normal):
#   1. 16k kernel RPM   -> kernel-install/dracut writes a BLS entry (co-exists with stock); LOCKED
#   2. mesa RPMs (26.2)  -> replace stock mesa at /usr (zink GL + venus Vulkan);            LOCKED
#   3. mutter RPMs       -> replace stock mutter at /usr (patched compositor);          NOT locked
#                          (mutter tracks the distro/gnome-shell version; see limina-enh-delivery)
#   4. limina-agent      -> /usr/local/bin + unit (clipboard, dynamic resize, PSI autoballoon).
#                          OPTIONAL: installed only if staged into the payload, and SELinux-
#                          relabeled so it starts on a stock (Enforcing) guest.
#   5. driver-select env -> /etc/environment.d (route GL through zink->venus, force the venus ICD)
#   6. GRUB              -> default to the 16k kernel + auto-boot (unattended)
#
# Usage (in guest):  sudo /path/to/install-enhanced.sh [PAYLOAD_DIR]
#   PAYLOAD_DIR defaults to the script's own directory (the mounted share).
set -euo pipefail
PAYLOAD="${1:-$(cd "$(dirname "$0")" && pwd)}"
echo "== limina enhanced-tier installer (payload: $PAYLOAD) =="
[ "$(id -u)" = 0 ] || { echo "must run as root (sudo)"; exit 1; }

# dnf versionlock plugin is not in a minimal install; pull it from what stock already ships.
echo "-- ensuring dnf versionlock plugin"
dnf install -y 'dnf-command(versionlock)' >/dev/null 2>&1 \
  || dnf install -y python3-dnf-plugin-versionlock >/dev/null 2>&1 || true

# Collect runtime RPMs for a glob, excluding debug/devel/tests (the guest does not build).
runtime_rpms() {  # <glob>
  ls $1 2>/dev/null | grep -vE 'debuginfo|debugsource|-devel-|-tests-' || true
}

### 1. 16k kernel RPM -> BLS entry via kernel-install + dracut; versionlock #######
RPM=$(ls "$PAYLOAD"/limina-kernel-16k-*.rpm 2>/dev/null | head -1)
[ -n "$RPM" ] || { echo "no kernel RPM in payload"; exit 1; }
echo "-- installing kernel: $(basename "$RPM")"
dnf install -y "$RPM"
KREL=$(ls -d /lib/modules/*limina16k* 2>/dev/null | xargs -n1 basename | head -1)
[ -n "$KREL" ] || { echo "kernel modules dir not found after install"; exit 1; }
[ -f "/boot/initramfs-$KREL.img" ] || { echo "dracut did NOT build an initramfs for $KREL"; exit 1; }
ls -1 "/boot/loader/entries/"*"$KREL"*.conf >/dev/null || { echo "no BLS entry for $KREL"; exit 1; }
dnf versionlock add limina-kernel-16k >/dev/null 2>&1 || true
echo "   kernel $KREL: initramfs + BLS entry present (versionlocked)"

### 2. mesa RPMs (26.2 zink+venus) -> REPLACE stock at /usr; versionlock #########
MESA_RPMS=$(runtime_rpms "$PAYLOAD/mesa-*.rpm")
[ -n "$MESA_RPMS" ] || { echo "no mesa RPMs in payload"; exit 1; }
echo "-- installing mesa (replaces stock 25.3.6 -> our 26.2):"; echo "$MESA_RPMS" | sed 's#.*/#     #'
# `dnf install` of the higher-versioned local RPMs upgrades/replaces the matching stock packages.
dnf install -y --allowerasing $MESA_RPMS
# Lock the mesa stack so a later `dnf update` cannot revert it to a venus-breaking stock version.
for p in $(rpm -qa 'mesa-*' --qf '%{NAME}\n' | sort -u); do
  dnf versionlock add "$p" >/dev/null 2>&1 || true
done
echo "   mesa 26.2 installed + versionlocked"
rpm -q mesa-vulkan-drivers --qf '   venus ICD pkg: %{NVRA}\n' 2>/dev/null || true

### 3. mutter RPMs (patched, target-matched) -> REPLACE stock at /usr; NOT locked #
MUTTER_RPMS=$(runtime_rpms "$PAYLOAD/mutter-*.rpm")
[ -n "$MUTTER_RPMS" ] || { echo "no mutter RPMs in payload"; exit 1; }
echo "-- installing mutter (patched, replaces stock; NOT versionlocked — tracks gnome-shell):"
echo "$MUTTER_RPMS" | sed 's#.*/#     #'
dnf install -y --allowerasing $MUTTER_RPMS
echo "   patched mutter installed"

### 4. limina-agent -> /usr/local/bin + unit (OPTIONAL; present iff staged into the payload) ##
# Folds in what scripts/install-guest-agent.sh used to do over SSH, so the whole enhanced upgrade
# rides the one offline virtiofs channel. Without the agent, clipboard / dynamic display resize /
# PSI autoballoon / share auto-mount stay inactive — so a payload that ships it is preferred, but
# its absence is non-fatal (two-tier: each feature lights up on its own prerequisite).
AGENT_BIN="$PAYLOAD/limina-agent"
if [ -f "$AGENT_BIN" ]; then
  echo "-- installing limina-agent + unit"
  install -m 0755 "$AGENT_BIN" /usr/local/bin/limina-agent
  UNIT="$PAYLOAD/limina-agent.service"
  [ -f "$UNIT" ] && install -m 0644 "$UNIT" /etc/systemd/system/limina-agent.service
  # Flat (linear) pointer profile so captured mouselook doesn't double-accelerate; ships as a
  # gschema DEFAULT override (a user gsettings override still wins; stock guests are unaffected).
  GSCHEMA="$PAYLOAD/90-limina-pointer.gschema.override"
  if [ -f "$GSCHEMA" ]; then
    install -m 0644 "$GSCHEMA" /usr/share/glib-2.0/schemas/
    glib-compile-schemas /usr/share/glib-2.0/schemas/ >/dev/null 2>&1 || true
  fi
  # SELinux: a freshly-converted stock guest runs Enforcing, so relabel what we just dropped (the
  # dev guest dodges this with selinux=0). A no-op on a permissive/disabled guest.
  if command -v restorecon >/dev/null 2>&1; then
    restorecon -v /usr/local/bin/limina-agent /etc/systemd/system/limina-agent.service 2>/dev/null || true
  fi
  systemctl daemon-reload
  if systemctl enable --now limina-agent 2>/dev/null; then
    echo "   limina-agent enabled (active: $(systemctl is-active limina-agent 2>/dev/null || echo unknown))"
  else
    echo "   WARN limina-agent failed to start (check: journalctl -u limina-agent)"
  fi
else
  echo "-- no limina-agent in payload; skipping (clipboard / dynamic resize / PSI stay inactive)"
fi

### 5. driver selection: GL via zink -> venus (force the venus ICD over lavapipe) #
# mesa now lives at /usr, so this is plain driver-selection env (no LD_LIBRARY_PATH/prefix). Our
# mesa-vulkan-drivers ships BOTH lavapipe (lvp) and venus (virtio) ICDs, so pin venus for zink;
# otherwise zink may enumerate lavapipe first and run GL on the CPU. On a stock-4k fallback boot
# venus will not init and GL degrades — acceptable, that path is the safety net, not the feature.
echo "-- GL driver selection -> zink over venus"
VENUS_ICD=$(ls /usr/share/vulkan/icd.d/virtio_icd.*.json 2>/dev/null | head -1)
mkdir -p /etc/environment.d
{
  echo "# limina enhanced tier: route GL through zink -> venus (virtio Vulkan), not llvmpipe."
  echo "GALLIUM_DRIVER=zink"
  echo "MESA_LOADER_DRIVER_OVERRIDE=zink"
  [ -n "$VENUS_ICD" ] && echo "VK_DRIVER_FILES=$VENUS_ICD"
  echo "VN_PERF=no_fence_feedback"
} > /etc/environment.d/90-limina-zink.conf
sed 's/^/   /' /etc/environment.d/90-limina-zink.conf

### 6. GRUB: default to the 16k kernel + unattended auto-boot ####################
echo "-- GRUB default -> $KREL, auto-boot"
grubby --set-default="/boot/vmlinuz-$KREL"
grub2-editenv - unset boot_indeterminate menu_auto_hide 2>/dev/null || true
echo "   default kernel: $(grubby --default-kernel)"

echo "== enhanced-tier install complete. Reboot to boot the 16k kernel + venus desktop. =="
