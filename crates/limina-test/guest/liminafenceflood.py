#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""One render-node file asks the Metal transport for more fences than will ever complete.

`flood` makes a limina context with a real ring, then asks for FENCE_COUNT fences on a seqno the
ring never reaches, and holds the file open: each one keeps a control-queue descriptor until it is
answered. It prints `FLOOD ok=N busy=M` (EBUSY refusals) once every ioctl has returned, then sleeps.
`probe` asks the device for its INFO on a fresh file and prints `PROBE ok`; a control queue filled
by the flood leaves it blocked instead.
"""
import ctypes
import errno
import fcntl
import mmap
import os
import sys
import time

FENCE_COUNT = 300
RING_DATA = 4096
RING_MAGIC = 0x52474D4C
WIRE_VERSION_OFF = 4


def iowr(nr, size, write=True, read=True):
    d = (1 if write else 0) | (2 if read else 0)
    return (d << 30) | (size << 16) | (ord("d") << 8) | (0x40 + nr)


class Info(ctypes.Structure):
    _fields_ = [("wire_version", ctypes.c_uint32), ("flags", ctypes.c_uint32)]


class CtxCreate(ctypes.Structure):
    _fields_ = [("flags", ctypes.c_uint32), ("pad", ctypes.c_uint32),
                ("doorbell_offset", ctypes.c_uint64)]


class ArenaCreate(ctypes.Structure):
    _fields_ = [("arena_id", ctypes.c_uint32), ("flags", ctypes.c_uint32),
                ("size", ctypes.c_uint64), ("gpu_base", ctypes.c_uint64),
                ("mmap_offset", ctypes.c_uint64)]


class RingAttach(ctypes.Structure):
    _fields_ = [("arena_id", ctypes.c_uint32), ("data_size", ctypes.c_uint32),
                ("offset", ctypes.c_uint64)]


class Fence(ctypes.Structure):
    _fields_ = [("seqno", ctypes.c_uint64), ("handles", ctypes.c_uint64),
                ("num_handles", ctypes.c_uint32), ("flags", ctypes.c_uint32),
                ("fence_fd", ctypes.c_int32), ("pad", ctypes.c_uint32)]


INFO = iowr(0x29, ctypes.sizeof(Info), write=False)
CTX_CREATE = iowr(0x20, ctypes.sizeof(CtxCreate))
ARENA_CREATE = iowr(0x21, ctypes.sizeof(ArenaCreate))
RING_ATTACH = iowr(0x23, ctypes.sizeof(RingAttach), read=False)
FENCE = iowr(0x26, ctypes.sizeof(Fence))


def node():
    """The first render node that answers INFO: the one with the extension."""
    refused = []
    for n in range(128, 192):
        path = f"/dev/dri/renderD{n}"
        if not os.path.exists(path):
            continue
        fd = os.open(path, os.O_RDWR | os.O_CLOEXEC)
        info = Info()
        try:
            fcntl.ioctl(fd, INFO, info)
            return fd, info.wire_version
        except OSError as e:
            refused.append(f"{path}: {e}")
            os.close(fd)
    sys.exit(f"error: no render node with the limina extension ({'; '.join(refused)})")


def flood():
    fd, version = node()
    fcntl.ioctl(fd, CTX_CREATE, CtxCreate())
    arena = ArenaCreate(arena_id=1, size=512 + RING_DATA)
    fcntl.ioctl(fd, ARENA_CREATE, arena)
    ring = mmap.mmap(fd, arena.size, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE,
                     offset=arena.mmap_offset)
    ring[0:512] = bytes(512)
    ring[0:4] = RING_MAGIC.to_bytes(4, "little")
    ring[WIRE_VERSION_OFF:WIRE_VERSION_OFF + 4] = version.to_bytes(4, "little")
    fcntl.ioctl(fd, RING_ATTACH, RingAttach(arena_id=1, data_size=RING_DATA, offset=0))
    print("ATTACHED", flush=True)
    ok = busy = 0
    for _ in range(FENCE_COUNT):
        try:
            fcntl.ioctl(fd, FENCE, Fence(seqno=2**64 - 1))
            ok += 1
        except OSError as e:
            if e.errno != errno.EBUSY:
                raise
            busy += 1
    print(f"FLOOD ok={ok} busy={busy}", flush=True)
    time.sleep(3600)


def probe():
    fd, _ = node()
    os.close(fd)
    print("PROBE ok", flush=True)


{"flood": flood, "probe": probe}[sys.argv[1]]()
