#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Deliver a guest-tools payload to enhanced-tier images, one after another, with the REAL
# installer — the step CLAUDE.md mandates after any guest component changes ("refresh the
# deliverables AND the enhanced images").
#
#   scripts/provision/deliver-payload.sh payload/limina-guest-tools-f44-r15.tar.zst \
#       Fedora-Workstation-44.enhanced.test.raw Fedora-Workstation-44.enhanced.raw ...
#
# Per image, in place:
#   1. refuse if a live limina-vmm has the image open (other sessions run VMs on this host);
#   2. CoW backup `<image>.bak-pre-<rev>.raw` (`<rev>` from the payload's `-rNN` suffix; skipped
#      when it already exists; LIMINA_DELIVER_BACKUP=0 skips it);
#   3. boot the image through the default vehicle (EFI + venus, `boot-enhanced-efi-kk.sh`) and wait
#      for sshd with `scripts/wait-guest-ssh.sh` — the one readiness oracle;
#   4. scp the tarball into the guest's home, extract, run `install-enhanced.sh` (its log lands in
#      LIMINA_DELIVER_LOGDIR, default /tmp);
#   5. verify the installed `limina-agent` / `limina-agent-session` are byte-for-byte the payload's
#      (sha256 on both ends) and print the installer's kernel-default lines — when the permanent
#      default is not the payload's 16k kernel, the image still OWES a trial boot (an EFI boot to
#      the desktop auto-promotes it; see docs/images.md);
#   6. clean poweroff, wait for the supervisor to exit, next image.
#
# Env: LIMINA_GUEST_USER (default claude — the images' autologin user with NOPASSWD sudo),
# LIMINA_DELIVER_SSH_TIMEOUT (seconds to wait for sshd, default 420), plus everything the boot
# vehicle honours (LIMINA_CPUS, LIMINA_RAM_MIB, ...).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

PAYLOAD="${1:?usage: deliver-payload.sh <payload.tar.zst> <image.raw>...}"
shift
[ "$#" -ge 1 ] || { echo "usage: deliver-payload.sh <payload.tar.zst> <image.raw>..." >&2; exit 2; }
[ -f "$PAYLOAD" ] || { echo "no such payload: $PAYLOAD" >&2; exit 2; }
for img in "$@"; do [ -f "$img" ] || { echo "no such image: $img" >&2; exit 2; }; done

USER_="${LIMINA_GUEST_USER:-claude}"
LOGDIR="${LIMINA_DELIVER_LOGDIR:-/tmp}"
SSH_TIMEOUT="${LIMINA_DELIVER_SSH_TIMEOUT:-420}"
TARBALL="$(basename "$PAYLOAD")"
# `limina-guest-tools-f44-r15.tar.zst` -> `r15`; anything else -> the tarball's stem.
REV="$(printf '%s' "$TARBALL" | sed -nE 's/.*-(r[0-9]+)\.tar\.zst$/\1/p')"
[ -n "$REV" ] || REV="${TARBALL%.tar.zst}"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

# The payload's own agent binaries, hashed on the host: the verification reference.
payload_sha() { tar --zstd -xOf "$PAYLOAD" "limina-guest-tools/$1" | shasum -a 256 | cut -d' ' -f1; }
WANT_AGENT="$(payload_sha limina-agent)"
WANT_SESSION="$(payload_sha limina-agent-session)"
echo "payload $TARBALL ($REV): limina-agent ${WANT_AGENT:0:12}… limina-agent-session ${WANT_SESSION:0:12}…"

deliver() {
  local img="$1" base name log boot port ilog
  base="${img%.raw}"; name="$(basename "$base")"
  log="/tmp/limina-worker-$name.log"
  ilog="$LOGDIR/limina-deliver-$name-$REV.install.log"
  echo "=== $img  $(date '+%F %T')"

  # Never touch an image a running VM has open — match on the disk path, which is unique per run.
  if pgrep -f "limina-vmm.*$(basename "$img")" >/dev/null; then
    echo "!!! a live limina-vmm has $img open — refusing (ps: $(pgrep -f "limina-vmm.*$(basename "$img")" | tr '\n' ' '))" >&2
    return 1
  fi

  if [ "${LIMINA_DELIVER_BACKUP:-1}" != "0" ]; then
    if [ -e "$base.bak-pre-$REV.raw" ]; then
      echo "backup $base.bak-pre-$REV.raw already exists; keeping it"
    else
      cp -c "$img" "$base.bak-pre-$REV.raw"
      echo "backup: $base.bak-pre-$REV.raw"
    fi
  fi

  LIMINA_DISK="$img" LIMINA_BOOT_LOG="$log" spikes/venus-draw-probe/boot-enhanced-efi-kk.sh \
    >"$LOGDIR/limina-deliver-$name-$REV.boot.log" 2>&1 &
  boot=$!
  port="$(scripts/wait-guest-ssh.sh "$log" "$SSH_TIMEOUT")"
  echo "guest up: ssh -p $port $USER_@127.0.0.1 (worker log $log)"

  scp -P "$port" "${SSH_OPTS[@]}" "$PAYLOAD" "$USER_@127.0.0.1:"
  if ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" \
      "set -e; rm -rf limina-guest-tools; tar --zstd -xf '$TARBALL'; sudo ./limina-guest-tools/install-enhanced.sh \"\$HOME/limina-guest-tools\"" \
      >"$ilog" 2>&1; then
    echo "installer ok (log $ilog)"
  else
    echo "!!! installer FAILED (log $ilog); tail:" >&2; tail -15 "$ilog" >&2
    ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" 'sudo systemctl poweroff' || true
    wait "$boot" || true
    return 1
  fi
  grep -E "permanent default|one-shot next boot|kernel .* already installed|installing .*kernel" "$ilog" | sed 's/^/   /' || true

  local got_agent got_session
  got_agent="$(ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" 'sha256sum /usr/local/bin/limina-agent 2>/dev/null | cut -d" " -f1' || true)"
  got_session="$(ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" 'sha256sum /usr/local/bin/limina-agent-session 2>/dev/null | cut -d" " -f1' || true)"
  local ok=1
  [ "$got_agent" = "$WANT_AGENT" ] || { echo "!!! /usr/local/bin/limina-agent is ${got_agent:-absent}, payload has $WANT_AGENT" >&2; ok=0; }
  [ "$got_session" = "$WANT_SESSION" ] || { echo "!!! /usr/local/bin/limina-agent-session is ${got_session:-absent}, payload has $WANT_SESSION" >&2; ok=0; }
  [ "$ok" = 1 ] && echo "verified: both agent binaries match the payload"

  ssh -p "$port" "${SSH_OPTS[@]}" "$USER_@127.0.0.1" "rm -rf limina-guest-tools '$TARBALL'; sudo systemctl poweroff" || true
  wait "$boot" || true
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
echo "delivered $TARBALL to: $*"
