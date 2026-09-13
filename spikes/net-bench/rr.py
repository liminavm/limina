#!/usr/bin/env python3
"""TCP request/response latency through the NAT path: the cost side of interrupt coalescing.

Host:  rr.py serve [port]            -- echo server on 127.0.0.1 (default 5202)
Guest: rr.py client <host> [port] [n] -- n one-byte round trips (default 10000), prints p50/p90/p99

The guest reaches the host's loopback as 192.168.127.254 through gvproxy. The reply leg is the
host -> guest RX path, the one a coalescing device would delay.
"""
import socket
import sys
import time


def serve(port):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    s.listen(4)
    while True:
        c, _ = s.accept()
        c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        while True:
            b = c.recv(1)
            if not b:
                break
            c.sendall(b)
        c.close()


def client(host, port, n):
    c = socket.create_connection((host, port))
    c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    for _ in range(200):  # warm the path
        c.sendall(b"x")
        c.recv(1)
    rtts = []
    for _ in range(n):
        t = time.perf_counter_ns()
        c.sendall(b"x")
        c.recv(1)
        rtts.append(time.perf_counter_ns() - t)
    c.close()
    rtts.sort()
    pct = lambda p: rtts[min(len(rtts) - 1, int(len(rtts) * p))] / 1000
    print("rr %d round trips: p50 %.0f us, p90 %.0f us, p99 %.0f us, max %.0f us" % (
        n, pct(0.50), pct(0.90), pct(0.99), rtts[-1] / 1000))


if __name__ == "__main__":
    if sys.argv[1] == "serve":
        serve(int(sys.argv[2]) if len(sys.argv) > 2 else 5202)
    else:
        client(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 5202,
               int(sys.argv[4]) if len(sys.argv) > 4 else 10000)
