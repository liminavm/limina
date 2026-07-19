#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkcontent — device-memory content integrity across suspend/resume (M9.3 P2).

The P2 gap: venus (vn) defers virtio-gpu blob creation until vkMapMemory, so a
never-mapped VkDeviceMemory is a plain host allocation — its bytes live in a KK
heap on the HOST, covered by neither the guest-RAM dump nor the P1 mapped-blob
capture. Textures and render targets are exactly this class; without content
capture a restored session replays the object graph but the pixels are garbage.

This probe parks a known pattern in that class and verifies it after restore:

  1. staging buffer S: host-mapped (=> blob, P1-covered), filled with a pattern
  2. device buffer D: never mapped (=> plain host alloc, THE memory under test)
  3. GPU copy S -> D, wait idle
  4. overwrite S with zeros through the mapping (a copy-back can't pass vacuously)
  5. print CONTENT READY, then poll for the trigger file /tmp/vkcontent-go
     (the harness touches it after restore), heartbeating CONTENT WAIT n
  6. on trigger: GPU copy D -> S with a FRESH command pool on the restored ring,
     wait idle, compare S's mapping against the pattern

Output lines the harness asserts on:
    INSTANCE OK | INSTANCE ERR <r>
    DEVICE <name> / NODEV <substring>
    CONTENT FAIL <stage> <r>
    CONTENT READY
    CONTENT WAIT <n>
    CONTENT OK | CONTENT BAD <mismatches>/<total> first=<offset> got=<b> want=<b>

Reuses vkfdcycle.py (staged next to it in /tmp) for the ctypes plumbing.
Usage: vkcontent.py [device-substring]   (default: Venus)
"""
import ctypes as C
import os
import sys
import time

sys.path.insert(0, "/tmp")
from vkfdcycle import (
    VK_SUCCESS,
    BufferCreateInfo,
    MemoryAllocateInfo,
    H,
    P,
    first_type,
    load,
    make_device,
    make_instance,
    pick_device,
    vp,
)

TRIGGER = "/tmp/vkcontent-go"
SIZE = 1 << 20  # 1 MiB


class CommandPoolCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 39
        ("pNext", P),
        ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32),
    ]


class CommandBufferAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 40
        ("pNext", P),
        ("commandPool", H),
        ("level", C.c_uint32),
        ("commandBufferCount", C.c_uint32),
    ]


class CommandBufferBeginInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 42
        ("pNext", P),
        ("flags", C.c_uint32),
        ("pInheritanceInfo", P),
    ]


class BufferCopy(C.Structure):
    _fields_ = [
        ("srcOffset", C.c_uint64),
        ("dstOffset", C.c_uint64),
        ("size", C.c_uint64),
    ]


class SubmitInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 4
        ("pNext", P),
        ("waitSemaphoreCount", C.c_uint32),
        ("pWaitSemaphores", P),
        ("pWaitDstStageMask", P),
        ("commandBufferCount", C.c_uint32),
        ("pCommandBuffers", P),
        ("signalSemaphoreCount", C.c_uint32),
        ("pSignalSemaphores", P),
    ]


def load_extra(vk):
    vk.vkGetDeviceQueue.argtypes = [P, C.c_uint32, C.c_uint32, P]
    vk.vkCreateCommandPool.argtypes = [P, P, P, P]
    vk.vkDestroyCommandPool.argtypes = [P, H, P]
    vk.vkAllocateCommandBuffers.argtypes = [P, P, P]
    vk.vkBeginCommandBuffer.argtypes = [P, P]
    vk.vkCmdCopyBuffer.argtypes = [P, H, H, C.c_uint32, P]
    vk.vkEndCommandBuffer.argtypes = [P]
    vk.vkQueueSubmit.argtypes = [P, C.c_uint32, P, H]
    vk.vkQueueWaitIdle.argtypes = [P]
    vk.vkMapMemory.argtypes = [P, H, C.c_uint64, C.c_uint64, C.c_uint32, P]
    for f in (
        vk.vkCreateCommandPool,
        vk.vkAllocateCommandBuffers,
        vk.vkBeginCommandBuffer,
        vk.vkEndCommandBuffer,
        vk.vkQueueSubmit,
        vk.vkQueueWaitIdle,
        vk.vkMapMemory,
    ):
        f.restype = C.c_int


def make_plain_buffer(vk, dev, size):
    """A plain (non-external) TRANSFER_SRC|DST buffer + its memory, bound."""
    bci = BufferCreateInfo()
    bci.sType = 12  # BUFFER_CREATE_INFO
    bci.size = size
    bci.usage = 0x3  # TRANSFER_SRC | TRANSFER_DST
    bci.sharingMode = 0
    buf = H()
    r = vk.vkCreateBuffer(dev, C.byref(bci), None, C.byref(buf))
    if r != VK_SUCCESS:
        return f"vkCreateBuffer {r}", None, None
    from vkfdcycle import MemoryRequirements

    req = MemoryRequirements()
    vk.vkGetBufferMemoryRequirements(dev, buf, C.byref(req))
    ai = MemoryAllocateInfo()
    ai.sType = 5
    ai.allocationSize = req.size
    ai.memoryTypeIndex = first_type(req.memoryTypeBits)
    mem = H()
    r = vk.vkAllocateMemory(dev, C.byref(ai), None, C.byref(mem))
    if r != VK_SUCCESS:
        return f"vkAllocateMemory {r}", None, None
    r = vk.vkBindBufferMemory(dev, buf, mem, 0)
    if r != VK_SUCCESS:
        return f"vkBindBufferMemory {r}", None, None
    return None, buf, mem


def gpu_copy(vk, dev, queue, src, dst, size):
    """One-shot: fresh pool -> record CopyBuffer -> submit -> wait idle."""
    pci = CommandPoolCreateInfo()
    pci.sType = 39
    pci.flags = 0x2  # RESET_COMMAND_BUFFER (irrelevant; pool is one-shot)
    pci.queueFamilyIndex = 0
    pool = H()
    r = vk.vkCreateCommandPool(dev, C.byref(pci), None, C.byref(pool))
    if r != VK_SUCCESS:
        return f"vkCreateCommandPool {r}"
    cai = CommandBufferAllocateInfo()
    cai.sType = 40
    cai.commandPool = pool.value
    cai.level = 0  # PRIMARY
    cai.commandBufferCount = 1
    cmd = C.c_void_p()
    r = vk.vkAllocateCommandBuffers(dev, C.byref(cai), C.byref(cmd))
    if r != VK_SUCCESS:
        return f"vkAllocateCommandBuffers {r}"
    bi = CommandBufferBeginInfo()
    bi.sType = 42
    bi.flags = 0x1  # ONE_TIME_SUBMIT
    r = vk.vkBeginCommandBuffer(cmd, C.byref(bi))
    if r != VK_SUCCESS:
        return f"vkBeginCommandBuffer {r}"
    region = BufferCopy()
    region.size = size
    vk.vkCmdCopyBuffer(cmd, src, dst, 1, C.byref(region))
    r = vk.vkEndCommandBuffer(cmd)
    if r != VK_SUCCESS:
        return f"vkEndCommandBuffer {r}"
    si = SubmitInfo()
    si.sType = 4
    si.commandBufferCount = 1
    si.pCommandBuffers = C.cast(C.byref(cmd), C.c_void_p)
    r = vk.vkQueueSubmit(queue, 1, C.byref(si), 0)
    if r != VK_SUCCESS:
        return f"vkQueueSubmit {r}"
    r = vk.vkQueueWaitIdle(queue)
    if r != VK_SUCCESS:
        return f"vkQueueWaitIdle {r}"
    vk.vkDestroyCommandPool(dev, pool, None)
    return None


def pattern(size):
    # Non-zero, position-dependent, cheap: a fresh (zeroed or garbage) host
    # alloc can't match it by accident.
    return bytes(((i * 131) ^ (i >> 8) ^ 0xA5) & 0xFF for i in range(size))


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else "Venus"
    vk = load()
    load_extra(vk)

    r, inst = make_instance(vk)
    if r != VK_SUCCESS:
        print(f"INSTANCE ERR {r}", flush=True)
        return
    print("INSTANCE OK", flush=True)
    phys = pick_device(vk, inst, want)
    if phys is None:
        print(f"NODEV {want}", flush=True)
        return
    dev = make_device(vk, phys)
    if dev is None:
        return
    queue = C.c_void_p()
    vk.vkGetDeviceQueue(dev, 0, 0, C.byref(queue))

    # S: staging, host-mapped => blob (P1-covered).
    err, buf_s, mem_s = make_plain_buffer(vk, dev, SIZE)
    if err:
        print(f"CONTENT FAIL S-{err}", flush=True)
        return
    ptr = C.c_void_p()
    r = vk.vkMapMemory(dev, mem_s, 0, SIZE, 0, C.byref(ptr))
    if r != VK_SUCCESS:
        print(f"CONTENT FAIL vkMapMemory {r}", flush=True)
        return
    # D: never mapped => plain host alloc (THE memory under test).
    err, buf_d, _mem_d = make_plain_buffer(vk, dev, SIZE)
    if err:
        print(f"CONTENT FAIL D-{err}", flush=True)
        return

    pat = pattern(SIZE)
    C.memmove(ptr, pat, SIZE)
    err = gpu_copy(vk, dev, queue, buf_s, buf_d, SIZE)
    if err:
        print(f"CONTENT FAIL copy-in {err}", flush=True)
        return
    # Zero S so the post-restore copy-back can't pass vacuously.
    C.memset(ptr, 0, SIZE)

    print("CONTENT READY", flush=True)

    n = 0
    while not os.path.exists(TRIGGER):
        n += 1
        print(f"CONTENT WAIT {n}", flush=True)
        time.sleep(2)

    err = gpu_copy(vk, dev, queue, buf_d, buf_s, SIZE)
    if err:
        print(f"CONTENT FAIL copy-back {err}", flush=True)
        return
    got = C.string_at(ptr, SIZE)
    if got == pat:
        print("CONTENT OK", flush=True)
    else:
        bad = sum(1 for a, b in zip(got, pat) if a != b)
        first = next(i for i, (a, b) in enumerate(zip(got, pat)) if a != b)
        print(
            f"CONTENT BAD {bad}/{SIZE} first={first} got={got[first]} want={pat[first]}",
            flush=True,
        )


if __name__ == "__main__":
    main()
