#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""The in-guest oracle for `limina input`: grab the three virtio input nodes (found by name),
print every event as `<dev> <type> <code> <value>`, and stop on the end marker (KEY_F14
released). Writes /tmp/limina-inject-ready once the nodes are grabbed. Argument: seconds to
wait for the marker."""

import fcntl, os, select, struct, sys, time
NAMES = {"kbd": "limina Virtual Keyboard", "ptr": "limina Virtual Pointer",
         "rel": "limina Virtual Mouse"}
nodes = {}
for block in open("/proc/bus/input/devices").read().split("\n\n"):
    name = ev = None
    for line in block.splitlines():
        if line.startswith("N: Name="):
            name = line[len("N: Name="):].strip().strip('"')
        if line.startswith("H: Handlers="):
            ev = next((h for h in line.split("=", 1)[1].split() if h.startswith("event")), None)
    for k, v in NAMES.items():
        if name == v and ev:
            nodes[k] = "/dev/input/" + ev
assert len(nodes) == 3, nodes
fds = {}
for k, p in nodes.items():
    fd = os.open(p, os.O_RDONLY | os.O_NONBLOCK)
    fcntl.ioctl(fd, 0x40044590, 1)  # EVIOCGRAB
    fds[fd] = k
print("NODES", nodes, flush=True)
open("/tmp/limina-inject-ready", "w").write("ready")
deadline = time.time() + float(sys.argv[1])
done = False
while time.time() < deadline and not done:
    ready, _, _ = select.select(list(fds), [], [], 0.5)
    for fd in ready:
        while True:
            try:
                data = os.read(fd, 24 * 64)
            except BlockingIOError:
                break
            if not data:
                break
            for i in range(0, len(data), 24):
                _, _, t, c, v = struct.unpack("qqHHi", data[i:i + 24])
                print(fds[fd], t, c, v, flush=True)
                if fds[fd] == "kbd" and t == 1 and c == 184 and v == 0:
                    done = True
print("END" if done else "TIMEOUT", flush=True)
