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

# ---- btrfs v1 space cache -> v2 free-space tree -------------------------------------------------
# A 16k-page kernel CANNOT mount a btrfs that still uses the v1 free-space cache: open_ctree fails
# with -22 and the boot drops to the (keyboard-less) emergency shell — the exact 2026-06-29
# dogfooding brick on a Parallels-migrated guest (a 2021-origin, pre-v2-default btrfs). Per the
# two-tier rule, switch every btrfs to the v2 free-space tree. Once the FREE_SPACE_TREE compat_ro
# flag is on the fs it is PERMANENT and BOTH the stock 4k and the 16k kernel mount it with no
# mount-option needed — but it MUST exist before the 16k's first boot. ensure_btrfs_free_space_tree
# sets FST_DEFER=1 if the tree could only be built by a later (stock) boot.
FST_DEFER=0

# rc 0 if $1 (a btrfs device) already carries the FREE_SPACE_TREE compat_ro bit (0x1).
btrfs_has_fst() {
  local f
  f=$(btrfs inspect-internal dump-super -f "$1" 2>/dev/null | awk '/^compat_ro_flags/{print $2}')
  [ -n "$f" ] && [ "$(( ${f:-0} & 0x1 ))" -ne 0 ]
}

# Set space_cache=v2 on every btrfs line in /etc/fstab (dropping any stale v1/bare space_cache).
# Safe: backs up first and only swaps in the rewrite if it kept the line count and the root entry.
fstab_set_space_cache_v2() {
  [ -f /etc/fstab ] || return 0
  cp -a /etc/fstab /etc/fstab.limina.bak
  awk 'BEGIN{OFS="\t"}
    /^[[:space:]]*#/ || NF<4 || $3!="btrfs" { print; next }
    {
      n=split($4,a,","); o=""
      for(i=1;i<=n;i++){ if(a[i] ~ /^space_cache/) continue; o=(o==""?a[i]:o","a[i]) }
      $4=(o==""?"space_cache=v2":o",space_cache=v2"); print
    }' /etc/fstab > /etc/fstab.limina.new
  if [ "$(wc -l </etc/fstab.limina.new)" = "$(wc -l </etc/fstab)" ] \
     && grep -qE '[[:space:]]/[[:space:]]' /etc/fstab.limina.new; then
    cat /etc/fstab.limina.new > /etc/fstab
    echo "   /etc/fstab: space_cache=v2 set on all btrfs mounts (backup: /etc/fstab.limina.bak)"
    grep -nE '[[:space:]]btrfs[[:space:]]' /etc/fstab | sed 's/^/     /'
  else
    echo "   WARN: fstab rewrite looked unsafe — left /etc/fstab unchanged (backup kept)" >&2
  fi
  rm -f /etc/fstab.limina.new
}

ensure_btrfs_free_space_tree() {
  command -v btrfs >/dev/null 2>&1 && command -v findmnt >/dev/null 2>&1 || return 0
  local out src tgt dev mp tok seen="" v1=""
  out=$(findmnt -t btrfs -rno SOURCE,TARGET || true)
  if [ -z "$out" ]; then echo "-- btrfs: no btrfs mounts found"; return 0; fi
  while read -r src tgt; do
    [ -n "$src" ] || continue
    dev=${src%%\[*}                                  # strip the [subvol] suffix
    case " $seen " in *" $dev "*) continue ;; esac   # one entry per filesystem
    seen="$seen $dev"
    if ! btrfs_has_fst "$dev"; then v1="$v1 $dev|$tgt"; fi
  done <<EOF
$out
EOF
  if [ -z "$v1" ]; then echo "-- btrfs: already on the v2 free-space tree"; return 0; fi

  echo "-- btrfs: legacy v1 space cache detected — a 16k kernel cannot mount it (open_ctree -22)"
  echo "   converting to the v2 free-space tree: fstab (all btrfs mounts) + build now"
  fstab_set_space_cache_v2
  for tok in $v1; do
    dev=${tok%%|*}; mp=${tok##*|}
    echo "   $dev (at $mp): building free-space tree"
    mount -o remount,clear_cache,space_cache=v2 "$mp" 2>/dev/null \
      || mount -o remount,space_cache=v2 "$mp" 2>/dev/null || true
    if btrfs_has_fst "$dev"; then
      echo "     built (compat_ro now carries FREE_SPACE_TREE)"
    else
      echo "     live build did not take — a plain stock boot will build it from fstab"
      FST_DEFER=1
    fi
  done
}

# Install a one-shot that arms the 16k trial ONLY after a stock boot has built the btrfs free-space
# tree (used when ensure_btrfs_free_space_tree could not build it live). Keeps the 16k from booting
# onto a still-v1 fs (which would strand it at the emergency shell).
install_arm_16k_after_fst_service() {
  cat > /usr/local/sbin/limina-arm-16k.sh <<'ARMSH'
#!/bin/sh
# Arm the limina 16k one-shot, but only once EVERY mounted btrfs has the v2 free-space tree
# (compat_ro bit 0x1). Until then a 16k boot would fail to mount root -> emergency shell.
case "$(uname -r)" in *limina16k*) exit 0 ;; esac   # already on 16k: nothing to arm
for d in $(findmnt -t btrfs -rno SOURCE | sed 's/\[.*//' | sort -u); do
  f=$(btrfs inspect-internal dump-super -f "$d" 2>/dev/null | awk '/^compat_ro_flags/{print $2}')
  [ -n "$f" ] && [ "$(( ${f:-0} & 0x1 ))" -ne 0 ] || exit 0
done
k=$(ls /boot/vmlinuz-*limina16k* 2>/dev/null | head -1)
[ -n "$k" ] || exit 0
idx=$(grubby --info="$k" 2>/dev/null | sed -n 's/^index=//p' | head -1)
if [ -n "$idx" ]; then
  grub2-reboot "$idx"
  logger -t limina "btrfs free-space tree ready; armed 16k ($k) — reboot to enter the enhanced kernel"
fi
systemctl disable limina-arm-16k.service
ARMSH
  chmod +x /usr/local/sbin/limina-arm-16k.sh
  cat > /etc/systemd/system/limina-arm-16k.service <<'ARMUNIT'
[Unit]
Description=Arm the limina 16k kernel once the btrfs free-space tree is built
After=multi-user.target
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/limina-arm-16k.sh
[Install]
WantedBy=multi-user.target
ARMUNIT
  systemctl daemon-reload
  systemctl enable limina-arm-16k.service >/dev/null 2>&1 || true
}

### 1. 16k kernel RPM -> BLS entry via kernel-install + dracut; versionlock #######
RPM=$(ls "$PAYLOAD"/limina-kernel-16k-*.rpm 2>/dev/null | head -1)
[ -n "$RPM" ] || { echo "no kernel RPM in payload"; exit 1; }

# Capture the CURRENT (stock) default kernel BEFORE we install the 16k — Fedora's kernel install
# auto-promotes the newest kernel to default, and step 6 must restore stock as the permanent
# fallback (two-tier safety; see step 6).
STOCK_DEFAULT=$(grubby --default-kernel 2>/dev/null || true)

# Pre-flight: the 16k vmlinuz + initramfs land on /boot. A FULL /boot makes dracut emit a
# truncated/driverless initramfs that boots far enough to run systemd-in-initramfs but then
# cannot mount root -> the dracut emergency shell (and limina has no keyboard there to recover —
# this is exactly the dogfooding brick of 2026-06-29). Fail EARLY with a cleanup hint instead.
BOOT_FREE_MB=$(($(df -Pk /boot | awk 'NR==2{print $4}') / 1024))
echo "-- /boot free: ${BOOT_FREE_MB} MiB"
if [ "${BOOT_FREE_MB:-0}" -lt 350 ]; then
  echo "ERROR: /boot has < 350 MiB free — the 16k kernel may not fit and would produce an" >&2
  echo "       unbootable initramfs. Free space first, e.g. prune old kernels:" >&2
  echo "         sudo dnf remove \$(dnf repoquery --installonly --latest-limit=-1 -q)" >&2
  exit 1
fi

# Make the 16k initramfs ROBUST and the emergency shell USABLE: force-include the virtio
# transport/block/console/input drivers and the common root filesystems regardless of dracut's
# host-only autodetection. virtio_input is what gives the (emergency) initramfs shell a keyboard
# — without it a failed boot is unrecoverable from inside the VM.
install -d /etc/dracut.conf.d
cat > /etc/dracut.conf.d/90-limina.conf <<'DRACUT'
# limina enhanced tier: guarantee the drivers needed to mount root under libkrun + a keyboard at
# the initramfs (emergency) shell. Applies to every initramfs built from here on.
add_drivers+=" virtio_blk virtio_pci virtio_console virtio_net virtio_input virtio_gpu "
filesystems+=" btrfs ext4 xfs vfat "
DRACUT

echo "-- installing kernel: $(basename "$RPM")"
dnf install -y "$RPM"
KREL=$(ls -d /lib/modules/*limina16k* 2>/dev/null | xargs -n1 basename | head -1)
[ -n "$KREL" ] || { echo "kernel modules dir not found after install"; exit 1; }
IMG="/boot/initramfs-$KREL.img"
[ -f "$IMG" ] || { echo "dracut did NOT build an initramfs for $KREL"; exit 1; }
ls -1 "/boot/loader/entries/"*"$KREL"*.conf >/dev/null || { echo "no BLS entry for $KREL"; exit 1; }

# Verify the initramfs can actually mount THIS guest's root — do NOT trust mere existence (that
# was the brick: an initramfs present but missing the root driver, so it dropped to emergency).
ROOT_FS=$(findmnt -no FSTYPE / 2>/dev/null || echo btrfs)
miss=""
lsinitrd "$IMG" 2>/dev/null | grep -q 'virtio_blk'  || miss="$miss virtio_blk"
lsinitrd "$IMG" 2>/dev/null | grep -q "$ROOT_FS"     || miss="$miss $ROOT_FS"
if [ -n "$miss" ]; then
  echo "ERROR: the 16k initramfs is missing root-mount driver(s):$miss" >&2
  echo "       It would drop to the (keyboard-less) emergency shell. Rebuild with:" >&2
  echo "         sudo dracut -f --add-drivers \"virtio_blk $ROOT_FS\" \"$IMG\" \"$KREL\"" >&2
  exit 1
fi
lsinitrd "$IMG" 2>/dev/null | grep -q 'virtio_input' \
  || echo "   WARN: no virtio_input in the initramfs — the emergency shell will have no keyboard"
dnf versionlock add limina-kernel-16k >/dev/null 2>&1 || true
echo "   kernel $KREL: initramfs verified (virtio_blk + $ROOT_FS) + BLS entry present (versionlocked)"

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

### 6. GRUB: try the 16k kernel for ONE boot; auto-fall-back to stock on failure ##
# CRITICAL two-tier safety. Do NOT make the unproven 16k kernel the permanent default: limina
# has no keyboard at GRUB or the emergency shell (2026-06-29), so a 16k that fails to boot would
# strand the guest with no way to pick stock. Instead keep stock as the PERMANENT default, boot
# 16k exactly ONCE (grub2-reboot sets next_entry, which GRUB consumes as it boots it), and let an
# on-success service promote 16k to default only after it actually reaches multi-user. A failed
# 16k boot just needs a power-cycle — the guest auto-returns to stock, no keyboard required.

# Convert any v1-space-cache btrfs to the v2 free-space tree FIRST: a 16k kernel cannot mount v1
# (this was the migrated-guest brick). May set FST_DEFER=1 if the tree can only be built by a
# subsequent stock boot — the one-shot arming below honors that.
ensure_btrfs_free_space_tree

echo "-- restoring stock as the permanent default; 16k gets a one-shot trial boot"
if [ -n "$STOCK_DEFAULT" ]; then
  grubby --set-default="$STOCK_DEFAULT" >/dev/null 2>&1 || true   # undo dnf's auto-promote of 16k
  echo "   permanent default kept at stock: $(grubby --default-kernel)"
else
  # Couldn't read the pre-install default — NEVER leave the auto-promoted, unproven 16k as the
  # permanent default. Find any non-16k kernel and pin it; if none exists, abort the GRUB step.
  STOCK_FALLBACK=$(ls -1 /boot/vmlinuz-* 2>/dev/null | grep -v limina16k | sort -V | tail -1)
  if [ -n "$STOCK_FALLBACK" ]; then
    grubby --set-default="$STOCK_FALLBACK" >/dev/null 2>&1 || true
    echo "   permanent default set to stock fallback: $(grubby --default-kernel)"
  else
    echo "ERROR: no stock kernel found to keep as a safe default; refusing to leave the unproven" >&2
    echo "       16k as the permanent default (would risk an unrecoverable boot). Aborting." >&2
    exit 1
  fi
fi
grub2-editenv - unset boot_indeterminate menu_auto_hide 2>/dev/null || true

# on-success promotion: once we are actually running the 16k kernel at multi-user, make it the
# default and self-disable. If 16k never boots, this never promotes and stock stays default.
cat > /etc/systemd/system/limina-kernel-promote.service <<PROMOTE
[Unit]
Description=Promote the limina 16k kernel to GRUB default after a verified boot
After=multi-user.target
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'uname -r | grep -q limina16k && grubby --set-default=/boot/vmlinuz-$KREL && echo "promoted 16k ($KREL) to default" || true'
ExecStartPost=/bin/sh -c 'systemctl disable limina-kernel-promote.service'
[Install]
WantedBy=multi-user.target
PROMOTE
systemctl daemon-reload
systemctl enable limina-kernel-promote.service >/dev/null 2>&1 || true

# one-shot next-boot = the 16k entry (by grubby index; GRUB auto-boots it, no keyboard needed) —
# UNLESS a btrfs free-space-tree conversion is still pending (FST_DEFER): the 16k cannot mount a
# v1 fs, so arm it only AFTER a plain stock boot builds the tree from the fstab change above.
if [ "$FST_DEFER" = 1 ]; then
  install_arm_16k_after_fst_service
  echo "   16k trial DEFERRED — a btrfs free-space-tree conversion is staged in /etc/fstab:"
  echo "     1) reboot now (you stay on stock) — that boot builds the free-space tree;"
  echo "     2) it then auto-arms the 16k; reboot once more to enter the enhanced kernel."
else
  K_INDEX=$(grubby --info="/boot/vmlinuz-$KREL" 2>/dev/null | sed -n 's/^index=//p' | head -1)
  if [ -n "$K_INDEX" ] && command -v grub2-reboot >/dev/null 2>&1; then
    grub2-reboot "$K_INDEX"
    echo "   one-shot next boot -> 16k (index $K_INDEX); auto-falls-back to stock if it fails"
  else
    echo "   WARN: could not arm a one-shot 16k boot — try manually: sudo grub2-reboot '$KREL'; sudo reboot" >&2
  fi
fi

echo "== enhanced-tier install complete. =="
if [ "$FST_DEFER" = 1 ]; then
  echo "   One extra step first (btrfs v1->v2): reboot now to build the free-space tree (you stay"
  echo "   on stock); the 16k trial then auto-arms. After that it boots ONCE on trial and, once it"
  echo "   reaches the desktop, auto-promotes to the default kernel."
else
  echo "   Reboot now. The 16k + venus desktop boots ONCE on trial:"
  echo "     - reaches the desktop -> auto-promoted to the default kernel;"
  echo "     - fails to boot       -> force a power-cycle; the guest auto-returns to stock."
  echo "   (limina has no keyboard at GRUB/emergency yet, so this trial-boot is the safe path.)"
fi
