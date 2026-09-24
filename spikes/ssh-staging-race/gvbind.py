#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""gvbind — does gvproxy survive losing the race for its SSH-forward port?

Both the test harness and the supervisor pick the forward port with a bind probe they
then CLOSE (`Guest::boot`, `gateway::allocate_ssh_port`), so two VMs starting at once can
agree on the same port. If gvproxy merely warned and carried on, the loser's supervisor
would announce a forward that belongs to somebody else's guest -- and since every test
image shares one login, the loser's test would quietly pass or fail against the WRONG VM.

It does not: gvproxy exits with "cannot add network services: listen tcp …: bind: address
already in use", so the loser has no gateway and no banner at all. See RESULTS.md §2.

Run: spikes/ssh-staging-race/gvbind.py
"""

import os
import signal
import socket
import subprocess
import time

PORT = 57771
SOCK = "/tmp/gvbind-test.sock"
GVPROXY = os.environ.get("LIMINA_GVPROXY_BIN", "/opt/homebrew/bin/gvproxy")


def main():
    for f in (SOCK,):
        try:
            os.unlink(f)
        except FileNotFoundError:
            pass

    hold = socket.socket()
    hold.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    hold.bind(("127.0.0.1", PORT))
    hold.listen(5)
    print(f"a rival holds 127.0.0.1:{PORT}; starting gvproxy on the same port")

    p = subprocess.Popen([GVPROXY, "-listen-vfkit", f"unixgram://{SOCK}",
                          "-ssh-port", str(PORT), "-mtu", "1500"],
                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    time.sleep(3)
    alive = p.poll() is None
    print(f"gvproxy still running after 3s: {alive} (rc={p.poll()})")
    if alive:
        p.send_signal(signal.SIGTERM)
    try:
        out = p.communicate(timeout=5)[0]
    except subprocess.TimeoutExpired:
        p.kill()
        out = p.communicate()[0]
    print("--- gvproxy said ---")
    print(out.strip())

    hold.close()
    try:
        os.unlink(SOCK)
    except FileNotFoundError:
        pass


if __name__ == "__main__":
    main()
