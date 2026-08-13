#!/usr/bin/env python3
"""Drive settle sweeps on a live venus guest and report the stats deltas."""
import socket
import sys
import time

SOCK = "/tmp/sweepsmoke-balloon.sock"


def query(cmd, read_reply=False):
    s = socket.socket(socket.AF_UNIX)
    s.connect(SOCK)
    s.sendall((cmd + "\n").encode())
    reply = None
    if read_reply:
        f = s.makefile()
        reply = f.readline().strip()
    s.close()
    return reply


def stats():
    line = query("stats", read_reply=True)
    return dict(tok.split("=", 1) for tok in line.split() if "=" in tok)


rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 3
s0 = stats()
print(f"before: sweeps={s0.get('sweeps')} sweep_faults={s0.get('sweep_faults')}")
for n in range(rounds):
    base = int(stats()["sweeps"])
    query("settle")
    deadline = time.time() + 30
    while time.time() < deadline:
        time.sleep(1)
        s = stats()
        if int(s["sweeps"]) > base:
            break
    else:
        print(f"round {n + 1}: sweep never completed (sweeps still {base})")
        sys.exit(1)
    print(
        f"round {n + 1}: sweeps={s['sweeps']} debited={int(s['sweep_debited']) >> 20} MiB "
        f"in {s['sweep_ms']} ms, sweep_faults={s['sweep_faults']}"
    )
    time.sleep(5)
print("smoke OK")
