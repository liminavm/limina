#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
#
# Test scripts/wait-guest-ssh.sh's stages against fakes — no VM required.
#
# CLAUDE.md names that script THE way to wait for a networked boot, and for a long time it
# stopped at the SSH banner. A banner is not readiness: sshd sends it the moment it listens,
# while pam_nologin refuses every login until systemd-user-sessions.service removes
# /run/nologin (~1 s on three boots in eight — spikes/ssh-staging-race/). Callers ssh the
# instant this script returns, so a banner-only wait hands them that gap.
#
# The fake below is a listener that greets and hangs up — a guest in exactly that state.
# Run it directly: scripts/tests/wait-guest-ssh-stages.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WAIT="$REPO/scripts/wait-guest-ssh.sh"
TMP="$(mktemp -d)"
FAKE_PID=""
cleanup() {
    [ -n "$FAKE_PID" ] && kill "$FAKE_PID" 2>/dev/null
    rm -rf "$TMP"
}
trap cleanup EXIT

pass=0
fail=0

# check <name> <expected-exit> <actual-exit> <output-file> [needle]
check() {
    local name="$1" want="$2" got="$3" out="$4" needle="${5:-}"
    if [ "$got" -ne "$want" ]; then
        printf 'FAIL %-42s expected exit %d, got %d\n' "$name" "$want" "$got"
        sed 's/^/       | /' "$out"
        fail=$((fail + 1))
        return
    fi
    if [ -n "$needle" ] && ! grep -q "$needle" "$out"; then
        printf 'FAIL %-42s exit %d as expected, but %s is missing\n' "$name" "$got" "$needle"
        sed 's/^/       | /' "$out"
        fail=$((fail + 1))
        return
    fi
    printf 'ok   %-42s (exit %d)\n' "$name" "$got"
    pass=$((pass + 1))
}

free_port() {
    python3 -c 'import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

# --- a log that never announces a forward ------------------------------------------------
: > "$TMP/empty.log"
"$WAIT" "$TMP/empty.log" 3 > "$TMP/o1" 2>&1
check "no forward line in the log" 1 $? "$TMP/o1" "guest SSH forward ready"

# --- the forward is announced but nothing ever listens -----------------------------------
closed="$(free_port)"
echo "guest SSH forward ready: ssh -p $closed claude@127.0.0.1" > "$TMP/closed.log"
"$WAIT" "$TMP/closed.log" 4 > "$TMP/o2" 2>&1
check "port never answers a banner" 1 $? "$TMP/o2" "never answered with an SSH banner"

# --- a banner with nothing behind it: THE case this exists for ---------------------------
python3 -c '
import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", 0)); s.listen(8)
sys.stdout.write(f"{s.getsockname()[1]}\n"); sys.stdout.flush()
while True:
    try:
        c, _ = s.accept()
        c.sendall(b"SSH-2.0-OpenSSH_10.2\r\n")
        c.close()
    except OSError:
        break
' > "$TMP/port" &
FAKE_PID=$!
# Off the job table: otherwise the shell reports the kill in cleanup() as `Terminated`,
# under the test output, which reads like a failure.
disown "$FAKE_PID" 2>/dev/null || true
for _ in $(seq 1 50); do
    [ -s "$TMP/port" ] && break
    sleep 0.1
done
greeter="$(cat "$TMP/port" 2>/dev/null)"
[ -n "$greeter" ] || { echo "the fake sshd never reported its port" >&2; exit 2; }
echo "guest SSH forward ready: ssh -p $greeter claude@127.0.0.1" > "$TMP/greet.log"

"$WAIT" "$TMP/greet.log" 6 > "$TMP/o3" 2>&1
check "a banner alone is not readiness" 1 $? "$TMP/o3" "no session could be"

# ...and the documented opt-out still stops at the banner, for a guest we have no login on.
WAIT_SSH_USER= "$WAIT" "$TMP/greet.log" 6 > "$TMP/o4" 2>&1
check "WAIT_SSH_USER= stops at the banner" 0 $? "$TMP/o4" "$greeter"

echo
echo "wait-guest-ssh stage tests: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
