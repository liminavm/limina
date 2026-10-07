#!/usr/bin/env python3
"""Echo servers for run-libkrun-baseline.sh.

fifo <out> <in>: copy what the guest writes to hvc0 (out FIFO) back to it (in FIFO), verbatim.
unix <path>:     listen on a UNIX socket and echo each connection (libkrun's vsock port map).
"""
import os
import socket
import sys
import threading

if sys.argv[1] == "fifo":
    # open the input side first, read-write, so neither open blocks on the other end
    w = os.open(sys.argv[3], os.O_RDWR)
    r = os.open(sys.argv[2], os.O_RDONLY)
    while True:
        b = os.read(r, 65536)
        if not b:
            break
        os.write(w, b)
else:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(sys.argv[2])
    s.listen(4)

    def serve(c):
        while True:
            b = c.recv(65536)
            if not b:
                return
            c.sendall(b)

    while True:
        c, _ = s.accept()
        threading.Thread(target=serve, args=(c,), daemon=True).start()
