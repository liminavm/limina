#!/bin/bash
# P0 of docs/design/vtpm.md: boot a clone of the stock F44 image under Homebrew QEMU with a
# swtpm TPM 2.0 on tpm-tis-device (the same device-tree node our libkrun device will expose),
# logging every command and response, so the guest-side consumers can be exercised on a
# known-good TPM and their command streams captured.
#
#   spikes/vtpm-p0/run.sh            # boot (backgrounds QEMU), ssh on 127.0.0.1:$SSH_PORT
#   spikes/vtpm-p0/run.sh --fresh    # discard the clone, TPM state and vars, then boot
#
# Needs: brew install qemu swtpm. Runs outside any tool sandbox (HVF).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
work="${VTPM_P0_WORK:-$here/work}"
base="${VTPM_P0_BASE:-$repo/Fedora-Workstation-44.stock.test.raw}"
SSH_PORT="${SSH_PORT:-2240}"
SWTPM_LOG_LEVEL="${SWTPM_LOG_LEVEL:-20}"
# One SHA-256 bank, as the engine allocates (docs/design/vtpm.md). Without swtpm_setup, libtpms
# allocates all four banks and the firmware extends every one of them.
PCR_BANKS="${PCR_BANKS:-sha256}"

if [[ "${1:-}" == "--fresh" ]]; then
    rm -rf "$work"
fi
mkdir -p "$work/tpm"

disk="$work/vtpm-p0.raw"
[[ -e "$disk" ]] || cp -c "$base" "$disk"           # APFS CoW clone; the base stays pristine
vars="$work/vars.fd"
[[ -e "$vars" ]] || truncate -s 64m "$vars"

if [[ ! -e "$work/tpm/tpm2-00.permall" ]]; then
    swtpm_setup --tpm2 --tpmstate "$work/tpm" --pcr-banks "$PCR_BANKS" --overwrite \
        > "$work/swtpm_setup.log" 2>&1
fi

sock="$work/swtpm.sock"
rm -f "$sock"
swtpm socket --tpm2 \
    --tpmstate dir="$work/tpm" \
    --ctrl type=unixio,path="$sock" \
    --log file="$work/swtpm.log",level="$SWTPM_LOG_LEVEL" \
    --flags startup-none \
    --daemon --pid file="$work/swtpm.pid"

qemu-system-aarch64 \
    -M virt,acpi=off -accel hvf -cpu host -smp 4 -m 4096 \
    -drive if=pflash,format=raw,readonly=on,file=/opt/homebrew/share/qemu/edk2-aarch64-code.fd \
    -drive if=pflash,format=raw,file="$vars" \
    -drive file="$disk",format=raw,if=virtio \
    -netdev user,id=n0,hostfwd=tcp:127.0.0.1:"$SSH_PORT"-:22 -device virtio-net-pci,netdev=n0 \
    -chardev socket,id=chrtpm,path="$sock" \
    -tpmdev emulator,id=tpm0,chardev=chrtpm -device tpm-tis-device,tpmdev=tpm0 \
    -display none -serial file:"$work/console.log" \
    -daemonize -pidfile "$work/qemu.pid"

echo "qemu pid $(cat "$work/qemu.pid"), swtpm pid $(cat "$work/swtpm.pid")"
echo "console: $work/console.log   tpm log: $work/swtpm.log"
echo "ssh: ssh -p $SSH_PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1"
