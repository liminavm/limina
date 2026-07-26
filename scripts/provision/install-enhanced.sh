#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# limina enhanced-tier in-guest installer.
#
# Runs INSIDE a basic (stock) Fedora guest and upgrades it to the enhanced tier. Per the
# two-tier guarantee (CLAUDE.md) this is the bootstrap substrate: it must depend ONLY on what
# a stock F43 already has (dnf, grubby, systemctl, virtiofs) — NOT on the 16k kernel / venus
# it installs. It is delivered via a virtio-fs share (the "attached volume") and run
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
#   1. 16k kernel RPM   -> kernel-install/dracut writes a BLS entry; INSTALLONLY (co-exists with
#                          stock AND keeps the prior enhanced kernel as a fallback). The STOCK
#                          kernel is locked (not ours) so a stock update can't steal the default.
#   2. mesa RPMs        -> replace stock mesa at /usr (zink GL + venus Vulkan); LOCKED, but the
#                          installer lifts the lock to UPDATE it, then re-locks (re-runnable).
#   3. clipboard@limina  -> the gnome-shell extension (GNOME tier of the clipboard bridge) at
#                          /usr/share/gnome-shell/extensions; limina-agent-session self-enables
#                          it per user. Replaces the patched-mutter delivery (2026-07-11):
#                          stock mutter stays stock, nothing for a distro update to displace.
#   3b. local dnf repo   -> /usr/share/limina-guest-tools/repo + a .repo file: EVERY mesa
#                          subpackage Fedora ships (-devel/-tests included, debuginfo excluded),
#                          at OUR NEVRA. Installed on demand (`dnf install mesa-libgbm-devel`) —
#                          Fedora's own -devel subpackages can never resolve against the
#                          versionlocked runtime (exact-NEVRA requires vs exclude filtering).
#   4. limina-agent      -> /usr/local/bin + system unit (dynamic resize, PSI autoballoon);
#      limina-agent-session -> /usr/local/bin + systemd USER unit (the clipboard bridge — it
#                          must live in the graphical session, so it is a separate binary and
#                          is enabled --global for every user's next login).
#                          Both OPTIONAL: installed only if staged into the payload, and
#                          SELinux-relabeled so they start on a stock (Enforcing) guest.
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

# Collect runtime RPMs for a glob, excluding debug/devel/tests (installed by DEFAULT on every
# guest; the devel/tests subpackages ship via the local repo of step 3b instead, on demand).
runtime_rpms() {  # <glob>
  ls $1 2>/dev/null | grep -vE 'debuginfo|debugsource|-devel-|-tests-' || true
}

# Payload RPMs for optional (-devel/-tests) subpackages the guest ALREADY installed (e.g.
# mesa-libgbm-devel pulled from the step-3b repo). They MUST ride the same upgrade transaction
# as the runtime set: their requires are exact-NEVRA, so upgrading the runtime alone would make
# `dnf --allowerasing` silently ERASE them instead.
installed_extra_rpms() {  # <glob>
  local r n
  for r in $(ls $1 2>/dev/null | grep -E -- '-devel-|-tests-' | grep -vE 'debuginfo|debugsource'); do
    n=$(rpm -qp --qf '%{NAME}' "$r" 2>/dev/null) || continue
    rpm -q "$n" >/dev/null 2>&1 && echo "$r"
  done
  return 0
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
  # Don't clobber a prior backup on re-run — keep the ORIGINAL pre-limina fstab.
  [ -e /etc/fstab.limina.bak ] || cp -a /etc/fstab /etc/fstab.limina.bak
  awk 'BEGIN{OFS="\t"}
    /^[[:space:]]*#/ || NF<4 || $3!="btrfs" { print; next }
    {
      n=split($4,a,","); o=""
      for(i=1;i<=n;i++){ if(a[i] ~ /^space_cache/) continue; o=(o==""?a[i]:o","a[i]) }
      $4=(o==""?"space_cache=v2":o",space_cache=v2"); print
    }' /etc/fstab > /etc/fstab.limina.new
  # Count records with awk NR (not `wc -l`): awk counts the final line even with NO trailing newline,
  # whereas wc -l counts newlines — and awk's reprint ADDS a trailing newline, so a fstab without one
  # would make wc -l report new=orig+1 and FALSE-REJECT the rewrite (silently stranding the deferred
  # tree-build). awk NR == awk NR is newline-robust.
  if [ "$(awk 'END{print NR}' /etc/fstab.limina.new)" = "$(awk 'END{print NR}' /etc/fstab)" ] \
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
# Pick the NEWEST limina16k kernel (sort -V | tail -1), NOT `ls | head -1` (lexical = OLDEST): with
# multiple co-installed enhanced kernels the newest is the one this install added — and the one the
# promote service ($KREL, baked at install time) will promote. head -1 would TRIAL the old kernel
# while the promote service promotes the new (never-trial-booted) one -> the unproven-default brick.
k=$(ls /boot/vmlinuz-*limina16k* 2>/dev/null | sort -V | tail -1)
[ -n "$k" ] || exit 0
# Arm by BLS ENTRY ID, not grubby's numeric index: grub2-reboot's numeric form selects by GRUB
# blscfg MENU POSITION (version-sorted via rpmvercmp, where stock 6.x.y-NNN outranks *-limina16k),
# which does NOT match grubby's index — a numeric one-shot booted the wrong entry (stock).
id=$(grubby --info="$k" 2>/dev/null | sed -n 's/^id="\(.*\)"$/\1/p' | head -1)
if [ -n "$id" ]; then
  grub2-reboot "$id"
  logger -t limina "btrfs free-space tree ready; armed 16k ($k) — reboot to enter the enhanced kernel"
  systemctl disable limina-arm-16k.service   # self-disable ONLY after a successful arm
else
  logger -t limina "16k arm FAILED (no BLS id for $k) — leaving the service enabled to retry next boot"
fi
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

# Capture the CURRENT default kernel BEFORE we install the 16k — Fedora's kernel install
# auto-promotes the newest kernel to default, and step 6 must restore the PREVIOUS default as the
# permanent fallback (two-tier safety; see step 6).
STOCK_DEFAULT=$(grubby --default-kernel 2>/dev/null || true)

# Is the payload kernel ALREADY installed at this exact NVR? Compute it up front so the /boot
# pre-flight below fires ONLY when an install would actually WRITE to /boot: a no-op re-run (e.g. to
# bump mesa) on a tight /boot must NOT abort here when the kernel section will write nothing.
KNVR=$(rpm -qp --qf '%{NAME}-%{VERSION}-%{RELEASE}' "$RPM" 2>/dev/null)
KERNEL_PRESENT=0
if [ -n "$KNVR" ] && rpm -q "$KNVR" >/dev/null 2>&1; then KERNEL_PRESENT=1; fi

# Pre-flight (ONLY when a kernel install will occur): the 16k vmlinuz + initramfs land on /boot. A
# FULL /boot makes dracut emit a truncated/driverless initramfs that boots far enough to run
# systemd-in-initramfs but then cannot mount root -> the dracut emergency shell (and limina has no
# keyboard there to recover — the dogfooding brick of 2026-06-29). Fail EARLY with a cleanup hint.
if [ "$KERNEL_PRESENT" != 1 ]; then
  BOOT_FREE_MB=$(($(df -Pk /boot | awk 'NR==2{print $4}') / 1024))
  echo "-- /boot free: ${BOOT_FREE_MB} MiB"
  if [ "${BOOT_FREE_MB:-0}" -lt 350 ]; then
    echo "ERROR: /boot has < 350 MiB free — the 16k kernel may not fit and would produce an" >&2
    echo "       unbootable initramfs. Free space first — prune an OLD kernel you don't need" >&2
    echo "       (dnf won't remove the running one; keep one enhanced kernel as a fallback):" >&2
    echo "         sudo dnf remove \$(dnf repoquery --installonly --latest-limit=-1 -q)   # all but newest per name" >&2
    echo "         # untracked /boot leftovers (no owning RPM): sudo kernel-install remove <uname-r>" >&2
    exit 1
  fi
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

# Clear any STALE limina-kernel-16k versionlock a previous installer left (locking our own kernel
# did nothing for stock and blocked our own updates); harmless if absent. We no longer lock it.
dnf versionlock delete limina-kernel-16k >/dev/null 2>&1 || true
# Install the kernel with rpm -i, NOT dnf install, so it CO-EXISTS with the running enhanced kernel
# (keeps the prior one as a fallback) instead of replacing it. Even with Provides:
# installonlypkg(kernel), dnf treats the new RPM as an UPGRADE of the installed older
# limina-kernel-16k (which predates the provide) and tries to REMOVE it — but that's the running
# kernel, so dnf aborts: "cannot install both ... conflicting requests". rpm -i adds the new
# versioned files with no conflict (/boot/vmlinuz-<rel>, /lib/modules/<rel>); %posttrans
# (kernel-install add) writes the BLS entry + initramfs just as dnf would. The provide keeps FUTURE
# dnf installonly bookkeeping correct. Idempotent: skip if this exact version is already installed
# (KNVR / KERNEL_PRESENT were computed above, before the /boot pre-flight).
if [ "$KERNEL_PRESENT" = 1 ]; then
  echo "-- kernel $KNVR already installed; skipping kernel install (idempotent)"
else
  echo "-- installing kernel (rpm -i, co-installs beside the running kernel): $(basename "$RPM")"
  rpm -ivh "$RPM"
fi
# KREL of the kernel we just installed — from the RPM's kernel-uname-r provide, NOT a /lib/modules
# glob: multiple limina16k kernels now co-exist, so a glob+head would pick the wrong (older) one.
KREL=$(rpm -qp --provides "$RPM" 2>/dev/null | sed -n 's/^kernel-uname-r = //p' | head -1)
[ -n "$KREL" ] || KREL=$(ls -d /lib/modules/*limina16k* 2>/dev/null | xargs -n1 basename | sort -V | tail -1)
[ -n "$KREL" ] || { echo "kernel modules dir not found after install"; exit 1; }
IMG="/boot/initramfs-$KREL.img"
[ -f "$IMG" ] || { echo "dracut did NOT build an initramfs for $KREL"; exit 1; }
ls -1 "/boot/loader/entries/"*"$KREL"*.conf >/dev/null || { echo "no BLS entry for $KREL"; exit 1; }

# Verify the kernel can actually mount THIS guest's root — do NOT trust mere existence (that was
# the brick: an initramfs present but missing the root driver, so it dropped to emergency). A
# driver counts as present if it is an initramfs MODULE *or* BUILT INTO the kernel (=y): the limina
# kernel builds virtio_blk/btrfs/virtio_input =y on purpose ("boot never depends on initramfs
# contents"), so they are NOT in the initramfs — checking only lsinitrd false-negatives and would
# abort a perfectly bootable install. Also consult modules.builtin.
ROOT_FS=$(findmnt -no FSTYPE / 2>/dev/null || echo btrfs)
BUILTIN="/lib/modules/$KREL/modules.builtin"
have_drv() {  # present as an initramfs module OR built into the kernel?
  lsinitrd "$IMG" 2>/dev/null | grep -q "$1" && return 0
  grep -q "$1" "$BUILTIN" 2>/dev/null && return 0
  return 1
}
miss=""
have_drv 'virtio_blk' || miss="$miss virtio_blk"
have_drv "$ROOT_FS"   || miss="$miss $ROOT_FS"
if [ -n "$miss" ]; then
  echo "ERROR: the 16k kernel cannot mount root — missing driver(s) (neither initramfs nor built-in):$miss" >&2
  echo "       It would drop to the (keyboard-less) emergency shell. Rebuild the initramfs with:" >&2
  echo "         sudo dracut -f --add-drivers \"virtio_blk $ROOT_FS\" \"$IMG\" \"$KREL\"" >&2
  exit 1
fi
have_drv 'virtio_input' \
  || echo "   WARN: no virtio_input (initramfs or built-in) — the emergency shell may have no keyboard"
# Pin the STOCK kernel (the names must match what you're actually pinning). dnf auto-promotes the
# newest installed kernel to the default; a stock kernel-core UPDATE would otherwise grab the
# default away from our 16k. Lock the stock kernel packages at their current version so stock stays
# the (working, unchanging) fallback and the 16k keeps the default. limina-kernel-16k is a DIFFERENT
# name, so it stays freely updatable (and installonly preserves the prior enhanced kernel).
for p in kernel kernel-core kernel-modules kernel-modules-core; do
  rpm -q "$p" >/dev/null 2>&1 && dnf versionlock add "$p" >/dev/null 2>&1 || true
done
echo "   kernel $KREL: initramfs verified (virtio_blk + $ROOT_FS) + BLS entry present"
echo "   (installonly: co-installed beside stock + prior enhanced; stock kernel pinned)"

### 2. mesa RPMs (26.2 zink+venus) -> REPLACE stock at /usr; versionlock #########
MESA_RPMS=$(runtime_rpms "$PAYLOAD/mesa-*.rpm")
[ -n "$MESA_RPMS" ] || { echo "no mesa RPMs in payload"; exit 1; }
# Guard against a STALE-OVERLAY payload (hit 2026-07-20): extracting a new tarball over an
# old payload dir leaves the previous mesa NEVRA's RPMs beside the new ones (tar does not
# delete), and dnf then sees both versions "from @commandline" and fails the transaction.
# All payload mesa RPMs must share one VERSION-RELEASE.
MESA_VRS=$(for r in $MESA_RPMS; do rpm -qp --qf '%{VERSION}-%{RELEASE}\n' "$r" 2>/dev/null; done | sort -u)
[ "$(echo "$MESA_VRS" | wc -l)" -eq 1 ] || {
  echo "ERROR: payload holds MIXED mesa versions ($(echo $MESA_VRS | tr '\n' ' ')) — stale" >&2
  echo "       extract-over-old-dir? rm the payload dir and re-extract the tarball fresh." >&2
  exit 1
}
# Carry any already-installed mesa -devel/-tests along in the SAME transaction (see the helper).
MESA_EXTRAS=$(installed_extra_rpms "$PAYLOAD/mesa-*.rpm")
[ -z "$MESA_EXTRAS" ] || MESA_RPMS="$MESA_RPMS
$MESA_EXTRAS"
echo "-- installing mesa (replaces stock -> our venus/zink build):"; echo "$MESA_RPMS" | sed 's#.*/#     #'
# Lift any EXISTING mesa versionlock FIRST. On a re-run / update the lock pins the currently
# installed enhanced mesa, so a newer build would be "filtered out by exclude filtering" and never
# install — lifting before install is what lets the installer UPDATE a versionlocked package. On a
# fresh guest there is no lock yet (no-op). We re-lock immediately after. (This needs the payload's
# mesa to be a STRICTLY NEWER NEVRA than what's installed; the build bumps LIMINA_REL for that.)
# Delete by EXACT name: dnf5's `versionlock delete <glob>` is a silent no-op that still exits 0
# (so a `glob || per-package` form never falls through). Iterate the installed mesa packages.
for p in $(rpm -qa 'mesa-*' --qf '%{NAME}\n' | sort -u); do
  dnf versionlock delete "$p" >/dev/null 2>&1 || true
done
# `dnf install` of the higher-versioned local RPMs upgrades/replaces the matching packages. GUARD it:
# the lock is now LIFTED, and under `set -e` a failed install would abort the script BEFORE the
# re-lock below — leaving mesa UNLOCKED so a later `dnf update` could drag it back to a venus-breaking
# stock version (a silent, keyboard-less-to-recover regression on a daily driver). On failure, dnf is
# transactional (the prior enhanced mesa stays installed), so re-lock THAT and abort loudly.
if ! dnf install -y --allowerasing $MESA_RPMS; then
  echo "ERROR: mesa install failed — restoring the mesa versionlock and aborting (mesa unchanged)" >&2
  for p in $(rpm -qa 'mesa-*' --qf '%{NAME}\n' | sort -u); do dnf versionlock add "$p" >/dev/null 2>&1 || true; done
  exit 1
fi
# Re-lock the mesa stack so a later `dnf update` cannot revert it to a venus-breaking stock version.
for p in $(rpm -qa 'mesa-*' --qf '%{NAME}\n' | sort -u); do
  dnf versionlock add "$p" >/dev/null 2>&1 || true
done
echo "   mesa installed + (re-)versionlocked"
rpm -q mesa-vulkan-drivers --qf '   venus ICD pkg: %{NVRA}\n' 2>/dev/null || true

### 3. clipboard@limina gnome-shell extension -> /usr/share/gnome-shell/extensions ###
# The GNOME tier of the clipboard bridge (guest/gnome-shell-extension/): plain GJS
# files scripting Meta.Selection inside the compositor — quiet (no screen-share
# indicator) without the retired patched-mutter carry, and immune to distro mutter
# updates. Enabling is per-user dconf state with no session at install time, so
# limina-agent-session enables it on its first probe (once per user; a later user
# disable is respected). A previously-installed patched mutter (older enhanced
# guests) is left alone: harmless while present, and the next distro update
# replaces it with stock — the helper's tier ladder absorbs either state.
EXT_UUID="clipboard@limina"
if [ -d "$PAYLOAD/$EXT_UUID" ]; then
  echo "-- installing the $EXT_UUID gnome-shell extension"
  EXT_DST="/usr/share/gnome-shell/extensions/$EXT_UUID"
  rm -rf "$EXT_DST"
  install -d "$(dirname "$EXT_DST")"
  cp -a "$PAYLOAD/$EXT_UUID" "$EXT_DST"
  # cp -a from the virtiofs share preserves xattrs — relabel for enforcing guests.
  if command -v restorecon >/dev/null 2>&1; then
    restorecon -R "$EXT_DST" 2>/dev/null || true
  fi
  echo "   installed at $EXT_DST (limina-agent-session enables it at next login)"
else
  echo "-- no $EXT_UUID in the payload — skipped (clipboard rides the RemoteDesktop fallback)"
fi

### 3b. local dnf repo: every mesa subpackage Fedora ships, installable on demand ###
# The runtime mesa stack is versionlocked at our NEVRA (step 2), which EXCLUDES the stock mesa
# versions — so Fedora's own -devel subpackages (exact-NEVRA `Requires: mesa-libgbm(aarch-64) =
# <stock>`) can never resolve on an enhanced guest ("filtered out by exclude filtering"). This
# repo serves OUR builds of the full subpackage set at the matching NEVRA, so a plain
# `dnf install mesa-libgbm-devel` works exactly as it does on stock Fedora. The repodata is
# pre-built by package-payload.sh (createrepo_c in the build guest) — nothing here needs network.
if [ -d "$PAYLOAD/repo/repodata" ]; then
  echo "-- installing the limina-guest-tools local dnf repo (-devel/-tests on demand)"
  rm -rf /usr/share/limina-guest-tools/repo
  install -d /usr/share/limina-guest-tools
  cp -a "$PAYLOAD/repo" /usr/share/limina-guest-tools/repo
  cat > /etc/yum.repos.d/limina-guest-tools.repo <<'REPOCONF'
# limina enhanced tier: local repo carrying our FULL mesa subpackage set (the runtime is
# versionlocked at the limina NEVRA, so Fedora's exact-version -devel subpackages cannot resolve;
# these match). Installed by install-enhanced.sh; refreshed on every enhanced upgrade.
[limina-guest-tools]
name=limina guest tools (local)
baseurl=file:///usr/share/limina-guest-tools/repo
enabled=1
gpgcheck=0
REPOCONF
  # cp -a from the virtiofs share preserves xattrs — relabel so enforcing guests read it cleanly.
  if command -v restorecon >/dev/null 2>&1; then
    restorecon -R /usr/share/limina-guest-tools /etc/yum.repos.d/limina-guest-tools.repo 2>/dev/null || true
  fi
  echo "   repo live: e.g. 'dnf install mesa-libgbm-devel' now resolves against our build"
else
  echo "-- payload has no repo/ (pre-repo payload); -devel subpackages stay uninstallable on this guest"
fi

### 4. limina-agent + limina-agent-session (OPTIONAL; present iff staged into the payload) ##
# Folds in what scripts/install-guest-agent.sh used to do over SSH, so the whole enhanced upgrade
# rides the one offline virtiofs channel. Without the agent, dynamic display resize /
# PSI autoballoon / share auto-mount stay inactive — so a payload that ships it is preferred, but
# its absence is non-fatal (two-tier: each feature lights up on its own prerequisite).
AGENT_BIN="$PAYLOAD/limina-agent"
if [ -f "$AGENT_BIN" ]; then
  echo "-- installing limina-agent + unit"
  install -m 0755 "$AGENT_BIN" /usr/local/bin/limina-agent
  UNIT="$PAYLOAD/limina-agent.service"
  [ -f "$UNIT" ] && install -m 0644 "$UNIT" /etc/systemd/system/limina-agent.service
  # The flat-pointer gschema override is RETIRED (2026-07-23): captured motion now drives the
  # absolute tablet (host-integrated, macOS-accelerated deltas), which libinput never
  # accelerates — no guest-side profile tweak needed. Remove it from previously-provisioned
  # guests so a passed-through real mouse keeps the distro default profile.
  if [ -f /usr/share/glib-2.0/schemas/90-limina-pointer.gschema.override ]; then
    rm -f /usr/share/glib-2.0/schemas/90-limina-pointer.gschema.override
    glib-compile-schemas /usr/share/glib-2.0/schemas/ >/dev/null 2>&1 || true
    echo "   removed retired 90-limina-pointer.gschema.override (flat-profile capture workaround)"
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
  echo "-- no limina-agent in payload; skipping (dynamic resize / PSI stay inactive)"
fi

# limina-agent-session: the per-user clipboard bridge. It is session state (an
# ext-data-control Wayland client, the clipboard@limina extension bridge on GNOME, or
# mutter's RemoteDesktop D-Bus API as the last resort), so it runs as a systemd USER
# unit inside the graphical session — the system-level limina-agent cannot do
# clipboard, and without this helper host<->guest copy/paste is silently inert (no guest peer
# ever advertises the `clipboard` cap; found on the 2026-07-01 dogfooding VM).
SESSION_BIN="$PAYLOAD/limina-agent-session"
if [ -f "$SESSION_BIN" ]; then
  echo "-- installing limina-agent-session + user unit (clipboard bridge)"
  install -m 0755 "$SESSION_BIN" /usr/local/bin/limina-agent-session
  SESSION_UNIT="$PAYLOAD/limina-agent-session.service"
  [ -f "$SESSION_UNIT" ] && install -m 0644 "$SESSION_UNIT" /usr/lib/systemd/user/limina-agent-session.service
  if command -v restorecon >/dev/null 2>&1; then
    restorecon -v /usr/local/bin/limina-agent-session \
      /usr/lib/systemd/user/limina-agent-session.service 2>/dev/null || true
  fi
  # --global enables it for EVERY user's graphical session (symlink under /etc/systemd/user);
  # no user bus needed, so it works from this root, offline installer. It starts at the next
  # login — the reboot this install already asks for covers that.
  if systemctl --user --global enable limina-agent-session 2>/dev/null; then
    echo "   limina-agent-session enabled --global (starts at each user's next graphical login)"
  else
    echo "   WARN limina-agent-session could not be enabled (clipboard stays inactive;" >&2
    echo "        try: sudo systemctl --user --global enable limina-agent-session)" >&2
  fi
else
  echo "-- no limina-agent-session in payload; skipping (host<->guest clipboard stays inactive)"
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
  # No VN_PERF here. `no_fence_feedback` used to be set and was RETIRED 2026-07-25.
  #
  # It was a workaround from the MoltenVK era: venus's feedback buffers are a host-written
  # counter in a host-visible blob, and that blob was not coherent on the 16 KiB hv_vm_map path,
  # so venus's poll loop spun forever (spikes/venus-render-server, Finding 6). The coherency bug
  # was fixed 2026-07-03 (libkrun 0043 + virglrenderer 0023 + patches/linux/0004), which removed
  # the reason for the flag; it just outlived it.
  #
  # Keeping it was not free. Without feedback every fence status check is a synchronous
  # driver<->renderer round trip (vn_queue.c: NO_FENCE_FEEDBACK skips the feedback slot, so
  # vn_GetFenceStatus falls through to vn_call_vkGetFenceStatus); with it, the check is a plain
  # guest-memory read. gnome-shell-rs measured the difference on submits carrying real work:
  # 13.6-15.3 ms for eight blur submits with feedback vs 17.7-22.1 ms without, i.e. 25-30% of the
  # wall clock, while a batched one-submit control is identical either way -- so it is the wait,
  # not the work.
  #
  # Verified before removal: with the flag gone the guest reboots, gnome-shell comes up on
  # zink->venus, and vkmark scores 1985 against 1929 with the workaround. The old hang does not
  # return -- unsurprising in hindsight, since only the FENCE flag was still set while the path
  # that actually hung was the SEMAPHORE one, and that has been running enabled for a long time.
  #
  # CHECK ON A MESA BUMP PAST 26.2.x: upstream's 4c1938c8adb "venus: deprecate fence feedback"
  # (2026-07-12) changes this area, and we have NOT established which way. Two readings, opposite
  # consequences: if the FLAG went and feedback is now unconditional, the bump hands us the fast
  # path for free; if the MECHANISM went, we land permanently on the synchronous path with no way
  # back and need to carry a revert. The mesa-cs tree cannot settle it (its venus history stops at
  # 2026-04-03), so read the commit before bumping. Either way removing the flag here is correct --
  # it is dead config under one reading and aligned with upstream under the other.
  #
  # Measured 2026-07-25, which moves the trigger: the F43 enhanced image's mesa 26.2.0-3.limina
  # STILL ships no_fence_feedback (checked with the strings command below, in-guest), so the
  # deprecation is not in 26.2.0 and lands in a later release. 26.1.x and 26.2.x are both fine.
  # Check the INSTALLED driver, not a source tree:
  #   strings /usr/lib64/libvulkan_virtio.so | grep '^no_'
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

echo "-- restoring the PREVIOUS default kernel as permanent; the new 16k gets a one-shot trial boot"
if [ -n "$STOCK_DEFAULT" ]; then
  grubby --set-default="$STOCK_DEFAULT" >/dev/null 2>&1 || true   # undo dnf's auto-promote of the new 16k
  # NB: on an already-enhanced guest the previous default is itself a (proven-working) 16k kernel,
  # not bare stock — a good upgrade fallback. The word "stock" is avoided here on purpose.
  echo "   permanent default kept at the previous default: $(grubby --default-kernel)"
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

# on-success promotion: once we are actually running THE TRIALED kernel ($KREL) at multi-user, make
# it the default and self-disable. Match the EXACT trialed release with `grep -qxF "$KREL"`, NOT a
# loose `grep -q limina16k`: on a guest whose permanent fallback default is ITSELF a 16k kernel —
# i.e. ANY already-enhanced guest, e.g. dogfood-guest's 7.0.13-limina16k — a loose match would fire on
# the FALLBACK boot after a FAILED $KREL trial and promote the broken $KREL to the permanent default
# → unrecoverable (no keyboard at GRUB). In the unquoted heredoc $KREL is baked at write time while
# `uname -r` (a runtime pipe, not $(...)) is evaluated at boot, so this promotes only after the
# trialed kernel itself reaches multi-user.
cat > /etc/systemd/system/limina-kernel-promote.service <<PROMOTE
[Unit]
Description=Promote the limina 16k kernel to GRUB default after a verified boot
After=multi-user.target
[Service]
Type=oneshot
ExecStart=/bin/sh -c 'uname -r | grep -qxF "$KREL" && grubby --set-default=/boot/vmlinuz-$KREL && echo "promoted 16k ($KREL) to default" || true'
ExecStartPost=/bin/sh -c 'systemctl disable limina-kernel-promote.service'
[Install]
WantedBy=multi-user.target
PROMOTE
systemctl daemon-reload
systemctl enable limina-kernel-promote.service >/dev/null 2>&1 || true

# one-shot next-boot = the 16k entry (by BLS entry ID; GRUB auto-boots it, no keyboard needed) —
# UNLESS a btrfs free-space-tree conversion is still pending (FST_DEFER): the 16k cannot mount a
# v1 fs, so arm it only AFTER a plain stock boot builds the tree from the fstab change above.
if [ "$FST_DEFER" = 1 ]; then
  install_arm_16k_after_fst_service
  echo "   16k trial DEFERRED — a btrfs free-space-tree conversion is staged in /etc/fstab:"
  echo "     1) reboot now (you stay on stock) — that boot builds the free-space tree;"
  echo "     2) it then auto-arms the 16k; reboot once more to enter the enhanced kernel."
else
  # Clean up any stale arm service a PRIOR deferred run may have left enabled — we arm the one-shot
  # directly below, so a leftover limina-arm-16k.service must not also fire later and re-arm.
  systemctl disable limina-arm-16k.service >/dev/null 2>&1 || true
  # By BLS entry ID, NOT grubby's numeric index: grub2-reboot's numeric form selects by GRUB
  # blscfg MENU POSITION (version-sorted via rpmvercmp, where stock 6.x.y-NNN outranks *-limina16k),
  # which differs from grubby's index — a numeric one-shot booted STOCK instead of the 16k
  # (root-caused + validated 2026-06-30). The id string feeds `set default=` and blscfg matches it
  # position-independently, exactly like saved_entry. Proven: one-shot-by-id overrode the default
  # and selected the intended 16k entry, then `next_entry` cleared (falls back if the 16k fails).
  K_ID=$(grubby --info="/boot/vmlinuz-$KREL" 2>/dev/null | sed -n 's/^id="\(.*\)"$/\1/p' | head -1)
  if [ -n "$K_ID" ] && command -v grub2-reboot >/dev/null 2>&1; then
    grub2-reboot "$K_ID"
    echo "   one-shot next boot -> 16k ($K_ID); auto-falls-back to stock if it fails"
  else
    echo "   WARN: could not arm a one-shot 16k boot. The kernel IS installed but won't boot until" >&2
    echo "         armed. One-shot it by entry id — 'sudo grubby --info=/boot/vmlinuz-$KREL' shows the" >&2
    echo "         'id=' field; then: sudo grub2-reboot '<that id>'; sudo reboot" >&2
    echo "         Do NOT 'grubby --set-default' the unproven kernel (no keyboard to recover if it fails)." >&2
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
