#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Make the vTPM L2 image: a CoW clone of the stock test image with the TPM consumers installed.
#
#   scripts/provision/make-tpm-test-image.sh            # -> Fedora-Workstation-44.tpm.test.raw
#   scripts/provision/make-tpm-test-image.sh <out.raw>
#
# The consumers `crates/limina-test/tests/l2_tpm.rs` runs are the ones P0 measured
# (spikes/vtpm-p0/consumers): systemd's own tools, which a stock Workstation ships, and these,
# which it does not — tpm2-pkcs11 (+ tools, opensc's pkcs11-tool), clevis, the tpm2 OpenSSL
# provider and the openssl command, and ssh-tpm-agent, which Fedora does not package (its
# release, checksum-pinned).
# Installing them over the network on every run would make the test slow and flaky, so they
# live in the image. Nothing else changes: no tss group membership (the test adds it in its own
# clone), no TPM state (the image has never met a TPM), so the image stays a stock guest with
# packages a user could install.
#
# Boots headless through EFI with the guest's own kernel, waits with scripts/wait-guest-ssh.sh,
# installs, verifies every package and binary, and powers off. Refuses to overwrite <out>.
#
# Env: LIMINA_TEST_DISK (the source, default Fedora-Workstation-44.stock.test.raw),
# LIMINA_FIRMWARE (default target/krun-efi/KRUN_EFI.gop.fd), LIMINA_SSH_TIMEOUT (default 420),
# LIMINA_LOGDIR (default /tmp). Needs `cargo xtask build` first.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

src="${LIMINA_TEST_DISK:-Fedora-Workstation-44.stock.test.raw}"
out="${1:-Fedora-Workstation-44.tpm.test.raw}"
firmware="${LIMINA_FIRMWARE:-target/krun-efi/KRUN_EFI.gop.fd}"
logdir="${LIMINA_LOGDIR:-/tmp}"
ssh_timeout="${LIMINA_SSH_TIMEOUT:-420}"
ssh_opts=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR
          -o BatchMode=yes)

for f in "$src" "$firmware" target/debug/limina; do
  [ -e "$f" ] || { echo "missing: $f" >&2; exit 2; }
done
[ ! -e "$out" ] || { echo "$out exists; remove it first" >&2; exit 2; }

cp -c "$src" "$out"
name="$(basename "${out%.raw}")"
log="$logdir/limina-$name.log"
target/debug/limina --cpus 4 --ram-mib 4096 --firmware "$firmware" --disk "$out" --net \
  --console "$logdir/limina-$name.console.log" >"$log" 2>&1 &
boot=$!
port="$(scripts/wait-guest-ssh.sh "$log" "$ssh_timeout" "$boot")" || true
if [ -z "$port" ]; then
  echo "!!! the guest never accepted ssh (see $log); $out is left half-made" >&2
  kill "$boot" 2>/dev/null || true
  wait "$boot" 2>/dev/null || true
  exit 1
fi
echo "guest up: ssh -p $port claude@127.0.0.1 (log $log)"

ssh -p "$port" "${ssh_opts[@]}" claude@127.0.0.1 bash -s <<'GUEST'
set -euxo pipefail
sudo dnf install -y -q tpm2-pkcs11 tpm2-pkcs11-tools clevis clevis-luks tpm2-openssl opensc openssl
d="$(mktemp -d)"
cd "$d"
curl -sSLO https://github.com/Foxboron/ssh-tpm-agent/releases/download/v0.9.0/ssh-tpm-agent-v0.9.0-linux-arm64.tar.gz
echo "50701ccfd2dfb990374aea8683fa7972742b2ad9a28bf0f6698c63697e351cf8  ssh-tpm-agent-v0.9.0-linux-arm64.tar.gz" | sha256sum -c
tar xzf ssh-tpm-agent-v0.9.0-linux-arm64.tar.gz
sudo install -m755 ssh-tpm-agent/ssh-tpm-add ssh-tpm-agent/ssh-tpm-agent \
  ssh-tpm-agent/ssh-tpm-hostkeys ssh-tpm-agent/ssh-tpm-keygen /usr/local/bin/
cd /
rm -rf "$d"
rpm -q tpm2-pkcs11 tpm2-pkcs11-tools clevis clevis-luks tpm2-openssl opensc openssl tpm2-tools
for b in ssh-tpm-agent ssh-tpm-keygen ssh-tpm-add tpm2_ptool pkcs11-tool clevis openssl; do command -v "$b"; done
test -e /usr/lib64/pkcs11/libtpm2_pkcs11.so
sudo dnf clean all -q
GUEST

ssh -p "$port" "${ssh_opts[@]}" claude@127.0.0.1 'sudo systemd-run --on-active=1 systemctl poweroff' || true
wait "$boot" || true
echo "made $out"
