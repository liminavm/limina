#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Host side of the nested-virt checks, run from a limina checkout on an M3+ Mac. Boots (or
# resumes) a flat headless VM on a clone, waits for ssh, and prints the port.
#
#   host-run.sh boot <disk> <log> [extra limina flags...]
#
# The ssh port is pinned (PORT, default 2391) so a peer VM cannot be mistaken for this one, and
# the same command resumes a suspended run: a flat run resumes from the snapshot beside its disk.
set -euo pipefail
cmd=$1 disk=$2 log=$3
shift 3
port=${PORT:-2391}
case $cmd in
boot)
    out=$(target/debug/limina --firmware target/krun-efi/KRUN_EFI.gop.fd --disk "$disk" --net \
        --ssh-port "$port" --cpus 4 --ram-mib 6144 --detach --log "$log" "$@" </dev/null)
    echo "$out"
    pid=$(echo "$out" | sed -n 's/.*detached pid=\([0-9]*\).*/\1/p')
    scripts/wait-guest-ssh.sh "$log" 300 "$pid" </dev/null
    ;;
*)
    echo "usage: $0 boot <disk> <log> [limina flags...]" >&2
    exit 2
    ;;
esac
