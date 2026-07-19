#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkfdhold — park a freed-while-exported cross-context blob across suspend/resume.

The ctx-15 replay failure (M9.3 eyeball item 3): a guest process allocates an
exportable VkDeviceMemory (venus CREATE_BLOBs it, blob_id = the memory's object id),
another context imports the dma_buf, and the exporter then vkFreeMemory's its handle
while the imported memory keeps the blob resource attached — Xwayland's routine
window-buffer flow. Pre-fix, the free pruned the vkAllocateMemory entry from the
re-creation journal, so a later snapshot replay could not re-create the memory the
CREATE_BLOB needs: `blob_id N is not a live VkDeviceMemory` — a structural failure
that kills the whole replay. The virgl pin fix retains the alloc (and the free,
replayed after the blob create) for the resource's lifetime.

This probe reproduces that exact state and then sleeps so the harness can suspend
mid-hold: export(A) → import(B) → free A's memory + destroy A's buffer → hold B's
side alive → print FDHOLD READY → sleep forever. Reuses vkfdcycle.py (staged next
to it) for all the ctypes plumbing.

Output lines the harness asserts on:
    INSTANCE OK | INSTANCE ERR <r>
    DEVICE <name> / NODEV <substring> / UNSUPPORTED <ext>
    FDHOLD FAIL <stage> <r>
    FDHOLD READY

Usage: vkfdhold.py [device-substring]   (default: Venus)
"""
import ctypes as C
import sys
import time

sys.path.insert(0, "/tmp")
from vkfdcycle import (
    DMA_BUF,
    VK_SUCCESS,
    ExportMemoryAllocateInfo,
    ImportMemoryFdInfoKHR,
    MemoryAllocateInfo,
    MemoryDedicatedAllocateInfo,
    MemoryFdPropertiesKHR,
    MemoryGetFdInfoKHR,
    H,
    P,
    first_type,
    load,
    make_device,
    make_external_buffer,
    make_instance,
    pick_device,
    vp,
)


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else "Venus"
    vk = load()

    insts = []
    for _ in range(2):
        r, inst = make_instance(vk)
        if r != VK_SUCCESS:
            print(f"INSTANCE ERR {r}", flush=True)
            return
        insts.append(inst)
    print("INSTANCE OK", flush=True)

    phys_a = pick_device(vk, insts[0], want)
    phys_b = pick_device(vk, insts[1], want)
    if phys_a is None or phys_b is None:
        print(f"NODEV {want}", flush=True)
        return
    dev_a = make_device(vk, phys_a)
    dev_b = make_device(vk, phys_b)
    if dev_a is None or dev_b is None:
        return

    fdcycle_t = C.CFUNCTYPE(C.c_int, P, P, P)
    get_fd = fdcycle_t(vk.vkGetDeviceProcAddr(dev_a, b"vkGetMemoryFdKHR"))
    props_t = C.CFUNCTYPE(C.c_int, P, C.c_uint32, C.c_int, P)
    get_fd_props = props_t(vk.vkGetDeviceProcAddr(dev_b, b"vkGetMemoryFdPropertiesKHR"))

    size = 1 << 20

    # export side (context A)
    r, buf_a, req_a = make_external_buffer(vk, dev_a, size)
    if r != VK_SUCCESS:
        print(f"FDHOLD FAIL vkCreateBuffer(A) {r}", flush=True)
        return
    dedicated_a = MemoryDedicatedAllocateInfo()
    dedicated_a.sType = 1000127001
    dedicated_a.buffer = buf_a.value
    export = ExportMemoryAllocateInfo()
    export.sType = 1000072002
    export.pNext = vp(dedicated_a)
    export.handleTypes = DMA_BUF
    ai = MemoryAllocateInfo()
    ai.sType = 5
    ai.pNext = vp(export)
    ai.allocationSize = req_a.size
    ai.memoryTypeIndex = first_type(req_a.memoryTypeBits)
    mem_a = H()
    r = vk.vkAllocateMemory(dev_a, C.byref(ai), None, C.byref(mem_a))
    if r != VK_SUCCESS:
        print(f"FDHOLD FAIL vkAllocateMemory(A,export) {r}", flush=True)
        return
    r = vk.vkBindBufferMemory(dev_a, buf_a, mem_a, 0)
    if r != VK_SUCCESS:
        print(f"FDHOLD FAIL vkBindBufferMemory(A) {r}", flush=True)
        return

    gi = MemoryGetFdInfoKHR()
    gi.sType = 1000074002
    gi.memory = mem_a.value
    gi.handleType = DMA_BUF
    fd = C.c_int(-1)
    r = get_fd(dev_a, C.byref(gi), C.byref(fd))
    if r != VK_SUCCESS:
        print(f"FDHOLD FAIL vkGetMemoryFdKHR {r}", flush=True)
        return

    # import side (context B)
    fp = MemoryFdPropertiesKHR()
    fp.sType = 1000074001
    r = get_fd_props(dev_b, DMA_BUF, fd, C.byref(fp))
    if r != VK_SUCCESS:
        print(f"FDHOLD FAIL vkGetMemoryFdPropertiesKHR {r}", flush=True)
        return
    r, buf_b, req_b = make_external_buffer(vk, dev_b, size)
    if r != VK_SUCCESS:
        print(f"FDHOLD FAIL vkCreateBuffer(B) {r}", flush=True)
        return
    bits = req_b.memoryTypeBits & fp.memoryTypeBits
    if not bits:
        print("FDHOLD FAIL no-import-memory-type 0", flush=True)
        return
    dedicated_b = MemoryDedicatedAllocateInfo()
    dedicated_b.sType = 1000127001
    dedicated_b.buffer = buf_b.value
    imp = ImportMemoryFdInfoKHR()
    imp.sType = 1000074000
    imp.pNext = vp(dedicated_b)
    imp.handleType = DMA_BUF
    imp.fd = fd.value
    ai_b = MemoryAllocateInfo()
    ai_b.sType = 5
    ai_b.pNext = vp(imp)
    ai_b.allocationSize = req_b.size
    ai_b.memoryTypeIndex = first_type(bits)
    mem_b = H()
    r = vk.vkAllocateMemory(dev_b, C.byref(ai_b), None, C.byref(mem_b))
    if r != VK_SUCCESS:
        print(f"FDHOLD FAIL vkAllocateMemory(B,import) {r}", flush=True)
        return
    r = vk.vkBindBufferMemory(dev_b, buf_b, mem_b, 0)
    if r != VK_SUCCESS:
        print(f"FDHOLD FAIL vkBindBufferMemory(B) {r}", flush=True)
        return

    # THE state under test: the exporter frees its handle; the import keeps the
    # blob resource attached. (Xwayland does this with every shared window buffer.)
    vk.vkDestroyBuffer(dev_a, buf_a, None)
    vk.vkFreeMemory(dev_a, mem_a, None)

    print("FDHOLD READY", flush=True)

    # Heartbeat: a synchronous host round-trip that references the PARKED imported
    # memory (vn_GetDeviceMemoryCommitment is a vn_call on B's ring naming mem_b).
    # If a restore fails to re-create the freed-while-exported blob, mem_b's host
    # object is missing: the host decode fails the lookup, the ring goes FATAL, the
    # call wedges in vn_relax and mesa aborts this process — the beats stop and the
    # harness's advance-after-restore assert fails. Generic calls are NOT enough
    # (run 23: fresh allocs kept beating while the parked import was broken).
    vk.vkGetDeviceMemoryCommitment.argtypes = [P, H, P]
    n = 0
    while True:
        committed = C.c_uint64(0)
        vk.vkGetDeviceMemoryCommitment(dev_b, mem_b, C.byref(committed))
        n += 1
        print(f"FDHOLD BEAT {n}", flush=True)
        time.sleep(5)


if __name__ == "__main__":
    main()
