#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Boot the stock test image over and over until one boot never reaches sshd, then leave that VM
# running and sample its worker, so the stall can be looked at while it is still happening.
#
#   spikes/snd-boot-stall/boot-loop.sh <outdir> [iterations] [extra limina args...]
#
# Each boot runs on an APFS clone of the image, with the snd device's debug log on and the guest
# console captured. A boot that reaches sshd is powered off and its files removed; a stalled one
# keeps everything in <outdir>/stall-<n>/ plus `sample` output of the worker.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:?usage: boot-loop.sh <outdir> [iterations] [extra limina args...]}"
iterations="${2:-40}"
shift 2 || shift $#
mkdir -p "$out"

image="$root/Fedora-Workstation-44.stock.test.raw"
firmware="$root/target/krun-efi/KRUN_EFI.gop.fd"

for n in $(seq 1 "$iterations"); do
    dir="$out/boot-$n"
    mkdir -p "$dir"
    cp -c "$image" "$dir/disk.raw"
    RUST_LOG=warn,limina=info,krun_devices::virtio::snd=debug \
        "$root/target/debug/limina" --firmware "$firmware" --disk "$dir/disk.raw" --net \
        --cpus 4 --ram-mib 4096 --console "$dir/console.log" "$@" \
        >"$dir/limina.log" 2>&1 &
    pid=$!
    started=$(date +%s)
    if port=$("$root/scripts/wait-guest-ssh.sh" "$dir/limina.log" 150 "$pid" 2>"$dir/wait.err"); then
        echo "boot $n: sshd on port $port after $(( $(date +%s) - started ))s"
        kill -INT "$pid" 2>/dev/null || true
        sleep 3
        kill -INT "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        rm -rf "$dir"
        continue
    fi
    echo "boot $n: STALLED (no sshd after 150s); keeping $dir and the VM (supervisor pid $pid)"
    worker=$(sed -n 's/.*VM worker started (pid \([0-9]*\)).*/\1/p' "$dir/limina.log" | head -1)
    if [ -n "$worker" ]; then
        sample "$worker" 5 -file "$dir/worker.sample.txt" >/dev/null 2>&1 || true
        echo "worker pid $worker sampled into $dir/worker.sample.txt"
    fi
    exit 1
done
echo "no stall in $iterations boots"
