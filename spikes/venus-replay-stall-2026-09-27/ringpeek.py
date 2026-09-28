#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Read a guest venus ring's words from inside the guest, while its process is live.

Usage (in the guest): sudo python3 ringpeek.py <pid> <vn_ring address, hex>

The address is the ring id: mesa sets `ring->id = (uintptr_t)ring`, and the worker names each ring
thread `virglrs-ring-<id>` in decimal. The `vn_ring` struct is ordinary heap, read through
/proc/<pid>/mem; the head/tail/status it points at live in a virtio-gpu blob, a PFN mapping that
/proc/<pid>/mem cannot read, so they are read through the ring's GEM object instead, mapped from a
pidfd_getfd copy of the process's render-node fd. The tail read that way must equal the guest's own
`ring->cur`, which is the check that the right fd and object were found. Offsets are for the aarch64
layout of mesa-guest 26.1.x (`vn_ring.c`, `vn_renderer.h`, `vn_renderer_virtgpu.c`).
"""
import ctypes, fcntl, mmap, os, struct, sys, time
pid = int(sys.argv[1]); ring = int(sys.argv[2], 16)
mem = open(f"/proc/{pid}/mem", "rb", 0)
def rd(addr, n):
    mem.seek(addr); return mem.read(n)
rid, inst, shmem, bsize, bmask, head_p, tail_p, status_p, buf_p, extra_p, cur = struct.unpack("<QQQIIQQQQQI", rd(ring, 76))
assert rid == ring, f"not a vn_ring: id {rid:#x}"
refc, res_id, msize, mptr = struct.unpack("<IIQQ", rd(shmem, 24))
gem = struct.unpack("<I", rd(shmem + 48, 4))[0]
now = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
print(f"ring {ring:#x}: cur(tail written)={cur:#x} buffer_size={bsize:#x} shmem res {res_id} gem {gem} size {msize:#x} @ {mptr:#x}")
# last_notify / next_notify: the struct's last two int64, one idle timeout apart.
blob = rd(ring, 1024)
for off in range(80, 1016, 8):
    a, b = struct.unpack_from("<qq", blob, off)
    if b - a == 1_000_000 and 0 < a <= now:
        print(f"last_notify @+{off}: {(now - a) / 1e9:.3f} s ago (next_notify {(now - b) / 1e9:.3f} s ago)")
libc = ctypes.CDLL(None, use_errno=True)
pidfd = libc.syscall(434, pid, 0)
assert pidfd >= 0, os.strerror(ctypes.get_errno())
for name in os.listdir(f"/proc/{pid}/fd"):
    if "/dev/dri/render" not in os.readlink(f"/proc/{pid}/fd/{name}"):
        continue
    fd = libc.syscall(438, pidfd, int(name), 0)
    if fd < 0:
        print(f"fd {name}: pidfd_getfd failed: {os.strerror(ctypes.get_errno())}"); continue
    arg = bytearray(struct.pack("<QII", 0, gem, 0))
    try:
        fcntl.ioctl(fd, 0xC0106441, arg)
    except OSError as e:
        print(f"fd {name}: VIRTGPU_MAP gem {gem}: {e}"); os.close(fd); continue
    off = struct.unpack("<Q", arg[:8])[0]
    m = mmap.mmap(fd, msize, mmap.MAP_SHARED, mmap.PROT_READ, offset=off)
    w = lambda p: struct.unpack_from("<I", m, p - mptr)[0]
    head, tail, status = w(head_p), w(tail_p), w(status_p)
    print(f"fd {name}: head={head:#x} tail={tail:#x} status={status:#x} "
          f"({'IDLE ' if status & 1 else ''}{'FATAL ' if status & 2 else ''}) "
          f"tail {'==' if tail == cur else '!='} guest cur; unread {(tail - head) & 0xffffffff} bytes")
    m.close(); os.close(fd)
