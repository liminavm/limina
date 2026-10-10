#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Wait for a limina guest's SSH to become reachable, and print the forwarded port.
#
#   scripts/wait-guest-ssh.sh WORKER_LOG [TIMEOUT_SECS] [BOOT_PID]
#
# WORKER_LOG is the log the boot vehicle points the supervisor at (the
# `limina pid=… (worker log /tmp/limina-worker-….log, …)` line names it) — that is
# where the supervisor prints `guest SSH forward ready: ssh -p N <user>@127.0.0.1`,
# NOT the boot script's own stdout. This script waits for the port (from that line, or
# from the running supervisor when BOOT_PID is given — see below), then for an SSH
# banner (gvproxy listens on the forward immediately, long before it can dial the
# guest), and THEN for a login that actually succeeds. Only then does it print the
# port on stdout. Nonzero exit + diagnostics on stderr on timeout.
#
# The last stage is not belt-and-braces. sshd sends its banner the moment it listens,
# but `pam_nologin` refuses every unprivileged login until systemd-user-sessions.service
# removes /run/nologin, and nothing orders sshd after that: a login in the gap is told
# "System is booting up. Unprivileged users are not permitted to log in yet." and ssh
# exits 255. Measured on the F44 test image (spikes/ssh-staging-race/): the banner lands
# at ~3.2 s and the gap is 0 s on five boots in eight, ~1.0-1.14 s on the other three.
# Callers ssh the instant this returns, so returning on the banner hands them that gap —
# which is how one suite lost a test to an ssh 255 with an empty stderr.
#
# WAIT_SSH_USER names the login to test with (default `claude`, the test images' user —
# docs/images.md §SSH access). Set it EMPTY to stop after the banner, for a guest this
# host has no credentials on; you then get the old, weaker guarantee.
#
# BOOT_PID is optional and is the boot vehicle's pid, or the supervisor's. Pass it
# whenever you have it, for two reasons:
#  - The port then comes from the running supervisor itself (`limina ssh-port <pid>`,
#    asked of BOOT_PID and of its direct children, which is where the boot scripts run
#    it), not from the log. A log that outlived an earlier run still names that run's
#    port, and some other VM may be answering there now. Without a pid the last forward
#    line in the log wins, so empty the log before the boot, as the callers do.
#    The `limina` asked is LIMINA_BIN (relative to the repo root, as the boot scripts read
#    it), default target/debug/limina; if that is not executable, the log is used.
#  - A vehicle that dies before the port is known (a missing worker binary, a bad disk
#    path) is otherwise indistinguishable from a slow boot, and the wait burns the full
#    timeout for a port that can never arrive. With the pid we notice in seconds.
#
# Typical use:
#   port=$(scripts/wait-guest-ssh.sh /tmp/limina-worker-<disk>.log 240)
#   ssh -p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null claude@127.0.0.1 …
set -euo pipefail

log="${1:?usage: wait-guest-ssh.sh WORKER_LOG [TIMEOUT_SECS] [BOOT_PID]}"
timeout="${2:-240}"
boot_pid="${3:-}"
deadline=$(( $(date +%s) + timeout ))

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
limina="${LIMINA_BIN:-target/debug/limina}"
case "$limina" in /*) ;; *) limina="$repo/$limina" ;; esac
ask_supervisor=""
if [ -n "$boot_pid" ]; then
    if [ -x "$limina" ]; then
        ask_supervisor=1
    else
        echo "wait-guest-ssh: no limina at $limina to ask for the port; reading $log" >&2
    fi
fi

# The forward port of the run under BOOT_PID: the first of BOOT_PID and its children that
# answers `limina ssh-port` as a supervisor. Nothing while the supervisor is not up yet.
supervisor_port() {
    local pid p
    for pid in "$boot_pid" $(pgrep -P "$boot_pid" 2>/dev/null || true); do
        p=$("$limina" ssh-port "$pid" 2>/dev/null || true)
        case "$p" in
            '' | *[!0-9]*) ;;
            *) echo "$p"; return 0 ;;
        esac
    done
    return 0
}

port=""
while [ -z "$port" ]; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
        if [ -n "$ask_supervisor" ]; then
            echo "wait-guest-ssh: no supervisor under pid $boot_pid answered \`limina ssh-port\`" \
                 "within ${timeout}s; log tail:" >&2
        else
            echo "wait-guest-ssh: no 'guest SSH forward ready' in $log after ${timeout}s; log tail:" >&2
        fi
        tail -5 "$log" >&2 2>/dev/null || echo "  (log unreadable)" >&2
        exit 1
    fi
    if [ -n "$ask_supervisor" ]; then
        port=$(supervisor_port)
    else
        # The log may not exist yet: a caller that launches the boot and waits in the
        # same breath (deliver-payload.sh) gets here before the vehicle creates it. Left
        # bare, sed's failure rides pipefail into set -e and kills this script SILENTLY --
        # the caller then reads an empty port, and every ssh after it fails with
        # `Bad port ''`. Absorb the failure and keep waiting for the file to appear.
        port=$({ sed -n 's/.*guest SSH forward ready: ssh -p \([0-9][0-9]*\).*/\1/p' "$log" 2>/dev/null || true; } | tail -1)
    fi
    if [ -z "$port" ] && [ -n "$boot_pid" ] && ! kill -0 "$boot_pid" 2>/dev/null; then
        echo "wait-guest-ssh: boot vehicle (pid $boot_pid) exited before announcing SSH; log tail:" >&2
        tail -5 "$log" >&2 2>/dev/null || echo "  (log unreadable)" >&2
        exit 1
    fi
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

user="${WAIT_SSH_USER-claude}"
last="(never ran)"
while [ -n "$user" ]; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "wait-guest-ssh: port $port answered an SSH banner but no session could be" >&2
        echo "  established as $user within ${timeout}s. Last ssh error:" >&2
        echo "$last" | sed 's/^/    /' >&2
        exit 1
    fi
    # LogLevel=INFO on purpose: at ERROR, a connection that is accepted and then dropped
    # mid-handshake exits 255 with NOTHING on stderr, and the guest's own refusal text
    # (the pam_nologin line) is INFO too -- so ERROR hides exactly the diagnosis.
    if last=$(ssh -p "$port" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
                  -o BatchMode=yes -o ConnectTimeout=5 -o LogLevel=INFO \
                  "$user@127.0.0.1" true 2>&1); then
        break
    fi
    sleep 1
done

echo "$port"
