#!/bin/sh
# Boot the probe initramfs under limina-vmm and print its RESULT/KMSG lines.
# usage: run.sh <limina-vmm> <kernel Image> <initramfs.cpio> <workdir> [extra limina-vmm args]
# e.g. extra args: --nested-virt [--ipa-granule 4k]
set -e
vmm=$1 kernel=$2 initrd=$3 work=$4
shift 4
mkdir -p "$work"
: > "$work/console.log"
"$vmm" --kernel "$kernel" --initramfs "$initrd" --cmdline "console=ttyAMA0 rdinit=/init" \
    --cpus 2 --ram-mib 1024 --console "$work/console.log" --no-snd --no-battery "$@" \
    > "$work/worker.log" 2>&1 || echo "worker exit $?"
grep -a -E 'RESULT|KMSG|kvm' "$work/console.log" || true
