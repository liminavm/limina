#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkclassicimport — import a CLASSIC virgl (vrend) gbm buffer into venus.

A Vulkan compositor's shape: it allocates buffers with gbm — which, since the
drop-guest-zink GL flip, resolves to the **virgl dri backend** (classic vrend pipe
resources, not venus blobs) — exports the dmabuf, and imports it into venus:

    gbm_bo_create(...) -> gbm_bo_get_fd -> vkGetMemoryFdPropertiesKHR
      -> vkAllocateMemory(VkImportMemoryFdInfoKHR) -> vkBindBufferMemory

Host-side that lands in vkr_dispatch_vkGetMemoryResourcePropertiesMESA, which needs
the resource in the venus context's table; proxy_context_attach_resource used to
drop classic pipe resources on macOS (no dmabuf export) -> "invalid res_id" ->
VK_ERROR_INVALID_EXTERNAL_HANDLE per frame. This probe is the regression oracle for
that import path.

TWO buffer classes, selected by argv[2] — they exercise different host gates:

  scanout   gbm_bo_create(USE_SCANOUT | USE_RENDERING) — the compositor's own KMS
            framebuffer. Backed by an IOSurface since the EGLImage scanout work.
  rendering gbm_bo_create(USE_RENDERING) — a CLIENT's window buffer, the far more
            numerous class. vrend only IOSurface-backed VIRGL_BIND_SCANOUT
            resources at first, so these imports failed while the compositor's own
            framebuffer worked; a Vulkan compositor could not run on virtio_gpu.

Both classes reach vrend with VIRGL_BIND_SHARED: gbm_bo_create unconditionally sets
__DRI_IMAGE_USE_SHARE ("Gallium drivers requires shared in order to get the
handle/stride"), which dri2 maps to PIPE_BIND_SHARED and virgl to VIRGL_BIND_SHARED.

MESA_LOADER_DRIVER_OVERRIDE must be `virtio_gpu` in the environment (the harness
sets it) — with `zink` the gbm buffer is a venus blob and the probe tests nothing.

Speaks ctypes to libgbm.so.1 + libvulkan.so.1 (no gcc on the guest); same pattern
as vkfdcycle.py. Structured output the harness asserts on:

    USAGE <mode>
    GBM OK <stride> | GBM FAIL <stage>
    DEVICE <name> | NODEV <substring> | UNSUPPORTED <ext> | INSTANCE ERR <r>
    PROPS OK 0x<memoryTypeBits> | PROPS FAIL <VkResult>
    IMPORT OK | IMPORT FAIL <stage> <VkResult>
    ALIAS OK | ALIAS SKIP <why> | ALIAS FAIL <detail>
    CLASSICIMPORT PASS <mode> | CLASSICIMPORT FAIL <mode> <stage>

ALIAS writes a pattern through gbm_bo_map and reads it back through the imported
venus memory — proof the two views share storage. It SKIPs (non-fatal) where a
mapping isn't available; PROPS/IMPORT/BIND are the load-bearing assertions.

Always exits 0 unless the probe itself is broken; the OUTPUT is the oracle.
"""
import ctypes as C
import os
import sys

VK_SUCCESS = 0
API_1_1 = (1 << 22) | (1 << 12)
DMA_BUF = 0x200  # VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT
DEVICE_EXTS = (b"VK_KHR_external_memory_fd", b"VK_EXT_external_memory_dma_buf")

GBM_FORMAT_ARGB8888 = 0x34325241  # fourcc 'AR24'
GBM_BO_USE_SCANOUT = 1 << 0
GBM_BO_USE_RENDERING = 1 << 2
GBM_BO_TRANSFER_WRITE = 1 << 1

# argv[2] -> gbm usage. "rendering" deliberately omits USE_SCANOUT: that is the
# client-buffer class, and the only thing distinguishing it host-side is which
# vrend bind flags arrive.
USAGE_MODES = {
    "scanout": GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING,
    "rendering": GBM_BO_USE_RENDERING,
}

W, H_PX = 256, 256

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
    # memoryTypeCount + 32*{propertyFlags,heapIndex} + heapCount + 16*{size,flags};
    # only propertyFlags per type index is read.
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
    vk.vkDestroyInstance.argtypes = [P, P]
    vk.vkEnumeratePhysicalDevices.argtypes = [P, P, P]
    vk.vkGetPhysicalDeviceProperties.argtypes = [P, P]
    vk.vkGetPhysicalDeviceMemoryProperties.argtypes = [P, P]
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
    vk.vkMapMemory.argtypes = [P, H, C.c_uint64, C.c_uint64, C.c_uint32, P]
    vk.vkUnmapMemory.argtypes = [P, H]
    vk.vkGetDeviceQueue.argtypes = [P, C.c_uint32, C.c_uint32, P]
    vk.vkCreateCommandPool.argtypes = [P, P, P, P]
    vk.vkDestroyCommandPool.argtypes = [P, H, P]
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


def load_gbm():
    gbm = C.CDLL("libgbm.so.1")
    gbm.gbm_create_device.argtypes = [C.c_int]
    gbm.gbm_create_device.restype = P
    gbm.gbm_device_destroy.argtypes = [P]
    gbm.gbm_bo_create.argtypes = [P, C.c_uint32, C.c_uint32, C.c_uint32, C.c_uint32]
    gbm.gbm_bo_create.restype = P
    gbm.gbm_bo_destroy.argtypes = [P]
    gbm.gbm_bo_get_fd.argtypes = [P]
    gbm.gbm_bo_get_fd.restype = C.c_int
    gbm.gbm_bo_get_stride.argtypes = [P]
    gbm.gbm_bo_get_stride.restype = C.c_uint32
    gbm.gbm_bo_map.argtypes = [
        P,
        C.c_uint32,
        C.c_uint32,
        C.c_uint32,
        C.c_uint32,
        C.c_uint32,
        C.POINTER(C.c_uint32),
        C.POINTER(C.c_void_p),
    ]
    gbm.gbm_bo_map.restype = P
    gbm.gbm_bo_unmap.argtypes = [P, P]
    return gbm


def make_gbm_bo(gbm, usage):
    node = None
    for cand in sorted(os.listdir("/dev/dri")):
        if cand.startswith("renderD"):
            node = f"/dev/dri/{cand}"
            break
    if node is None:
        print("GBM FAIL no-render-node", flush=True)
        return None
    fd = os.open(node, os.O_RDWR)
    dev = gbm.gbm_create_device(fd)
    if not dev:
        print("GBM FAIL gbm_create_device", flush=True)
        return None
    bo = gbm.gbm_bo_create(dev, W, H_PX, GBM_FORMAT_ARGB8888, usage)
    if not bo:
        print("GBM FAIL gbm_bo_create", flush=True)
        return None
    bo_fd = gbm.gbm_bo_get_fd(bo)
    if bo_fd < 0:
        print("GBM FAIL gbm_bo_get_fd", flush=True)
        return None
    stride = gbm.gbm_bo_get_stride(bo)
    print(f"GBM OK {stride}", flush=True)
    return dev, bo, bo_fd, stride


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


def gpu_readback_check(vk, phys, dev, src_buf, size, stride):
    """Copy the imported buffer into a fresh host-visible buffer, map, compare.
    True = pattern matches, False = mismatch (printed), None = SKIP (printed)."""
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
    pci.sType = 39  # COMMAND_POOL_CREATE_INFO
    pci.queueFamilyIndex = 0
    pool = H()
    if vk.vkCreateCommandPool(dev, C.byref(pci), None, C.byref(pool)) != VK_SUCCESS:
        print("ALIAS SKIP cmdpool", flush=True)
        return None
    cai = CommandBufferAllocateInfo()
    cai.sType = 40  # COMMAND_BUFFER_ALLOCATE_INFO
    cai.commandPool = pool.value
    cai.level = 0
    cai.commandBufferCount = 1
    cmd = C.c_void_p()
    if vk.vkAllocateCommandBuffers(dev, C.byref(cai), C.byref(cmd)) != VK_SUCCESS:
        print("ALIAS SKIP cmdbuf", flush=True)
        return None
    cbi = CommandBufferBeginInfo()
    cbi.sType = 42  # COMMAND_BUFFER_BEGIN_INFO
    cbi.flags = 0x1  # ONE_TIME_SUBMIT
    vk.vkBeginCommandBuffer(cmd, C.byref(cbi))
    region = BufferCopy()
    region.size = size
    vk.vkCmdCopyBuffer(cmd, src_buf, dst, 1, C.byref(region))
    vk.vkEndCommandBuffer(cmd)
    si = SubmitInfo()
    si.sType = 4  # SUBMIT_INFO
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
    gotmid = C.string_at(data.value + (H_PX // 2) * stride, 4)
    vk.vkUnmapMemory(dev, dmem)
    if got0 == bytes([0x11, 0x22, 0x33, 0x44]) and gotmid == got0:
        return True
    print(f"ALIAS FAIL row0={got0.hex()} mid={gotmid.hex()}", flush=True)
    return False


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else "Venus"
    mode = sys.argv[2] if len(sys.argv) > 2 else "scanout"
    print(f"USAGE {mode}", flush=True)

    def fail(stage):
        print(f"CLASSICIMPORT FAIL {mode} {stage}", flush=True)

    if mode not in USAGE_MODES:
        fail(f"unknown-usage-mode-{mode}")
        return
    override = os.environ.get("MESA_LOADER_DRIVER_OVERRIDE", "")
    if override != "virtio_gpu":
        # With zink the gbm buffer is a venus blob and this probe tests nothing.
        fail(f"loader-override-is-{override or 'unset'}")
        return

    gbm = load_gbm()
    got = make_gbm_bo(gbm, USAGE_MODES[mode])
    if got is None:
        fail("gbm")
        return
    gbm_dev, bo, bo_fd, stride = got

    # Write an R/B-asymmetric pattern through gbm before the Vulkan side ever
    # sees the buffer (transfer flushes on unmap).
    pattern = bytes([0x11, 0x22, 0x33, 0x44] * W)
    map_stride = C.c_uint32(0)
    map_data = C.c_void_p()
    mp = gbm.gbm_bo_map(
        bo, 0, 0, W, H_PX, GBM_BO_TRANSFER_WRITE, C.byref(map_stride), C.byref(map_data)
    )
    wrote_pattern = False
    if mp:
        row = C.create_string_buffer(pattern, len(pattern))
        for y in range(H_PX):
            C.memmove(mp + y * map_stride.value, row, W * 4)
        gbm.gbm_bo_unmap(bo, map_data)
        wrote_pattern = True

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

    props_t = C.CFUNCTYPE(C.c_int, P, C.c_uint32, C.c_int, P)
    get_fd_props = props_t(vk.vkGetDeviceProcAddr(dev, b"vkGetMemoryFdPropertiesKHR"))

    fp = MemoryFdPropertiesKHR()
    fp.sType = 1000074001
    r = get_fd_props(dev, DMA_BUF, bo_fd, C.byref(fp))
    if r != VK_SUCCESS:
        print(f"PROPS FAIL {r}", flush=True)
        fail("props")
        return
    print(f"PROPS OK 0x{fp.memoryTypeBits:x}", flush=True)

    # The import: an external buffer bound to the imported memory (the memory-level
    # mechanics are what the host must support; synoik's image bind rides the same
    # vkAllocateMemory import).
    ext = ExternalMemoryBufferCreateInfo()
    ext.sType = 1000072000
    ext.handleTypes = DMA_BUF
    size = stride * H_PX
    bci = BufferCreateInfo()
    bci.sType = 12
    bci.pNext = vp(ext)
    bci.size = size
    bci.usage = 0x3  # TRANSFER_SRC | TRANSFER_DST
    bci.sharingMode = 0
    buf = H()
    r = vk.vkCreateBuffer(dev, C.byref(bci), None, C.byref(buf))
    if r != VK_SUCCESS:
        print(f"IMPORT FAIL vkCreateBuffer {r}", flush=True)
        fail("import")
        return
    req = MemoryRequirements()
    vk.vkGetBufferMemoryRequirements(dev, buf, C.byref(req))
    bits = req.memoryTypeBits & fp.memoryTypeBits
    if not bits:
        print("IMPORT FAIL no-common-memory-type 0", flush=True)
        fail("import")
        return

    dedicated = MemoryDedicatedAllocateInfo()
    dedicated.sType = 1000127001
    dedicated.buffer = buf.value
    imp = ImportMemoryFdInfoKHR()
    imp.sType = 1000074000
    imp.pNext = vp(dedicated)
    imp.handleType = DMA_BUF
    imp.fd = os.dup(bo_fd)  # consumed by a successful import
    ai = MemoryAllocateInfo()
    ai.sType = 5
    ai.pNext = vp(imp)
    ai.allocationSize = max(req.size, size)
    ai.memoryTypeIndex = first_type(bits)
    mem = H()
    r = vk.vkAllocateMemory(dev, C.byref(ai), None, C.byref(mem))
    if r != VK_SUCCESS:
        print(f"IMPORT FAIL vkAllocateMemory {r}", flush=True)
        fail("import")
        return
    r = vk.vkBindBufferMemory(dev, buf, mem, 0)
    if r != VK_SUCCESS:
        print(f"IMPORT FAIL vkBindBufferMemory {r}", flush=True)
        fail("import")
        return
    print("IMPORT OK", flush=True)

    # Alias proof: the pattern written through gbm (a virgl TRANSFER into the
    # vrend texture -> the IOSurface bytes) must be visible through the imported
    # venus memory. Imported memory isn't guest-mappable on venus, so read it
    # with a GPU copy into a plain host-visible venus buffer and map THAT.
    if not wrote_pattern:
        print("ALIAS SKIP gbm-map-unavailable", flush=True)
    else:
        alias = gpu_readback_check(vk, phys, dev, buf, size, stride)
        if alias is True:
            print("ALIAS OK", flush=True)
        elif alias is False:
            fail("alias")
            return
        # None = SKIP already printed

    print(f"CLASSICIMPORT PASS {mode}", flush=True)


if __name__ == "__main__":
    main()
