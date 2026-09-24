#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""sshprobe — what does ssh actually SAY when it fails, at each LogLevel?

The harness reports a failed `ssh_exec` by quoting the child's stderr. One failure in the
2026-09-23 suite quoted nothing at all: exit 255, empty stderr. Rather than guess which
failure mode prints nothing, drive ssh against listeners that fail in each way on purpose.

The answer (table in RESULTS.md §1): `LogLevel=ERROR` prints "Connection refused" and
"Permission denied", but says NOTHING when the connection is accepted and then dropped
before the key exchange completes -- which is precisely the shape gvproxy produces when
its forward is up but the guest's sshd will not take the session. `INFO` prints it.

Run: spikes/ssh-staging-race/sshprobe.py
"""

import socket
import subprocess
import threading
import time

# Fixed loopback ports in the private range; each round uses a fresh one so a lingering
# TIME_WAIT from the previous round cannot colour the next.
BASE = 59900
OPTS = ["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
        "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]


def serve(mode, port):
    """Accept one connection and fail in the requested way."""
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    s.listen(5)
    c, _ = s.accept()
    if mode == "banner_then_close":
        c.sendall(b"SSH-2.0-OpenSSH_10.2\r\n")
        time.sleep(0.2)
    c.close()
    s.close()


def run(mode, port, level):
    if mode != "refused":
        threading.Thread(target=serve, args=(mode, port), daemon=True).start()
        time.sleep(0.3)
    p = subprocess.run(["ssh", "-p", str(port)] + OPTS + ["-o", "LogLevel=" + level,
                                                          "claude@127.0.0.1", "true"],
                       capture_output=True, timeout=30)
    err = p.stderr.decode(errors="replace").strip() or "(empty)"
    print(f"{mode:<18} LogLevel={level:<7} exit={p.returncode}  {err}")


def main():
    port = BASE
    for level in ("ERROR", "INFO"):
        for mode in ("refused", "accept_then_close", "banner_then_close"):
            port += 1
            run(mode, port, level)


if __name__ == "__main__":
    main()
