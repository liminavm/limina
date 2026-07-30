#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkfencestorm — venus sync_file fence-loss probe (the §22 wedge invariant).

A guest fence must never vanish: every exported sync_file has to signal
eventually, even when the host rejects the command that carried it (a fenced
submit decoded against a wiped session — the dogfood-mac 2026-07-30 KMS wedge:
`create_fence 30247 -> ErrRutabaga(InvalidContextId)` parked the response
descriptor forever, so the guest dma_fence never signaled and `commit_tail`
hung in D-state until reboot).

Each iteration mirrors what a compositor's explicit-sync path does per frame:

  1. create a VkFence declared exportable as SYNC_FD
  2. empty vkQueueSubmit(fence)      — the fence-out execbuf
  3. vkGetFenceFdKHR(SYNC_FD) -> fd  — a live, possibly-pending sync_file
  4. poll(fd) until POLLIN           — the fence's kernel-side signal

The harness (venus_fence_lost.rs) boots the worker with
LIMINA_GPU_TEST_FAIL_NEXT_FENCE=1, so exactly one of these fences takes the
create-fence failure path host-side. Pre-fix that fd never signals (STUCK);
post-fix the error response still retires the fence and every fd signals.

Output lines the harness asserts on:
    INSTANCE OK | INSTANCE ERR <r>
    DEVICE <name> / NODEV <substring>
    UNSUPPORTED <extension>
    STORM READY
    SIGNAL <n> <ms>
    STUCK <n>
    STORM DONE <signaled>/<total>

Reuses vkfdcycle.py (staged next to it in /tmp) for the ctypes plumbing.
Usage: vkfencestorm.py [iterations] [device-substring]   (default: 50, Venus)
"""
import ctypes as C
import select
import sys
import time

sys.path.insert(0, "/tmp")
from vkfdcycle import (
    VK_SUCCESS,
    BufferCreateInfo,
    DeviceCreateInfo,
    DeviceQueueCreateInfo,
    ExtensionProperties,
    H,
    MemoryAllocateInfo,
    MemoryRequirements,
    P,
    load,
    make_instance,
    pick_device,
    vp,
)
from vkpipeline import (
    CommandBufferAllocateInfo,
    CommandBufferBeginInfo,
    CommandPoolCreateInfo,
)

STORM_EXTS = [b"VK_KHR_external_fence", b"VK_KHR_external_fence_fd"]
SYNC_FD = 0x8  # VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT

# How long one fd may stay pending before we call it lost. An empty submit's
# fence retires in microseconds on a healthy stack; 10 s is pure margin.
STUCK_TIMEOUT_S = 10.0


class ExportFenceCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("handleTypes", C.c_uint32)]


class FenceCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32)]


class FenceGetFdInfoKHR(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("fence", H),
        ("handleType", C.c_uint32),
    ]


class SubmitInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("waitSemaphoreCount", C.c_uint32),
        ("pWaitSemaphores", P),
        ("pWaitDstStageMask", P),
        ("commandBufferCount", C.c_uint32),
        ("pCommandBuffers", P),
        ("signalSemaphoreCount", C.c_uint32),
        ("pSignalSemaphores", P),
    ]





class BufferCopy(C.Structure):
    _fields_ = [("srcOffset", C.c_uint64), ("dstOffset", C.c_uint64), ("size", C.c_uint64)]


class MemoryBarrier(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 46
        ("pNext", P),
        ("srcAccessMask", C.c_uint32),
        ("dstAccessMask", C.c_uint32),
    ]


def record_slow_work(vk, dev, copies):
    """A one-shot command buffer with `copies` serialized 32 MiB buffer copies.

    The point is a submit whose fence stays PENDING for seconds: venus only backs
    a sync_file export with a fresh fence-out execbuf while the fence is pending —
    an already-signaled fence exports a signaled fd and never reaches the host's
    create_fence path (attempt 9 lesson).
    """
    from vkfdcycle import first_type

    size = 32 * 1024 * 1024
    vk.vkCreateBuffer.argtypes = [P, P, P, P]
    vk.vkGetBufferMemoryRequirements.argtypes = [P, H, P]
    vk.vkAllocateMemory.argtypes = [P, P, P, P]
    vk.vkBindBufferMemory.argtypes = [P, H, H, C.c_uint64]
    bufs = []
    for _ in range(2):
        bci = BufferCreateInfo()
        bci.sType = 12
        bci.size = size
        bci.usage = 0x3  # TRANSFER_SRC | TRANSFER_DST
        buf = H()
        if vk.vkCreateBuffer(dev, C.byref(bci), None, C.byref(buf)) != VK_SUCCESS:
            return None
        req = MemoryRequirements()
        vk.vkGetBufferMemoryRequirements(dev, buf.value, C.byref(req))
        mai = MemoryAllocateInfo()
        mai.sType = 5
        mai.allocationSize = req.size
        mai.memoryTypeIndex = first_type(req.memoryTypeBits)
        mem = H()
        if vk.vkAllocateMemory(dev, C.byref(mai), None, C.byref(mem)) != VK_SUCCESS:
            return None
        if vk.vkBindBufferMemory(dev, buf.value, mem.value, 0) != VK_SUCCESS:
            return None
        bufs.append(buf)

    pci = CommandPoolCreateInfo()
    pci.sType = 39
    pool = H()
    if vk.vkCreateCommandPool(dev, C.byref(pci), None, C.byref(pool)) != VK_SUCCESS:
        return None
    cai = CommandBufferAllocateInfo()
    cai.sType = 40
    cai.commandPool = pool.value
    cai.level = 0
    cai.commandBufferCount = 1
    cmd = C.c_void_p()
    if vk.vkAllocateCommandBuffers(dev, C.byref(cai), C.byref(cmd)) != VK_SUCCESS:
        return None
    bi = CommandBufferBeginInfo()
    bi.sType = 42
    if vk.vkBeginCommandBuffer(cmd, C.byref(bi)) != VK_SUCCESS:
        return None
    region = BufferCopy(0, 0, size)
    mb = MemoryBarrier()
    mb.sType = 46
    mb.srcAccessMask = 0x1000  # TRANSFER_WRITE
    mb.dstAccessMask = 0x0800 | 0x1000  # TRANSFER_READ | TRANSFER_WRITE
    vk.vkCmdCopyBuffer.argtypes = [P, H, H, C.c_uint32, P]
    vk.vkCmdPipelineBarrier.argtypes = [
        P, C.c_uint32, C.c_uint32, C.c_uint32,
        C.c_uint32, P, C.c_uint32, P, C.c_uint32, P,
    ]  # fmt: skip
    for i in range(copies):
        vk.vkCmdCopyBuffer(cmd, bufs[i % 2].value, bufs[(i + 1) % 2].value, 1, C.byref(region))
        vk.vkCmdPipelineBarrier(
            cmd, 0x1000, 0x1000, 0, 1, C.byref(mb), 0, None, 0, None
        )
    if vk.vkEndCommandBuffer(cmd) != VK_SUCCESS:
        return None
    return cmd


def make_fence_device(vk, phys):
    """A device with the external-fence fd extensions, or UNSUPPORTED."""
    n = C.c_uint32(0)
    vk.vkEnumerateDeviceExtensionProperties(phys, None, C.byref(n), None)
    exts = (ExtensionProperties * max(n.value, 1))()
    vk.vkEnumerateDeviceExtensionProperties(phys, None, C.byref(n), exts)
    have = {exts[i].extensionName for i in range(n.value)}
    for want in STORM_EXTS:
        if want not in have:
            print(f"UNSUPPORTED {want.decode()}", flush=True)
            return None
    prio = C.c_float(1.0)
    qci = DeviceQueueCreateInfo()
    qci.sType = 2  # DEVICE_QUEUE_CREATE_INFO
    qci.queueFamilyIndex = 0
    qci.queueCount = 1
    qci.pQueuePriorities = C.pointer(prio)
    names = (C.c_char_p * len(STORM_EXTS))(*STORM_EXTS)
    dci = DeviceCreateInfo()
    dci.sType = 3  # DEVICE_CREATE_INFO
    dci.queueCreateInfoCount = 1
    dci.pQueueCreateInfos = vp(qci)
    dci.enabledExtensionCount = len(STORM_EXTS)
    dci.ppEnabledExtensionNames = C.cast(names, C.c_void_p)
    dev = C.c_void_p()
    r = vk.vkCreateDevice(phys, C.byref(dci), None, C.byref(dev))
    if r != VK_SUCCESS:
        print(f"STORM FAIL vkCreateDevice {r}", flush=True)
        return None
    return dev


def ledger(tag):
    """Print the guest's emitted/signaled fence counters (root, debugfs)."""
    import glob

    for p in glob.glob("/sys/kernel/debug/dri/*/*fence*"):
        try:
            print(f"LEDGER {tag} {open(p).read().strip()}", flush=True)
            return
        except OSError:
            pass
    print(f"LEDGER {tag} unavailable", flush=True)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    iterations = int(args[0]) if args else 50
    want = args[1] if len(args) > 1 else "Venus"

    vk = load()
    r, inst = make_instance(vk)
    if r != VK_SUCCESS:
        print(f"INSTANCE ERR {r}", flush=True)
        return 1
    print("INSTANCE OK", flush=True)
    phys = pick_device(vk, inst, want)
    if phys is None:
        print(f"NODEV {want}", flush=True)
        return 1
    dev = make_fence_device(vk, phys)
    if dev is None:
        return 1

    vk.vkGetDeviceQueue.argtypes = [P, C.c_uint32, C.c_uint32, P]
    q = C.c_void_p()
    vk.vkGetDeviceQueue(dev, 0, 0, C.byref(q))

    vk.vkCreateFence.argtypes = [P, P, P, P]
    vk.vkCreateFence.restype = C.c_int
    vk.vkDestroyFence.argtypes = [P, H, P]
    vk.vkQueueSubmit.argtypes = [P, C.c_uint32, P, H]
    vk.vkQueueSubmit.restype = C.c_int
    vk.vkGetDeviceProcAddr.restype = C.c_void_p
    vk.vkGetDeviceProcAddr.argtypes = [P, C.c_char_p]
    p_get_fd = vk.vkGetDeviceProcAddr(dev, b"vkGetFenceFdKHR")
    if not p_get_fd:
        print("UNSUPPORTED vkGetFenceFdKHR", flush=True)
        return 1
    get_fence_fd = C.CFUNCTYPE(C.c_int, P, P, C.POINTER(C.c_int))(p_get_fd)

    # One-shot mode: seconds of real GPU work so the fence is still PENDING at
    # export time (venus only emits the fence-out execbuf for a pending fence).
    slow_cmd = None
    if "--wait-go" in sys.argv:
        copies = int(__import__("os").environ.get("STORM_COPIES", "6000"))
        slow_cmd = record_slow_work(vk, dev, copies)
        if slow_cmd is None:
            print("STORM FAIL record_slow_work", flush=True)
            return 1

    if slow_cmd is not None:
        ledger("ready")
    print("STORM READY", flush=True)
    # One-shot mode: hold AFTER device init (so no later fence on this ring can
    # mask a lost one) until the harness drops /tmp/storm-go, then do the
    # iterations (typically 1). The harness arms the host-side fence-failure
    # seam in the gap — device-init traffic can't steal the poison.
    if "--wait-go" in sys.argv:
        import os

        deadline = time.monotonic() + 60
        while not os.path.exists("/tmp/storm-go"):
            if time.monotonic() > deadline:
                print("STORM FAIL no go-file after 60s", flush=True)
                return 1
            time.sleep(0.2)
        print("STORM GO", flush=True)
    signaled = 0
    for n in range(iterations):
        exp = ExportFenceCreateInfo()
        exp.sType = 1000113000  # EXPORT_FENCE_CREATE_INFO
        exp.handleTypes = SYNC_FD
        fci = FenceCreateInfo()
        fci.sType = 8  # FENCE_CREATE_INFO
        fci.pNext = vp(exp)
        fence = H()
        r = vk.vkCreateFence(dev, C.byref(fci), None, C.byref(fence))
        if r != VK_SUCCESS:
            print(f"STORM FAIL vkCreateFence {r}", flush=True)
            return 1
        if slow_cmd is not None and n == 0:
            si = SubmitInfo()
            si.sType = 4
            si.commandBufferCount = 1
            si.pCommandBuffers = C.cast(C.byref(slow_cmd), C.c_void_p)
            r = vk.vkQueueSubmit(q, 1, C.byref(si), fence.value)
        else:
            r = vk.vkQueueSubmit(q, 0, None, fence.value)
        if r != VK_SUCCESS:
            print(f"STORM FAIL vkQueueSubmit {r}", flush=True)
            return 1
        # Second gate (--wait-go only): the submit itself may carry a fence-out
        # execbuf; holding HERE lets the harness arm the failure seam so the
        # EXPORT's execbuf fence — the one backing the fd we poll — takes the
        # poison, with nothing after it on the ring to mask the loss.
        if "--wait-go" in sys.argv and n == 0:
            import os

            ledger("submitted")
            print("STORM SUBMITTED", flush=True)
            deadline = time.monotonic() + 60
            while not os.path.exists("/tmp/storm-go2"):
                if time.monotonic() > deadline:
                    print("STORM FAIL no go2-file after 60s", flush=True)
                    return 1
                time.sleep(0.2)
        info = FenceGetFdInfoKHR()
        info.sType = 1000115000  # FENCE_GET_FD_INFO_KHR
        info.fence = fence.value
        info.handleType = SYNC_FD
        fd = C.c_int(-1)
        r = get_fence_fd(dev, vp(info), C.byref(fd))
        if r != VK_SUCCESS:
            print(f"STORM FAIL vkGetFenceFdKHR {r}", flush=True)
            return 1
        if slow_cmd is not None and n == 0:
            ledger("exported")
        # fd == -1 is a legal "already signaled" export.
        if fd.value < 0:
            signaled += 1
            print(f"SIGNAL {n} 0.0", flush=True)
        else:
            t0 = time.monotonic()
            ready, _, _ = select.select([fd.value], [], [], STUCK_TIMEOUT_S)
            if ready:
                signaled += 1
                print(f"SIGNAL {n} {(time.monotonic() - t0) * 1000:.1f}", flush=True)
            else:
                print(f"STUCK {n}", flush=True)
            import os

            os.close(fd.value)
        # NOTE: no vkWaitForFences after export — SYNC_FD export RESETS the
        # fence (spikes/venus-fence-truth: waiting it post-export blocks forever).
        if slow_cmd is not None and n == 0:
            vk.vkQueueWaitIdle.argtypes = [P]
            vk.vkQueueWaitIdle(q)  # drain the copy work before teardown
        vk.vkDestroyFence(dev, fence.value, None)
    print(f"STORM DONE {signaled}/{iterations}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
