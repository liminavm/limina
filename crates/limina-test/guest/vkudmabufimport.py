#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkudmabufimport — import a udmabuf (guest-RAM dmabuf) into venus.

GStreamer's shape (gstglupload/gstvideo since 1.24): software-decoded frames are
placed in memfd pages, wrapped as a dmabuf via /dev/udmabuf, and handed to the GL
or Vulkan stack for zero-copy upload:

    memfd_create -> F_SEAL_SHRINK -> UDMABUF_CREATE -> vkGetMemoryFdPropertiesKHR
      -> vkAllocateMemory(VkImportMemoryFdInfoKHR) -> vkBindBufferMemory

This is a DIFFERENT resource class from vkclassicimport.py's gbm buffers: the
pages are plain guest anonymous memory, not a virtio-gpu resource, so the guest
kernel's PRIME import must first materialize a virtio-gpu object for them and the
host must see that object attached to the venus context. The 2026-08-13 dogfood
totem crash died on exactly this leg: host-side `failed to import resource:
invalid res_id`, then the async-ghost ring-FATAL. This probe is the regression
oracle for the udmabuf leg, independent of GStreamer's negotiation mood.

Stage layering (each isolates a different suspect):

    UDMABUF OK | UDMABUF FAIL <stage>           guest kernel: memfd->dmabuf
    PRIME OK <handle> | PRIME FAIL <errno>      guest kernel: virtio-gpu accepts
                                                the foreign dmabuf at all
    PROPS OK 0x<bits> | PROPS FAIL <VkResult>   venus+host: fd -> res_id lookup
    IMPORT OK | IMPORT FAIL <stage> <VkResult>  the vkAllocateMemory import
    ALIAS OK | ALIAS SKIP <why> | ALIAS FAIL    pattern written to the memfd is
                                                visible through venus (GPU copy)
    CONTEXT ALIVE | CONTEXT DEAD <stage> <r>    a plain alloc+map still works
                                                AFTER whatever happened above —
                                                the async-ghost seam oracle
    UDMABUFIMPORT PASS | UDMABUFIMPORT FAIL <stage>

CONTEXT is printed even on failure paths (that is the point: an import failure
must degrade, not kill the ring). Always exits 0 unless the probe itself is
broken; the OUTPUT is the oracle. Needs the venus ICD selected (VK_DRIVER_FILES)
and rw access to /dev/udmabuf + a virtio-gpu render node.
"""
import ctypes as C
import fcntl
import mmap
import os
import struct
import sys

VK_SUCCESS = 0
API_1_1 = (1 << 22) | (1 << 12)
DMA_BUF = 0x200  # VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT
DEVICE_EXTS = (b"VK_KHR_external_memory_fd", b"VK_EXT_external_memory_dma_buf")

W, H_PX = 256, 256
STRIDE = W * 4
SIZE = STRIDE * H_PX  # 256 KiB — a multiple of both 4k and 16k pages

UDMABUF_CREATE = 0x40187542  # _IOW('u', 0x42, struct udmabuf_create{u32,u32,u64,u64})
UDMABUF_FLAGS_CLOEXEC = 0x01
DRM_IOCTL_PRIME_FD_TO_HANDLE = 0xC00C642E  # _IOWR('d', 0x2e, {u32 handle,u32 flags,s32 fd})
DRM_IOCTL_GEM_CLOSE = 0x40086409  # _IOW('d', 0x09, {u32 handle, u32 pad})
# _IOWR('d', 0x40+0x05, struct drm_virtgpu_resource_info{u32 bo_handle,res_handle,size,blob_mem})
DRM_IOCTL_VIRTGPU_RESOURCE_INFO = 0xC0106445

P = C.c_void_p
H = C.c_uint64


def vp(x):
    return C.cast(C.byref(x), C.c_void_p)


class ApplicationInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("pApplicationName", C.c_char_p),
        ("applicationVersion", C.c_uint32),
        ("pEngineName", C.c_char_p),
        ("engineVersion", C.c_uint32),
        ("apiVersion", C.c_uint32),
    ]


class InstanceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("flags", C.c_uint32),
        ("pApplicationInfo", P),
        ("enabledLayerCount", C.c_uint32),
        ("ppEnabledLayerNames", P),
        ("enabledExtensionCount", C.c_uint32),
        ("ppEnabledExtensionNames", P),
    ]


class DeviceQueueCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32),
        ("queueCount", C.c_uint32),
        ("pQueuePriorities", C.POINTER(C.c_float)),
    ]


class DeviceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("flags", C.c_uint32),
        ("queueCreateInfoCount", C.c_uint32),
        ("pQueueCreateInfos", P),
        ("enabledLayerCount", C.c_uint32),
        ("ppEnabledLayerNames", P),
        ("enabledExtensionCount", C.c_uint32),
        ("ppEnabledExtensionNames", P),
        ("pEnabledFeatures", P),
    ]


class ExtensionProperties(C.Structure):
    _fields_ = [("extensionName", C.c_char * 256), ("specVersion", C.c_uint32)]


class BufferCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("flags", C.c_uint32),
        ("size", C.c_uint64),
        ("usage", C.c_uint32),
        ("sharingMode", C.c_uint32),
        ("queueFamilyIndexCount", C.c_uint32),
        ("pQueueFamilyIndices", P),
    ]


class ExternalMemoryBufferCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("handleTypes", C.c_uint32)]


class MemoryRequirements(C.Structure):
    _fields_ = [
        ("size", C.c_uint64),
        ("alignment", C.c_uint64),
        ("memoryTypeBits", C.c_uint32),
    ]


class MemoryAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("allocationSize", C.c_uint64),
        ("memoryTypeIndex", C.c_uint32),
    ]


class ImportMemoryFdInfoKHR(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("handleType", C.c_uint32),
        ("fd", C.c_int),
    ]


class MemoryDedicatedAllocateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("image", H), ("buffer", H)]


class MemoryFdPropertiesKHR(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("memoryTypeBits", C.c_uint32)]


class CommandPoolCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32),
    ]


class CommandBufferAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("commandPool", H),
        ("level", C.c_uint32),
        ("commandBufferCount", C.c_uint32),
    ]


class CommandBufferBeginInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
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


class PhysicalDeviceMemoryProperties(C.Structure):
    _fields_ = [
        ("memoryTypeCount", C.c_uint32),
        ("memoryTypes", (C.c_uint32 * 2) * 32),
        ("memoryHeapCount", C.c_uint32),
        ("_pad", C.c_uint32),
        ("memoryHeaps", (C.c_uint64 * 2) * 16),
    ]


def load_vk():
    vk = C.CDLL("libvulkan.so.1")
    vk.vkCreateInstance.argtypes = [P, P, P]
    vk.vkEnumeratePhysicalDevices.argtypes = [P, P, P]
    vk.vkGetPhysicalDeviceProperties.argtypes = [P, P]
    vk.vkGetPhysicalDeviceMemoryProperties.argtypes = [P, P]
    vk.vkEnumerateDeviceExtensionProperties.argtypes = [P, C.c_char_p, P, P]
    vk.vkCreateDevice.argtypes = [P, P, P, P]
    vk.vkGetDeviceProcAddr.argtypes = [P, C.c_char_p]
    vk.vkGetDeviceProcAddr.restype = P
    vk.vkCreateBuffer.argtypes = [P, P, P, P]
    vk.vkGetBufferMemoryRequirements.argtypes = [P, H, P]
    vk.vkAllocateMemory.argtypes = [P, P, P, P]
    vk.vkFreeMemory.argtypes = [P, H, P]
    vk.vkBindBufferMemory.argtypes = [P, H, H, C.c_uint64]
    vk.vkMapMemory.argtypes = [P, H, C.c_uint64, C.c_uint64, C.c_uint32, P]
    vk.vkUnmapMemory.argtypes = [P, H]
    vk.vkGetDeviceQueue.argtypes = [P, C.c_uint32, C.c_uint32, P]
    vk.vkCreateCommandPool.argtypes = [P, P, P, P]
    vk.vkAllocateCommandBuffers.argtypes = [P, P, P]
    vk.vkBeginCommandBuffer.argtypes = [P, P]
    vk.vkCmdCopyBuffer.argtypes = [P, H, H, C.c_uint32, P]
    vk.vkEndCommandBuffer.argtypes = [P]
    vk.vkQueueSubmit.argtypes = [P, C.c_uint32, P, H]
    vk.vkQueueWaitIdle.argtypes = [P]
    for f in (
        vk.vkCreateCommandPool,
        vk.vkAllocateCommandBuffers,
        vk.vkBeginCommandBuffer,
        vk.vkEndCommandBuffer,
        vk.vkQueueSubmit,
        vk.vkQueueWaitIdle,
        vk.vkCreateInstance,
        vk.vkEnumeratePhysicalDevices,
        vk.vkEnumerateDeviceExtensionProperties,
        vk.vkCreateDevice,
        vk.vkCreateBuffer,
        vk.vkAllocateMemory,
        vk.vkBindBufferMemory,
        vk.vkMapMemory,
    ):
        f.restype = C.c_int
    return vk


def make_udmabuf():
    """memfd -> seal -> /dev/udmabuf. Returns (dmabuf_fd, memfd) or None."""
    page = os.sysconf("SC_PAGE_SIZE")
    if SIZE % page:
        print(f"UDMABUF FAIL size-not-page-multiple-{page}", flush=True)
        return None
    try:
        memfd = os.memfd_create("vkudmabufimport", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
    except OSError as e:
        print(f"UDMABUF FAIL memfd_create-{e.errno}", flush=True)
        return None
    os.ftruncate(memfd, SIZE)
    # Pattern first, seal, then wrap: udmabuf requires F_SEAL_SHRINK.
    with mmap.mmap(memfd, SIZE) as m:
        m.write(bytes([0x11, 0x22, 0x33, 0x44]) * (SIZE // 4))
    try:
        fcntl.fcntl(memfd, fcntl.F_ADD_SEALS, fcntl.F_SEAL_SHRINK)
    except OSError as e:
        print(f"UDMABUF FAIL seal-{e.errno}", flush=True)
        return None
    try:
        dev = os.open("/dev/udmabuf", os.O_RDWR)
    except OSError as e:
        print(f"UDMABUF FAIL open-/dev/udmabuf-{e.errno}", flush=True)
        return None
    # Mutable buffer so ioctl() returns the syscall's return value — for
    # UDMABUF_CREATE that IS the new dmabuf fd (immutable bytes would make
    # Python return the buffer copy and discard the fd).
    req = bytearray(struct.pack("=IIQQ", memfd, UDMABUF_FLAGS_CLOEXEC, 0, SIZE))
    try:
        buf_fd = fcntl.ioctl(dev, UDMABUF_CREATE, req)
    except OSError as e:
        print(f"UDMABUF FAIL UDMABUF_CREATE-{e.errno}", flush=True)
        return None
    finally:
        os.close(dev)
    print("UDMABUF OK", flush=True)
    return buf_fd, memfd


def prime_check(dmabuf_fd):
    """Mirror venus's first step in isolation: can virtio-gpu PRIME-import this
    foreign dmabuf at all? Prints PRIME OK/FAIL; never fatal to the probe (venus
    does its own import on its own fd — this is layering evidence only)."""
    node = None
    for cand in sorted(os.listdir("/dev/dri")):
        if cand.startswith("renderD"):
            node = f"/dev/dri/{cand}"
            break
    if node is None:
        print("PRIME FAIL no-render-node", flush=True)
        return
    drm = os.open(node, os.O_RDWR)
    try:
        arg = bytearray(struct.pack("=IIi", 0, 0, dmabuf_fd))
        try:
            fcntl.ioctl(drm, DRM_IOCTL_PRIME_FD_TO_HANDLE, arg)
        except OSError as e:
            print(f"PRIME FAIL {e.errno}", flush=True)
            return
        handle = struct.unpack_from("=I", arg)[0]
        print(f"PRIME OK {handle}", flush=True)
        # Does the imported gem object carry a HOST resource id? venus's
        # bo_create_from_dma_buf needs one; res_handle 0 / EINVAL here means
        # the guest kernel never created a host-side resource for these pages
        # and the failure is guest-side, host exonerated.
        info = bytearray(struct.pack("=IIII", handle, 0, 0, 0))
        try:
            fcntl.ioctl(drm, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, info)
            _, res_handle, size, blob_mem = struct.unpack("=IIII", bytes(info))
            print(f"RESINFO OK res_id={res_handle} size={size} blob_mem={blob_mem}", flush=True)
        except OSError as e:
            print(f"RESINFO FAIL {e.errno}", flush=True)
        fcntl.ioctl(drm, DRM_IOCTL_GEM_CLOSE, struct.pack("=II", handle, 0))
    finally:
        os.close(drm)


def make_instance(vk):
    app = ApplicationInfo()
    app.sType = 0
    app.apiVersion = API_1_1
    ici = InstanceCreateInfo()
    ici.sType = 1
    ici.pApplicationInfo = vp(app)
    inst = C.c_void_p()
    r = vk.vkCreateInstance(C.byref(ici), None, C.byref(inst))
    return r, inst


def pick_device(vk, inst, want):
    n = C.c_uint32(0)
    vk.vkEnumeratePhysicalDevices(inst, C.byref(n), None)
    devs = (C.c_void_p * max(n.value, 1))()
    vk.vkEnumeratePhysicalDevices(inst, C.byref(n), devs)
    for i in range(n.value):
        props = C.create_string_buffer(4096)
        vk.vkGetPhysicalDeviceProperties(devs[i], props)
        name = props.raw[20 : 20 + 256].split(b"\0")[0].decode()
        print(f"DEVICE {name}", flush=True)
        if want in name:
            return devs[i]
    return None


def make_device(vk, phys):
    n = C.c_uint32(0)
    vk.vkEnumerateDeviceExtensionProperties(phys, None, C.byref(n), None)
    exts = (ExtensionProperties * max(n.value, 1))()
    vk.vkEnumerateDeviceExtensionProperties(phys, None, C.byref(n), exts)
    have = {exts[i].extensionName for i in range(n.value)}
    for want in DEVICE_EXTS:
        if want not in have:
            print(f"UNSUPPORTED {want.decode()}", flush=True)
            return None
    prio = C.c_float(1.0)
    qci = DeviceQueueCreateInfo()
    qci.sType = 2
    qci.queueFamilyIndex = 0
    qci.queueCount = 1
    qci.pQueuePriorities = C.pointer(prio)
    names = (C.c_char_p * len(DEVICE_EXTS))(*DEVICE_EXTS)
    dci = DeviceCreateInfo()
    dci.sType = 3
    dci.queueCreateInfoCount = 1
    dci.pQueueCreateInfos = vp(qci)
    dci.enabledExtensionCount = len(DEVICE_EXTS)
    dci.ppEnabledExtensionNames = C.cast(names, C.c_void_p)
    dev = C.c_void_p()
    r = vk.vkCreateDevice(phys, C.byref(dci), None, C.byref(dev))
    if r != VK_SUCCESS:
        print(f"IMPORT FAIL vkCreateDevice {r}", flush=True)
        return None
    return dev


def host_visible(vk, phys, type_index):
    mp = PhysicalDeviceMemoryProperties()
    vk.vkGetPhysicalDeviceMemoryProperties(phys, C.byref(mp))
    if type_index >= mp.memoryTypeCount:
        return False
    return bool(mp.memoryTypes[type_index][0] & 0x2)  # HOST_VISIBLE_BIT


def first_type(bits):
    for i in range(32):
        if bits & (1 << i):
            return i
    return None


def context_alive(vk, phys, dev):
    """The seam oracle: AFTER the import attempt (success or failure), a plain
    non-imported alloc+map must still work. If the ring died, every venus call
    from here returns OOM (-1/-2)."""
    bci = BufferCreateInfo()
    bci.sType = 12
    bci.size = 4096
    bci.usage = 0x2
    bci.sharingMode = 0
    buf = H()
    r = vk.vkCreateBuffer(dev, C.byref(bci), None, C.byref(buf))
    if r != VK_SUCCESS:
        print(f"CONTEXT DEAD create {r}", flush=True)
        return
    req = MemoryRequirements()
    vk.vkGetBufferMemoryRequirements(dev, buf, C.byref(req))
    idx = None
    for i in range(32):
        if req.memoryTypeBits & (1 << i) and host_visible(vk, phys, i):
            idx = i
            break
    if idx is None:
        print("CONTEXT DEAD no-host-visible-type 0", flush=True)
        return
    ai = MemoryAllocateInfo()
    ai.sType = 5
    ai.allocationSize = req.size
    ai.memoryTypeIndex = idx
    mem = H()
    r = vk.vkAllocateMemory(dev, C.byref(ai), None, C.byref(mem))
    if r != VK_SUCCESS:
        print(f"CONTEXT DEAD alloc {r}", flush=True)
        return
    data = C.c_void_p()
    r = vk.vkMapMemory(dev, mem, 0, 4096, 0, C.byref(data))
    if r != VK_SUCCESS or not data.value:
        print(f"CONTEXT DEAD map {r}", flush=True)
        return
    vk.vkUnmapMemory(dev, mem)
    print("CONTEXT ALIVE", flush=True)


def gpu_readback_check(vk, phys, dev, src_buf, size):
    bci = BufferCreateInfo()
    bci.sType = 12
    bci.size = size
    bci.usage = 0x2  # TRANSFER_DST
    bci.sharingMode = 0
    dst = H()
    if vk.vkCreateBuffer(dev, C.byref(bci), None, C.byref(dst)) != VK_SUCCESS:
        print("ALIAS SKIP dst-create", flush=True)
        return None
    req = MemoryRequirements()
    vk.vkGetBufferMemoryRequirements(dev, dst, C.byref(req))
    idx = None
    for i in range(32):
        if req.memoryTypeBits & (1 << i) and host_visible(vk, phys, i):
            idx = i
            break
    if idx is None:
        print("ALIAS SKIP no-host-visible-dst-type", flush=True)
        return None
    ai = MemoryAllocateInfo()
    ai.sType = 5
    ai.allocationSize = req.size
    ai.memoryTypeIndex = idx
    dmem = H()
    if vk.vkAllocateMemory(dev, C.byref(ai), None, C.byref(dmem)) != VK_SUCCESS:
        print("ALIAS SKIP dst-alloc", flush=True)
        return None
    if vk.vkBindBufferMemory(dev, dst, dmem, 0) != VK_SUCCESS:
        print("ALIAS SKIP dst-bind", flush=True)
        return None

    queue = C.c_void_p()
    vk.vkGetDeviceQueue(dev, 0, 0, C.byref(queue))
    pci = CommandPoolCreateInfo()
    pci.sType = 39
    pci.queueFamilyIndex = 0
    pool = H()
    if vk.vkCreateCommandPool(dev, C.byref(pci), None, C.byref(pool)) != VK_SUCCESS:
        print("ALIAS SKIP cmdpool", flush=True)
        return None
    cai = CommandBufferAllocateInfo()
    cai.sType = 40
    cai.commandPool = pool.value
    cai.level = 0
    cai.commandBufferCount = 1
    cmd = C.c_void_p()
    if vk.vkAllocateCommandBuffers(dev, C.byref(cai), C.byref(cmd)) != VK_SUCCESS:
        print("ALIAS SKIP cmdbuf", flush=True)
        return None
    cbi = CommandBufferBeginInfo()
    cbi.sType = 42
    cbi.flags = 0x1
    vk.vkBeginCommandBuffer(cmd, C.byref(cbi))
    region = BufferCopy()
    region.size = size
    vk.vkCmdCopyBuffer(cmd, src_buf, dst, 1, C.byref(region))
    vk.vkEndCommandBuffer(cmd)
    si = SubmitInfo()
    si.sType = 4
    si.commandBufferCount = 1
    si.pCommandBuffers = C.cast(C.byref(cmd), C.c_void_p)
    r = vk.vkQueueSubmit(queue, 1, C.byref(si), 0)
    if r != VK_SUCCESS:
        print(f"ALIAS SKIP submit-{r}", flush=True)
        return None
    vk.vkQueueWaitIdle(queue)

    data = C.c_void_p()
    r = vk.vkMapMemory(dev, dmem, 0, size, 0, C.byref(data))
    if r != VK_SUCCESS or not data.value:
        print(f"ALIAS SKIP dst-map-{r}", flush=True)
        return None
    got0 = C.string_at(data.value, 4)
    gotmid = C.string_at(data.value + size // 2, 4)
    vk.vkUnmapMemory(dev, dmem)
    if got0 == bytes([0x11, 0x22, 0x33, 0x44]) and gotmid == got0:
        return True
    print(f"ALIAS FAIL start={got0.hex()} mid={gotmid.hex()}", flush=True)
    return False


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else "Venus"
    # "props" (default): the well-behaved flow — vkGetMemoryFdPropertiesKHR gates
    # the import, so an unattachable resource fails there, synchronously and clean.
    # "forcealloc": skip the props gate and drive vkAllocateMemory(import) directly
    # — legal per spec, and exactly what exposes the venus async-alloc ghost: the
    # host rejects the import but the async guest keeps a live-looking handle; the
    # next command naming it ring-FATALs. This mode is the seam regression oracle:
    # after the fix, IMPORT FAIL + CONTEXT ALIVE; before it, CONTEXT DEAD.
    mode = sys.argv[2] if len(sys.argv) > 2 else "props"
    print(f"MODE {mode}", flush=True)

    def fail(stage):
        print(f"UDMABUFIMPORT FAIL {stage}", flush=True)

    got = make_udmabuf()
    if got is None:
        fail("udmabuf")
        return
    buf_fd, _memfd = got
    prime_check(buf_fd)

    vk = load_vk()
    r, inst = make_instance(vk)
    if r != VK_SUCCESS:
        print(f"INSTANCE ERR {r}", flush=True)
        fail("instance")
        return
    phys = pick_device(vk, inst, want)
    if phys is None:
        print(f"NODEV {want}", flush=True)
        fail("nodev")
        return
    dev = make_device(vk, phys)
    if dev is None:
        fail("device")
        return

    fp = MemoryFdPropertiesKHR()
    fp.sType = 1000074001
    if mode == "forcealloc":
        # No props gate: pretend every type is importable and let the host-side
        # vkAllocateMemory import be the first thing that can refuse.
        fp.memoryTypeBits = 0xFFFFFFFF
    else:
        props_t = C.CFUNCTYPE(C.c_int, P, C.c_uint32, C.c_int, P)
        get_fd_props = props_t(
            vk.vkGetDeviceProcAddr(dev, b"vkGetMemoryFdPropertiesKHR")
        )
        r = get_fd_props(dev, DMA_BUF, buf_fd, C.byref(fp))
        if r != VK_SUCCESS:
            print(f"PROPS FAIL {r}", flush=True)
            context_alive(vk, phys, dev)
            fail("props")
            return
        print(f"PROPS OK 0x{fp.memoryTypeBits:x}", flush=True)

    ext = ExternalMemoryBufferCreateInfo()
    ext.sType = 1000072000
    ext.handleTypes = DMA_BUF
    bci = BufferCreateInfo()
    bci.sType = 12
    bci.pNext = vp(ext)
    bci.size = SIZE
    bci.usage = 0x3  # TRANSFER_SRC | TRANSFER_DST
    bci.sharingMode = 0
    buf = H()
    r = vk.vkCreateBuffer(dev, C.byref(bci), None, C.byref(buf))
    if r != VK_SUCCESS:
        print(f"IMPORT FAIL vkCreateBuffer {r}", flush=True)
        context_alive(vk, phys, dev)
        fail("import")
        return
    req = MemoryRequirements()
    vk.vkGetBufferMemoryRequirements(dev, buf, C.byref(req))
    bits = req.memoryTypeBits & fp.memoryTypeBits
    if not bits:
        print("IMPORT FAIL no-common-memory-type 0", flush=True)
        context_alive(vk, phys, dev)
        fail("import")
        return

    dedicated = MemoryDedicatedAllocateInfo()
    dedicated.sType = 1000127001
    dedicated.buffer = buf.value
    imp = ImportMemoryFdInfoKHR()
    imp.sType = 1000074000
    imp.pNext = vp(dedicated)
    imp.handleType = DMA_BUF
    imp.fd = os.dup(buf_fd)  # consumed by a successful import
    ai = MemoryAllocateInfo()
    ai.sType = 5
    ai.pNext = vp(imp)
    ai.allocationSize = max(req.size, SIZE)
    ai.memoryTypeIndex = first_type(bits)
    mem = H()
    r = vk.vkAllocateMemory(dev, C.byref(ai), None, C.byref(mem))
    if r != VK_SUCCESS:
        print(f"IMPORT FAIL vkAllocateMemory {r}", flush=True)
        context_alive(vk, phys, dev)
        fail("import")
        return
    if mode == "forcealloc":
        # On the broken stack this VK_SUCCESS is a LIE (async ghost): the host
        # refused the import, and the bind below — the first command naming the
        # ghost handle — poisons the ring. mesa's vn_relax SIGABRTs the process
        # on the FATAL bit, so a missing CONTEXT line IS the red signal; the
        # harness must assert on "CONTEXT ALIVE" presence, not exit code.
        print("ALLOC-RET VK_SUCCESS (ghost if host refused)", flush=True)
    r = vk.vkBindBufferMemory(dev, buf, mem, 0)
    if r != VK_SUCCESS:
        print(f"IMPORT FAIL vkBindBufferMemory {r}", flush=True)
        context_alive(vk, phys, dev)
        fail("import")
        return
    print("IMPORT OK", flush=True)
    if mode == "forcealloc":
        context_alive(vk, phys, dev)
        print("UDMABUFIMPORT PASS forcealloc", flush=True)
        return

    # Alias proof: the pattern written into the memfd must be visible through
    # the imported venus memory (GPU copy into a mappable venus buffer). This is
    # the leg that catches a "successful" import backed by the WRONG pages.
    alias = gpu_readback_check(vk, phys, dev, buf, SIZE)
    if alias is True:
        print("ALIAS OK", flush=True)
    context_alive(vk, phys, dev)
    if alias is False:
        fail("alias")
        return

    print("UDMABUFIMPORT PASS", flush=True)


if __name__ == "__main__":
    main()
