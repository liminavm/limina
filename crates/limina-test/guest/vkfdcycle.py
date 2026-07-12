#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkfdcycle — cross-context dmabuf export/import cycler for the worker fd-census test.

Two VkInstances in one guest process = two venus (virtio-gpu) contexts. Each cycle:
instance A allocates an exportable VkDeviceMemory (dedicated to a VkBuffer) and
exports it as a dma_buf fd; instance B imports that fd. On a virtio-gpu guest the
import runs PRIME fd->handle on B's DRM fd and the gem-object open sends
CTX_ATTACH_RESOURCE(ctx B) to the host — the cross-context attach whose
SCM_RIGHTS-received SHM carrier fd the host venus import used to leak (one fd per
attach; virglrenderer patch 0032, memory `limina-venus-ring-poison`). Everything is
destroyed each cycle, so a healthy worker's carrier-fd census is flat across a run.

Speaks to libvulkan.so.1 via ctypes (no vulkan-tools / gcc needed on the guest) —
same pattern as spikes/venus-4k-dkms/vkprobe.py. Prints structured lines the host
harness asserts on:

    INSTANCE OK | INSTANCE ERR <VkResult>
    DEVICE <name>
    NODEV <substring>
    UNSUPPORTED <extension>
    FDCYCLE FAIL <stage> <VkResult>
    FDCYCLE PASS <cycles>

Usage: vkfdcycle.py [device-substring] [cycles]   (defaults: Venus, 20)
Always exits 0 unless the probe itself is broken; the OUTPUT is the oracle.
"""
import ctypes as C
import sys

VK_SUCCESS = 0
API_1_1 = (1 << 22) | (1 << 12)  # VK_API_VERSION_1_1 (external-memory core deps)
DMA_BUF = 0x200  # VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT
DEVICE_EXTS = (b"VK_KHR_external_memory_fd", b"VK_EXT_external_memory_dma_buf")

P = C.c_void_p  # dispatchable handles / opaque pointers
H = C.c_uint64  # non-dispatchable handles (VkBuffer, VkDeviceMemory) are 64-bit


def vp(x):
    """byref(...) as a c_void_p field value (pNext chains)."""
    return C.cast(C.byref(x), C.c_void_p)


class ApplicationInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 0
        ("pNext", P),
        ("pApplicationName", C.c_char_p),
        ("applicationVersion", C.c_uint32),
        ("pEngineName", C.c_char_p),
        ("engineVersion", C.c_uint32),
        ("apiVersion", C.c_uint32),
    ]


class InstanceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 1
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
        ("sType", C.c_uint32),  # 2
        ("pNext", P),
        ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32),
        ("queueCount", C.c_uint32),
        ("pQueuePriorities", C.POINTER(C.c_float)),
    ]


class DeviceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 3
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
        ("sType", C.c_uint32),  # 12
        ("pNext", P),
        ("flags", C.c_uint32),
        ("size", C.c_uint64),
        ("usage", C.c_uint32),
        ("sharingMode", C.c_uint32),
        ("queueFamilyIndexCount", C.c_uint32),
        ("pQueueFamilyIndices", P),
    ]


class ExternalMemoryBufferCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 1000072000
        ("pNext", P),
        ("handleTypes", C.c_uint32),
    ]


class MemoryRequirements(C.Structure):
    _fields_ = [
        ("size", C.c_uint64),
        ("alignment", C.c_uint64),
        ("memoryTypeBits", C.c_uint32),
    ]


class MemoryAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 5
        ("pNext", P),
        ("allocationSize", C.c_uint64),
        ("memoryTypeIndex", C.c_uint32),
    ]


class ExportMemoryAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 1000072002
        ("pNext", P),
        ("handleTypes", C.c_uint32),
    ]


class ImportMemoryFdInfoKHR(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 1000074000
        ("pNext", P),
        ("handleType", C.c_uint32),
        ("fd", C.c_int),
    ]


class MemoryDedicatedAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 1000127001
        ("pNext", P),
        ("image", H),
        ("buffer", H),
    ]


class MemoryGetFdInfoKHR(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 1000074002
        ("pNext", P),
        ("memory", H),
        ("handleType", C.c_uint32),
    ]


class MemoryFdPropertiesKHR(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 1000074001
        ("pNext", P),
        ("memoryTypeBits", C.c_uint32),
    ]


def load():
    vk = C.CDLL("libvulkan.so.1")
    # Explicit signatures: without argtypes, ctypes passes 64-bit handles as 32-bit
    # C ints — truncating and crashing inside the loader (see vkprobe.py).
    vk.vkCreateInstance.argtypes = [P, P, P]
    vk.vkDestroyInstance.argtypes = [P, P]
    vk.vkEnumeratePhysicalDevices.argtypes = [P, P, P]
    vk.vkGetPhysicalDeviceProperties.argtypes = [P, P]
    vk.vkEnumerateDeviceExtensionProperties.argtypes = [P, C.c_char_p, P, P]
    vk.vkCreateDevice.argtypes = [P, P, P, P]
    vk.vkDestroyDevice.argtypes = [P, P]
    vk.vkGetDeviceProcAddr.argtypes = [P, C.c_char_p]
    vk.vkGetDeviceProcAddr.restype = P
    vk.vkCreateBuffer.argtypes = [P, P, P, P]
    vk.vkDestroyBuffer.argtypes = [P, H, P]
    vk.vkGetBufferMemoryRequirements.argtypes = [P, H, P]
    vk.vkAllocateMemory.argtypes = [P, P, P, P]
    vk.vkFreeMemory.argtypes = [P, H, P]
    vk.vkBindBufferMemory.argtypes = [P, H, H, C.c_uint64]
    for f in (
        vk.vkCreateInstance,
        vk.vkEnumeratePhysicalDevices,
        vk.vkEnumerateDeviceExtensionProperties,
        vk.vkCreateDevice,
        vk.vkCreateBuffer,
        vk.vkAllocateMemory,
        vk.vkBindBufferMemory,
    ):
        f.restype = C.c_int
    return vk


def make_instance(vk):
    app = ApplicationInfo()
    app.sType = 0  # APPLICATION_INFO
    app.apiVersion = API_1_1
    ici = InstanceCreateInfo()
    ici.sType = 1  # INSTANCE_CREATE_INFO
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
        # deviceName (char[256]) at offset 20 of VkPhysicalDeviceProperties.
        props = C.create_string_buffer(4096)
        vk.vkGetPhysicalDeviceProperties(devs[i], props)
        name = props.raw[20 : 20 + 256].split(b"\0")[0].decode()
        print(f"DEVICE {name}", flush=True)
        if want in name:
            return devs[i]
    return None


def make_device(vk, phys):
    """Create a device with the external-memory fd extensions, or report UNSUPPORTED."""
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
    qci.sType = 2  # DEVICE_QUEUE_CREATE_INFO
    qci.queueFamilyIndex = 0
    qci.queueCount = 1
    qci.pQueuePriorities = C.pointer(prio)
    names = (C.c_char_p * len(DEVICE_EXTS))(*DEVICE_EXTS)
    dci = DeviceCreateInfo()
    dci.sType = 3  # DEVICE_CREATE_INFO
    dci.queueCreateInfoCount = 1
    dci.pQueueCreateInfos = vp(qci)
    dci.enabledExtensionCount = len(DEVICE_EXTS)
    dci.ppEnabledExtensionNames = C.cast(names, C.c_void_p)
    dev = C.c_void_p()
    r = vk.vkCreateDevice(phys, C.byref(dci), None, C.byref(dev))
    if r != VK_SUCCESS:
        print(f"FDCYCLE FAIL vkCreateDevice {r}", flush=True)
        return None
    return dev


def make_external_buffer(vk, dev, size):
    """A 1 MiB TRANSFER_SRC|DST buffer declared shareable as a dma_buf."""
    ext = ExternalMemoryBufferCreateInfo()
    ext.sType = 1000072000  # EXTERNAL_MEMORY_BUFFER_CREATE_INFO
    ext.handleTypes = DMA_BUF
    bci = BufferCreateInfo()
    bci.sType = 12  # BUFFER_CREATE_INFO
    bci.pNext = vp(ext)
    bci.size = size
    bci.usage = 0x3  # TRANSFER_SRC | TRANSFER_DST
    bci.sharingMode = 0  # EXCLUSIVE
    buf = H()
    r = vk.vkCreateBuffer(dev, C.byref(bci), None, C.byref(buf))
    if r != VK_SUCCESS:
        return r, None, None
    req = MemoryRequirements()
    vk.vkGetBufferMemoryRequirements(dev, buf, C.byref(req))
    return VK_SUCCESS, buf, req


def first_type(bits):
    for i in range(32):
        if bits & (1 << i):
            return i
    return None


def cycle(vk, dev_a, dev_b, get_fd, get_fd_props, size):
    """One export(A) -> import(B) -> teardown round. Returns an error string or None."""
    r, buf_a, req_a = make_external_buffer(vk, dev_a, size)
    if r != VK_SUCCESS:
        return f"vkCreateBuffer(A) {r}"

    dedicated_a = MemoryDedicatedAllocateInfo()
    dedicated_a.sType = 1000127001  # MEMORY_DEDICATED_ALLOCATE_INFO
    dedicated_a.buffer = buf_a.value
    export = ExportMemoryAllocateInfo()
    export.sType = 1000072002  # EXPORT_MEMORY_ALLOCATE_INFO
    export.pNext = vp(dedicated_a)
    export.handleTypes = DMA_BUF
    ai = MemoryAllocateInfo()
    ai.sType = 5  # MEMORY_ALLOCATE_INFO
    ai.pNext = vp(export)
    ai.allocationSize = req_a.size
    ai.memoryTypeIndex = first_type(req_a.memoryTypeBits)
    mem_a = H()
    r = vk.vkAllocateMemory(dev_a, C.byref(ai), None, C.byref(mem_a))
    if r != VK_SUCCESS:
        return f"vkAllocateMemory(A,export) {r}"
    r = vk.vkBindBufferMemory(dev_a, buf_a, mem_a, 0)
    if r != VK_SUCCESS:
        return f"vkBindBufferMemory(A) {r}"

    gi = MemoryGetFdInfoKHR()
    gi.sType = 1000074002  # MEMORY_GET_FD_INFO_KHR
    gi.memory = mem_a.value
    gi.handleType = DMA_BUF
    fd = C.c_int(-1)
    r = get_fd(dev_a, C.byref(gi), C.byref(fd))
    if r != VK_SUCCESS:
        return f"vkGetMemoryFdKHR {r}"

    fp = MemoryFdPropertiesKHR()
    fp.sType = 1000074001  # MEMORY_FD_PROPERTIES_KHR
    r = get_fd_props(dev_b, DMA_BUF, fd, C.byref(fp))
    if r != VK_SUCCESS:
        return f"vkGetMemoryFdPropertiesKHR {r}"

    r, buf_b, req_b = make_external_buffer(vk, dev_b, size)
    if r != VK_SUCCESS:
        return f"vkCreateBuffer(B) {r}"
    bits = req_b.memoryTypeBits & fp.memoryTypeBits
    if not bits:
        return "no-import-memory-type 0"
    dedicated_b = MemoryDedicatedAllocateInfo()
    dedicated_b.sType = 1000127001
    dedicated_b.buffer = buf_b.value
    imp = ImportMemoryFdInfoKHR()
    imp.sType = 1000074000  # IMPORT_MEMORY_FD_INFO_KHR
    imp.pNext = vp(dedicated_b)
    imp.handleType = DMA_BUF
    imp.fd = fd.value  # consumed by a successful import
    ai_b = MemoryAllocateInfo()
    ai_b.sType = 5
    ai_b.pNext = vp(imp)
    ai_b.allocationSize = req_b.size
    ai_b.memoryTypeIndex = first_type(bits)
    mem_b = H()
    r = vk.vkAllocateMemory(dev_b, C.byref(ai_b), None, C.byref(mem_b))
    if r != VK_SUCCESS:
        return f"vkAllocateMemory(B,import) {r}"
    r = vk.vkBindBufferMemory(dev_b, buf_b, mem_b, 0)
    if r != VK_SUCCESS:
        return f"vkBindBufferMemory(B) {r}"

    # Full teardown: a healthy host frees every carrier this cycle pinned.
    vk.vkDestroyBuffer(dev_b, buf_b, None)
    vk.vkFreeMemory(dev_b, mem_b, None)
    vk.vkDestroyBuffer(dev_a, buf_a, None)
    vk.vkFreeMemory(dev_a, mem_a, None)
    return None


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else "Venus"
    cycles = int(sys.argv[2]) if len(sys.argv) > 2 else 20
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

    for i in range(cycles):
        err = cycle(vk, dev_a, dev_b, get_fd, get_fd_props, 1 << 20)
        if err:
            print(f"FDCYCLE FAIL {err} (cycle {i})", flush=True)
            return

    vk.vkDestroyDevice(dev_a, None)
    vk.vkDestroyDevice(dev_b, None)
    for inst in insts:
        vk.vkDestroyInstance(inst, None)
    print(f"FDCYCLE PASS {cycles}", flush=True)


if __name__ == "__main__":
    main()
