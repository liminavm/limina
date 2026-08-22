#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Wait for a limina guest's SSH to become reachable, and print the forwarded port.
#
#   scripts/wait-guest-ssh.sh WORKER_LOG [TIMEOUT_SECS]
#
# WORKER_LOG is the log the boot vehicle points the supervisor at (the
# `limina pid=… (worker log /tmp/limina-worker-….log, …)` line names it) — that is
# where the supervisor prints `guest SSH forward ready: ssh -p N <user>@127.0.0.1`,
# NOT the boot script's own stdout. This script waits for that line, then keeps
# probing the port until sshd actually answers with an SSH banner (gvproxy listens
# on the forward immediately, long before it can dial the guest), and only then
# prints the port on stdout. Nonzero exit + diagnostics on stderr on timeout.
#
# Typical use:
#   port=$(scripts/wait-guest-ssh.sh /tmp/limina-worker-<disk>.log 240)
#   ssh -p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1 …
set -euo pipefail

log="${1:?usage: wait-guest-ssh.sh WORKER_LOG [TIMEOUT_SECS]}"
timeout="${2:-240}"
deadline=$(( $(date +%s) + timeout ))

port=""
while [ -z "$port" ]; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "wait-guest-ssh: no 'guest SSH forward ready' in $log after ${timeout}s; log tail:" >&2
        tail -5 "$log" >&2 2>/dev/null || echo "  (log unreadable)" >&2
        exit 1
    fi
    port=$(sed -n 's/.*guest SSH forward ready: ssh -p \([0-9][0-9]*\).*/\1/p' "$log" 2>/dev/null | tail -1)
    [ -n "$port" ] || sleep 2
done

while :; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "wait-guest-ssh: port $port never answered with an SSH banner within ${timeout}s" >&2
        exit 1
    fi
    banner=$(nc -w 2 127.0.0.1 "$port" </dev/null 2>/dev/null | head -c 8 || true)
    case "$banner" in
        SSH-*) break ;;
    esac
    sleep 2
done

echo "$port"
