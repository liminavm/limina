#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# LiminaOS A/B rollback + recovery, on REAL KRUN_EFI.
#
# The happy path is already green on real firmware and rollback is green under TCG. This
# harness exists because those are not the same evidence: under TCG the recovering component
# is TCG's systemd-boot, and rollback is precisely the path where the firmware — not the
# guest — is the system under test.
#
# Leg R (rollback): install a payload whose verity superblock is corrupt, then power-cycle
#   repeatedly and watch sd-boot spend the new entry's tries and fall back.
# Leg C (recovery): from the rolled-back state, install a good 0.3 and confirm the machine
#   climbs back out, the survivor is untouched, and the dead entry is evicted.
#
# THREE THINGS ARE SCORED SEPARATELY, and that split is the point of the harness:
#   (a) the boot-count sequence, read HOST-side from the ESP with the guest powered off;
#   (b) a verity marker in the failing boot's console — positive evidence of WHERE it died,
#       not merely that it did;
#   (c) the slot the machine finally lands on.
# Collapsing these into "did it end up in the right slot" is how a lazily-invisible
# corruption reads as a pass: dm-verity hashes a block when something reads it, so a
# payload corrupted in the middle of the filesystem boots cleanly and gets blessed. This
# payload corrupts the SUPERBLOCK, so it fails at activation — but the scoring must not
# depend on that being true, or it stops being a test.
#
# Assertions only ever match guest-produced output. The serial console echoes everything we
# type, so a grep for a string we sent will match our own keystrokes and report success by
# construction. Nothing below greps for a marker this script typed.
#
# No `sed`, no `find`, no `grep -o` in-guest: this image has a deliberately small userland
# and a missing binary makes a check come back BLANK rather than failing. Guest-side output
# is built with shell builtins and `echo` only.
set -u

IMG=${IMG:-/tmp/ab2.raw}
PAY=${PAY:-/tmp/ab2pay}
SHARE_BAD="$PAY/updates-bad-verity"
SHARE_GOOD3="$PAY/updates-good3"
WORK=${WORK:-/tmp/rollback-run}
# Prefer the delivered bundle: it is the artifact users actually run, and its worker is already
# codesigned. A bare `target/debug/limina-vmm` has NO com.apple.security.hypervisor entitlement
# and HVF refuses it — which presents as a broken build rather than a missing signature. Fix
# that with `crates/limina-vmm/sign.sh <profile>`, which signs in place and so cannot relink
# binaries under another session's running VM the way a rebuild can.
# Note /Applications/Limina.app does NOT exist on the dev Mac: the deliverable is copied to the
# dogfood machine, so the local path is target/.
for cand in "$(dirname "$0")/../../target/Limina.app" /Applications/Limina.app; do
  if [ -x "$cand/Contents/MacOS/limina" ]; then
    LIM=${LIM:-$cand/Contents/MacOS/limina}
    FW=${FW:-$cand/Contents/Resources/KRUN_EFI.gop.fd}
    break
  fi
done
LIM=${LIM:-$(dirname "$0")/../../target/debug/limina}
FW=${FW:-$(dirname "$0")/../../target/krun-efi/KRUN_EFI.gop.fd}
VERIFY=${VERIFY:-$(dirname "$0")/../liminaos-slot-health/verify-slot.py}
PASS='liminatest123'
HASH='$6$liminatest$NN7NsF0PG3XGXCYSCcQIq8pkgxS0XSgo0VEsu4Q2Ce5SNwlUK38GCvj6Tvl18UUhzysPkCBiucCGDebgPmHUU1'

# ---------------------------------------------------------------- preflight
# Check every input BEFORE booting anything. A missing payload directory makes sysupdate
# report "no update available" and exit 0, which reads as a clean run that found nothing to
# do — indistinguishable from success at a glance.
fail=0
for p in "$IMG" "$SHARE_BAD" "$SHARE_GOOD3" "$LIM" "$FW" "$VERIFY"; do
  [ -e "$p" ] || { echo "PREFLIGHT: missing $p"; fail=1; }
done
[ "$fail" = 0 ] || { echo "FATAL: preflight failed — nothing was booted"; exit 1; }
for d in "$SHARE_BAD" "$SHARE_GOOD3"; do
  n=$(ls -1 "$d" | wc -l | tr -d ' ')
  [ "$n" -gt 0 ] || { echo "FATAL: $d is EMPTY"; exit 1; }
  echo "preflight: $d holds $n files"
done

# Prove the two payload trees are the ones we think they are. They are NOT distinguishable by
# listing: the filename encodes the INTENDED root hash, and corrupting a file does not change
# its name, so `updates-good/` and `updates-bad-verity/` hold identically-named files. Worse,
# the bad tree's SHA256SUMS is regenerated over the corrupt payload and verifies clean — by
# design, so the guest's own pre-check passes and sysupdate is genuinely reached. Nothing
# short of reading the bytes tells them apart, and mounting the wrong one produces a rollback
# test that passes for the wrong reason: a payload that never reached the update mechanism
# looks, from outside, exactly like one the mechanism correctly refused.
#
# The discriminator is the squashfs magic ('hsqs') at offset 0 — which is the very thing
# bad-verity2 corrupts, so it is also a direct check that the intended defect is present.
squashfs_magic() {   # $1 = directory
  python3 - "$1" <<'PY'
import sys, os, glob
d = sys.argv[1]
hits = sorted(glob.glob(os.path.join(d, "*.squashfs")))
if not hits:
    print("NONE"); raise SystemExit
with open(hits[0], "rb") as f:
    print("hsqs" if f.read(4) == b"hsqs" else "CORRUPT")
PY
}
good3_magic=$(squashfs_magic "$SHARE_GOOD3")
bad_magic=$(squashfs_magic "$SHARE_BAD")
echo "preflight: updates-good3   squashfs superblock = $good3_magic (expect hsqs)"
echo "preflight: updates-bad-verity squashfs superblock = $bad_magic (expect CORRUPT)"
[ "$good3_magic" = "hsqs" ] || { echo "FATAL: the GOOD 0.3 payload is not a valid squashfs — wrong tree or a damaged copy"; exit 1; }
[ "$bad_magic" = "CORRUPT" ] || { echo "FATAL: the BAD payload has an intact superblock — the defect under test is ABSENT, so a green run would mean nothing"; exit 1; }
echo

rm -rf "$WORK"; mkdir -p "$WORK"
# Assert the working copy is COMPLETE. A short copy (out of disk space) boots and hangs in
# gpt-auto-root, which reads as a defect in the image rather than a truncated file — that
# cost a day once and the assert is cheaper than the confusion.
cp "$IMG" "$WORK/disk.raw" || { echo "FATAL: cp failed"; exit 1; }
src=$(stat -f%z "$IMG"); dst=$(stat -f%z "$WORK/disk.raw")
[ "$src" = "$dst" ] || { echo "FATAL: working copy TRUNCATED ($dst of $src)"; df -h /tmp | tail -1; exit 1; }
echo "working copy complete: $dst bytes"
echo

# ---------------------------------------------------------------- guest driving
SUP=""; HOLDER=""; FIFO=""

launch() {   # $1 = tag, $2 = share dir ("" for none)
  local tag="$1" share="$2"
  FIFO="$WORK/$tag.in"; rm -f "$FIFO"; mkfifo "$FIFO"
  sleep 100000 > "$FIFO" & HOLDER=$!
  local args=(--firmware "$FW" --disk "$WORK/disk.raw" --cpus 2 --ram-mib 2048
              --console "$WORK/$tag.log" --console-input "$FIFO"
              --smbios-oem-string "io.systemd.credential:passwd.hashed-password.root=$HASH")
  [ -n "$share" ] && args+=(--share "updates=$share")
  "$LIM" "${args[@]}" > "$WORK/$tag.sup.log" 2>&1 &
  SUP=$!
}

# Wait for a login prompt. Returns 0 if the guest reached login, 1 if it did not.
# A boot that fails verity does not reboot itself — it stops in the initrd's emergency
# shell — so "did not reach login" is a timeout, and the power cycle is ours to perform.
await_login() {   # $1 = tag, $2 = max seconds
  local tag="$1" max="$2" i
  for ((i = 0; i < max; i += 2)); do
    sleep 2
    tr '\r' '\n' < "$WORK/$tag.log" 2>/dev/null | grep -qa 'login:' && return 0
  done
  return 1
}

login_as_root() {
  sleep 3
  printf 'root\n' > "$FIFO"; sleep 3
  printf '%s\n' "$PASS" > "$FIFO"; sleep 6
}

halt_guest() {
  kill $SUP 2>/dev/null; sleep 5
  pkill -f "limina-vmm .*$(basename "$WORK")" 2>/dev/null; sleep 2
  pkill -9 -f "limina-vmm .*$(basename "$WORK")" 2>/dev/null
  kill $HOLDER 2>/dev/null; sleep 1
}

# ---------------------------------------------------------------- host-side scoring
esp_files() {
  python3 "$VERIFY" "$WORK/disk.raw" 2>/dev/null | grep -aoE '[A-Za-z0-9_.+-]+\.efi' | sort | tr '\n' ' '
}

snapshot() {   # $1 = label
  echo "----- host-side, guest powered off: $1 -----"
  # --verify walks the whole tree. That is the point: it scores the corrupt slot CORRUPT
  # host-side, with the guest powered off, independently of whatever the console said —
  # so scoring (b) rests on two unrelated observations rather than on log greps alone.
  python3 "$VERIFY" --verify "$WORK/disk.raw" 2>&1
  echo
}

# Guest-produced evidence that VERITY specifically is what refused. Never a string we typed.
#
# The pattern must stay narrow. A first version also matched `dracut` and `emergency`, and the
# shutdown flood ("Stopped dracut … hook", "Reached target Emergency Mode") then pushed the
# actual cause out of the tail — leaving a record indistinguishable from ANY initrd failure,
# which is the exact collapse this split-out channel exists to prevent. Match the cause, not
# the aftermath; the aftermath is reported separately below.
verity_markers() {   # $1 = tag
  tr '\r' '\n' < "$WORK/$1.log" 2>/dev/null \
    | grep -aiE 'device-mapper: verity|dm-verity|veritysetup|Buffer I/O error|Failed to (start|set up).*[Vv]erity' \
    | head -6
  # The landing state, kept deliberately to one line so it cannot crowd out the cause above.
  tr '\r' '\n' < "$WORK/$1.log" 2>/dev/null \
    | grep -aoE 'Cannot open access to console, the root account is locked' | tail -1
}

SEQ=""      # accumulated ESP filename sequence, one snapshot per line
record_seq() { SEQ="$SEQ$1: $(esp_files)"$'\n'; }

########################################################################
echo "############ LEG R — rollback from a corrupt-verity payload ############"
echo

# --- R1: install the bad payload -------------------------------------------
echo "### R1: install the bad 0.2 over virtiofs"
launch r1 "$SHARE_BAD"
if await_login r1 180; then
  login_as_root
  printf 'echo "PRE:$(. /etc/os-release; echo $IMAGE_VERSION):END"\n' > "$FIFO"; sleep 4
  printf 'systemctl start liminaos-update.service; echo "UPDATE_RC:$?:END"\n' > "$FIFO"; sleep 150
  printf 'echo "UNITSTATE:$(systemctl is-failed liminaos-update.service):END"\n' > "$FIFO"; sleep 5
  printf 'journalctl -u liminaos-update.service --no-pager | tail -12\n' > "$FIFO"; sleep 6
  printf 'ls /boot/EFI/Linux/ | tr "\\n" " "; echo ESPLIST_END\n' > "$FIFO"; sleep 5
  printf 'systemctl poweroff\n' > "$FIFO"; sleep 12
else
  echo "!!! R1 never reached a login prompt — the run is void, not a finding"
fi
halt_guest
record_seq "after install"
snapshot "after installing the bad 0.2"

# --- R2..R5: power-cycle and watch the tries burn down ----------------------
# sd-boot renames the entry as it LOADS it, so a boot that dies in the initrd still spends a
# try. Expect roughly +3-0 → +2-1 → +1-2 → +0-3, then the exhausted entry sorts last and the
# survivor boots. The exact number of cycles is not asserted here; the SEQUENCE is recorded
# and the landing slot is asserted, because "tries exhausted ⇒ skipped" is NOT what sd-boot
# does — an exhausted entry is sorted last, and if it is the only entry it is retried forever.
LANDED=""
for cycle in 2 3 4 5 6; do
  echo "### R$cycle: power cycle"
  launch "r$cycle" ""
  if await_login "r$cycle" 150; then
    login_as_root
    printf 'echo "BOOTED:$(. /etc/os-release; echo $IMAGE_VERSION):END"\n' > "$FIFO"; sleep 4
    printf 'echo "USRMNT:$(findmnt -no SOURCE,FSTYPE /usr):END"\n' > "$FIFO"; sleep 4
    printf 'ls /boot/EFI/Linux/ | tr "\\n" " "; echo ESPLIST_END\n' > "$FIFO"; sleep 5
    printf 'systemctl poweroff\n' > "$FIFO"; sleep 12
    halt_guest
    LANDED=$(tr '\r' '\n' < "$WORK/r$cycle.log" | grep -aoE 'BOOTED:[0-9.]+:END' | tail -1)
    record_seq "cycle $cycle (BOOTED)"
    echo "    reached a shell: $LANDED"
    break
  fi
  halt_guest
  record_seq "cycle $cycle (no login)"
  echo "    no login prompt — verity evidence from this boot:"
  verity_markers "r$cycle" | while IFS= read -r l; do echo "      | $l"; done
done

snapshot "after rollback"

########################################################################
echo "############ LEG C — recovery: install a good 0.3 ############"
# Expectation, derived from the transfer definitions BEFORE the run rather than fitted to it:
#   - liminaos-usr.transfer has ProtectVersion=%A and no InstancesMax, so 0.3 must land in the
#     0.2 partition slot; the running 0.1 is protected and must survive.
#   - liminaos-uki.transfer has InstancesMax=2, so one ESP entry must go. Version-sorting alone
#     would drop 0.1 and KEEP THE DEAD 0.2 — which would leave the machine with a known-bad
#     fallback. ProtectVersion=%A is the only thing preventing that, so this leg is a live test
#     of that protection, not just of "did 0.3 install".
#   - Its Target MatchPattern carries all three boot-count name forms (`+@l-@d`, `+@l`, bare),
#     so the renamed `liminaos_0.2+0-3.efi` is matchable and therefore evictable. A pattern
#     missing those wildcards would silently orphan it — checked, and it is not the case here.
echo
SURVIVOR_ESP=$(esp_files)

launch c1 "$SHARE_GOOD3"
if await_login c1 180; then
  login_as_root
  printf 'echo "PRE3:$(. /etc/os-release; echo $IMAGE_VERSION):END"\n' > "$FIFO"; sleep 4
  # ProtectVersion=%A expands from IMAGE_VERSION in os-release, and that is what @v matches
  # against. Read it BEFORE the install: it is the only thing that separates the two ways
  # leg C can fail, and it is unavailable afterwards. If it reads 0.1, a wrong eviction means
  # ProtectVersion was not honoured; if it is empty or names a version no instance claims,
  # ProtectVersion was protecting nothing all along — which is the slot-label near-miss
  # resurfacing on a different surface. Same wrong outcome, different defect.
  # Both files are read. On a hermetic-/usr image /etc/os-release is not a symlink — it is
  # materialised by tmpfiles from /usr/share/factory/etc — so a divergence is not merely a
  # mis-expansion clue: it would mean the factory materialisation did not track the /usr that
  # was installed. Nothing else currently checks that, and factory /etc is write-once per
  # guest, so a stale copy can never be corrected by a later update.
  printf 'echo "PROTECT_USRLIB:$(. /usr/lib/os-release; echo $IMAGE_VERSION):END"\n' > "$FIFO"; sleep 4
  printf 'systemctl start liminaos-update.service; echo "UPDATE3_RC:$?:END"\n' > "$FIFO"; sleep 150
  printf 'echo "UNITSTATE3:$(systemctl is-failed liminaos-update.service):END"\n' > "$FIFO"; sleep 5
  printf 'journalctl -u liminaos-update.service --no-pager | tail -12\n' > "$FIFO"; sleep 6
  printf 'ls /boot/EFI/Linux/ | tr "\\n" " "; echo ESPLIST_END\n' > "$FIFO"; sleep 5
  printf 'systemctl poweroff\n' > "$FIFO"; sleep 12
else
  echo "!!! C1 never reached a login prompt"
fi
halt_guest
record_seq "after installing 0.3"
snapshot "after installing 0.3"

launch c2 ""
if await_login c2 180; then
  login_as_root
  printf 'echo "POST3:$(. /etc/os-release; echo $IMAGE_VERSION):END"\n' > "$FIFO"; sleep 4
  printf 'echo "USRMNT3:$(findmnt -no SOURCE,FSTYPE /usr):END"\n' > "$FIFO"; sleep 4
  printf 'for w in $(cat /proc/cmdline); do case $w in usrhash=*) echo "CMDLINE3_$w:END";; esac; done\n' > "$FIFO"; sleep 4
  printf 'echo "FAILEDUNITS3:$(systemctl --failed --no-legend | wc -l):END"\n' > "$FIFO"; sleep 4
  printf 'ls /boot/EFI/Linux/ | tr "\\n" " "; echo ESPLIST_END\n' > "$FIFO"; sleep 5
  printf 'systemctl poweroff\n' > "$FIFO"; sleep 12
else
  echo "!!! C2 never reached a login prompt"
fi
halt_guest
record_seq "after booting 0.3"

########################################################################
echo
echo "=========================== VERDICT ==========================="
p(){ tr '\r' '\n' < "$1" 2>/dev/null | grep -aoE "$2" | tail -1; }

echo "--- leg R: rollback ---"
echo "version before          : $(p "$WORK/r1.log" 'PRE:[0-9.]+:END')"
echo "bad update installed    : $(p "$WORK/r1.log" 'UPDATE_RC:[0-9]+:END')   <-- expect 0: the transport has no integrity check"
echo "update unit state       : $(p "$WORK/r1.log" 'UNITSTATE:[a-z-]+:END')"
echo "landed on               : ${LANDED:-NEVER REACHED A SHELL}"
echo
echo "(a) ESP filename sequence, host-side, guest powered off:"
printf '%s' "$SEQ" | while IFS= read -r l; do echo "      $l"; done
echo
echo "(b) verity evidence per failed boot:"
for t in r2 r3 r4 r5 r6; do
  [ -f "$WORK/$t.log" ] || continue
  tr '\r' '\n' < "$WORK/$t.log" | grep -qa 'login:' && continue
  echo "    $t:"; verity_markers "$t" | while IFS= read -r l; do echo "      | $l"; done
done
echo
echo "--- leg C: recovery ---"
echo "version at recovery start: $(p "$WORK/c1.log" 'PRE3:[0-9.]+:END')"
echo "ProtectVersion source    : $(p "$WORK/c1.log" 'PROTECT_USRLIB:[0-9.]*:END')   <-- what %A expands from"
echo "0.3 install exit         : $(p "$WORK/c1.log" 'UPDATE3_RC:[0-9]+:END')"
echo "0.3 unit state           : $(p "$WORK/c1.log" 'UNITSTATE3:[a-z-]+:END')"
echo "version after reboot     : $(p "$WORK/c2.log" 'POST3:[0-9.]+:END')"
echo "/usr mount               : $(p "$WORK/c2.log" 'USRMNT3:.*:END')"
echo "usrhash booted           : $(p "$WORK/c2.log" 'CMDLINE3_usrhash=[0-9a-f]+:END')"
echo "failed units             : $(p "$WORK/c2.log" 'FAILEDUNITS3:[0-9]+:END')"
echo
echo "ESP before recovery      : $SURVIVOR_ESP"
echo "ESP after recovery       : $(esp_files)"
echo "   expect: 0.3 present, the survivor still present, and the exhausted +0-3 entry gone"
echo
# ProtectVersion=%A rides on THREE transfers in this one run, and the blast radii differ:
# on the ESP a failure leaves a bad FALLBACK; on liminaos-usr / liminaos-usr-verity it means
# installing over the mounted, running /usr. If leg C goes wrong, establish which — one of
# those is a test result and the other is a live system being overwritten.
slots_now=$(python3 "$VERIFY" "$WORK/disk.raw" 2>/dev/null | grep -aoE 'liminaos_usr_[0-9.]+' | sort -u | tr '\n' ' ')
echo "usr slots at the end     : $slots_now"
case "$slots_now" in
  *liminaos_usr_0.1*) echo "   running /usr slot 0.1 SURVIVED  <-- partition-side ProtectVersion held" ;;
  *) echo "   !!! running /usr slot 0.1 IS GONE — the mounted, running /usr was the eviction target" ;;
esac
echo
snapshot "final"
echo "=== leftovers ==="; pgrep -f "limina-vmm .*$(basename "$WORK")" | wc -l
