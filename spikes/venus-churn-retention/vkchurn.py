#!/usr/bin/env python3
"""Churn venus allocations with a tiny live set, so the HOST can be asked whether it
reclaims what the guest frees.

The question this exists to answer (synoik, 2026-08-07): a compositor churned ~84 GB of 4K
scanout buffers through a live set of only ~133 MB, and the host worker grew monotonically
to 50 GB. If host frees kept pace with guest frees, that should have been steady-state ugly,
not monotonic growth. So: does the host retain what the guest has already freed?

The guest half is deliberately trivial and unambiguous — allocate, free, repeat, holding at
most `live` allocations at a time. Anything the host keeps beyond `live * size` is retention
on the host side, because the guest has provably let go.

Modes:
  mem    — vkAllocateMemory / vkFreeMemory only. The floor: no image, no binding, no export.
  image  — vkCreateImage + vkAllocateMemory + vkBindImageMemory, then destroy all three.
           Closer to the real workload, and the path where a host-side IOSurface or Metal
           texture would be created per iteration.

  scanout— vkCreateImage with VkExternalMemoryImageCreateInfo chained, then vkDestroyImage.
           Nothing else: no memory, no bind, no export. That struct plus an IOSurface-mappable
           format is the entire trigger for the host to allocate an IOSurface per image
           (vkr_image.c), which is the object the 2026-08-07 storm retained by the thousand.
           Deliberately the SMALLEST thing that should reproduce it. It does NOT: measured
           2026-08-07, 20 images charged 20 IOSurfaces and credited every one, teardown
           clean. It is therefore the NEGATIVE CONTROL — proof the allocate/free pair is
           balanced on its own.

  scanout-leak
         — the above plus vkAllocateMemory + vkBindImageMemory, destroying the image but
           NEVER freeing the memory, then holding the process open. The host's +1 on the
           IOSurface hangs off the MEMORY object, so this is where retention should appear.
           It is the guest compositor's bug in miniature, and the vehicle for the question
           that actually matters: does SIGKILLing it give the host memory back?
           CAVEAT: it never WRITES, so it retains address space rather than resident pages.
           Its clean-teardown result is therefore about the ledger, not about footprint.

  scanout-bind / scanout-render
         — the A/B for the 2026-08-07 finding that our objects all die while the KERNEL
           IOSurface survives. Both create a scanout image, bind dedicated exported memory,
           then destroy the image and free the memory — a complete release, every iteration.
           scanout-render additionally CLEARS the image on the GPU and waits for it. One
           variable: was the surface used. Retention in the render arm alone means the
           surviving reference is taken on use, and it reproduces the storm without a
           compositor. Both hold the process open afterwards so the census can be read
           against a live context and the teardown tested by SIGKILL.

Default size is 3840x2160x4 = 33,177,600 B — the exact size of the leaked regions, so a
host-side census can match on it.

Usage: vkchurn.py <device-substring> mem|image <iterations> [live] [width] [height]

Output:
    DEVICE <name> | NODEV <sub>
    START <mode> <iterations> x <bytes> live=<n>
    PROGRESS <i>                     every 200 iterations
    CHURN DONE <mode> <completed> <bytes-each>
    CHURN FAIL <stage> <VkResult>
"""
import ctypes as C
import os
import sys
import time

VK_SUCCESS = 0
API_1_1 = (1 << 22) | (1 << 12)

P = C.c_void_p
H = C.c_uint64


def vp(x):
    return C.cast(C.byref(x), C.c_void_p)


class ApplicationInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("pApplicationName", C.c_char_p),
        ("applicationVersion", C.c_uint32), ("pEngineName", C.c_char_p),
        ("engineVersion", C.c_uint32), ("apiVersion", C.c_uint32),
    ]


class InstanceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
        ("pApplicationInfo", P), ("enabledLayerCount", C.c_uint32),
        ("ppEnabledLayerNames", P), ("enabledExtensionCount", C.c_uint32),
        ("ppEnabledExtensionNames", P),
    ]


class DeviceQueueCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32), ("queueCount", C.c_uint32),
        ("pQueuePriorities", C.POINTER(C.c_float)),
    ]


class DeviceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
        ("queueCreateInfoCount", C.c_uint32), ("pQueueCreateInfos", P),
        ("enabledLayerCount", C.c_uint32), ("ppEnabledLayerNames", P),
        ("enabledExtensionCount", C.c_uint32), ("ppEnabledExtensionNames", P),
        ("pEnabledFeatures", P),
    ]


class MemoryAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("allocationSize", C.c_uint64),
        ("memoryTypeIndex", C.c_uint32),
    ]


class ExportMemoryAllocateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("handleTypes", C.c_uint32)]


class MemoryDedicatedAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("image", C.c_uint64),
        ("buffer", C.c_uint64),
    ]


class Extent3D(C.Structure):
    _fields_ = [("width", C.c_uint32), ("height", C.c_uint32), ("depth", C.c_uint32)]


class ImageCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
        ("imageType", C.c_uint32), ("format", C.c_uint32), ("extent", Extent3D),
        ("mipLevels", C.c_uint32), ("arrayLayers", C.c_uint32), ("samples", C.c_uint32),
        ("tiling", C.c_uint32), ("usage", C.c_uint32), ("sharingMode", C.c_uint32),
        ("queueFamilyIndexCount", C.c_uint32), ("pQueueFamilyIndices", P),
        ("initialLayout", C.c_uint32),
    ]


class MemoryRequirements(C.Structure):
    _fields_ = [("size", C.c_uint64), ("alignment", C.c_uint64),
                ("memoryTypeBits", C.c_uint32)]


class ExternalMemoryImageCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("handleTypes", C.c_uint32)]


class ImageDrmFormatModifierListCreateInfoEXT(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P),
                ("drmFormatModifierCount", C.c_uint32),
                ("pDrmFormatModifiers", C.POINTER(C.c_uint64))]


# Values read from include/vulkan/vulkan_core.h, not recalled.
ST_EXTERNAL_MEMORY_IMAGE = 1000072001   # VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO
ST_MODIFIER_LIST = 1000158003           # ..._IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT
TILING_DRM_MODIFIER = 1000158000        # VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT
DRM_FORMAT_MOD_LINEAR = 0
# VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT — what a Linux guest actually asks for.
# Command-submission constants for the scanout-render arm. Values read out of
# /opt/homebrew/include/vulkan/vulkan_core.h, not recalled.
ST_SUBMIT_INFO = 4
ST_COMMAND_POOL_CREATE_INFO = 39
ST_COMMAND_BUFFER_ALLOCATE_INFO = 40
ST_COMMAND_BUFFER_BEGIN_INFO = 42
ST_IMAGE_MEMORY_BARRIER = 45
LAYOUT_UNDEFINED = 0
LAYOUT_GENERAL = 1
LAYOUT_TRANSFER_DST_OPTIMAL = 7
STAGE_TOP_OF_PIPE = 0x1
STAGE_TRANSFER = 0x1000
ACCESS_TRANSFER_WRITE = 0x1000
ASPECT_COLOR = 0x1
USAGE_TRANSFER_DST = 0x2
CMD_LEVEL_PRIMARY = 0
ST_FENCE_CREATE_INFO = 8

HANDLE_DMA_BUF = 0x200
ST_EXPORT_MEMORY_ALLOCATE_INFO = 1000072002
ST_MEMORY_DEDICATED_ALLOCATE_INFO = 1000127001
SCANOUT_EXTS = (b"VK_KHR_external_memory", b"VK_EXT_external_memory_dma_buf",
                b"VK_EXT_image_drm_format_modifier", b"VK_KHR_external_memory_fd")


def load_vk():
    vk = C.CDLL("libvulkan.so.1")
    for name, args in [
        ("vkCreateInstance", [P, P, P]),
        ("vkEnumeratePhysicalDevices", [P, P, P]),
        ("vkGetPhysicalDeviceProperties", [P, P]),
        ("vkCreateDevice", [P, P, P, P]),
        ("vkAllocateMemory", [P, P, P, P]),
        ("vkFreeMemory", [P, H, P]),
        ("vkCreateImage", [P, P, P, P]),
        ("vkDestroyImage", [P, H, P]),
        ("vkGetImageMemoryRequirements", [P, H, P]),
        ("vkBindImageMemory", [P, H, H, C.c_uint64]),
    ]:
        f = getattr(vk, name)
        f.argtypes = args
        f.restype = C.c_int
    return vk


def make_instance(vk):
    app = ApplicationInfo()
    app.sType = 0
    app.apiVersion = API_1_1
    ici = InstanceCreateInfo()
    ici.sType = 1
    ici.pApplicationInfo = vp(app)
    inst = C.c_void_p()
    return vk.vkCreateInstance(C.byref(ici), None, C.byref(inst)), inst


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


def make_device(vk, phys, exts=()):
    prio = C.c_float(1.0)
    qci = DeviceQueueCreateInfo()
    qci.sType = 2
    qci.queueFamilyIndex = 0
    qci.queueCount = 1
    qci.pQueuePriorities = C.pointer(prio)
    dci = DeviceCreateInfo()
    dci.sType = 3
    dci.queueCreateInfoCount = 1
    dci.pQueueCreateInfos = vp(qci)
    names = None
    if exts:
        names = (C.c_char_p * len(exts))(*exts)
        dci.enabledExtensionCount = len(exts)
        dci.ppEnabledExtensionNames = C.cast(names, C.c_void_p)
    dev = C.c_void_p()
    r = vk.vkCreateDevice(phys, C.byref(dci), None, C.byref(dev))
    if r != VK_SUCCESS:
        print(f"CHURN FAIL vkCreateDevice {r}", flush=True)
        return None
    return dev


def alloc(vk, dev, size, type_index=0, dedicated_image=None):
    ai = MemoryAllocateInfo()
    ai.sType = 5
    ai.allocationSize = size
    ai.memoryTypeIndex = type_index
    # Keep the chained structs referenced until the call returns.
    keep = []
    if dedicated_image is not None:
        # WITHOUT these two, venus SUBALLOCATES the memory guest-side and the host allocator
        # is never reached — measured 2026-08-07: 20 vkAllocateMemory calls produced ZERO
        # host device-memory charges while synoik's ledger row showed real ones. Exporting
        # and dedicating is what a compositor's scanout buffer does, and it is what forces
        # a real host allocation.
        exp = ExportMemoryAllocateInfo()
        exp.sType = ST_EXPORT_MEMORY_ALLOCATE_INFO
        exp.handleTypes = HANDLE_DMA_BUF
        ded = MemoryDedicatedAllocateInfo()
        ded.sType = ST_MEMORY_DEDICATED_ALLOCATE_INFO
        ded.image = dedicated_image
        ded.buffer = 0
        ded.pNext = vp(exp)
        ai.pNext = vp(ded)
        keep = [exp, ded]
    mem = C.c_uint64()
    r = vk.vkAllocateMemory(dev, C.byref(ai), None, C.byref(mem))
    del keep
    return r, mem.value


def make_image(vk, dev, w, h, external=False):
    ci = ImageCreateInfo()
    ci.sType = 14
    ext = None
    modlist = None
    mods = None
    if external:
        # Two structs, because one is not enough. VkExternalMemoryImageCreateInfo alone gets
        # vkr's attention but leaves the image OPTIMAL-tiled, and a 1000-image run charged
        # exactly ONE IOSurface — the allocation-side scanout path was never entered. What a
        # compositor actually does, and what reaches vkr_image.c's IOSurface allocation, is a
        # DRM_FORMAT_MODIFIER-tiled image with a modifier LIST (the LIST form marks an
        # allocation; the EXPLICIT form would mark an import and deliberately skips it).
        # All three stay alive until vkCreateImage returns.
        mods = (C.c_uint64 * 1)(DRM_FORMAT_MOD_LINEAR)
        modlist = ImageDrmFormatModifierListCreateInfoEXT()
        modlist.sType = ST_MODIFIER_LIST
        modlist.drmFormatModifierCount = 1
        modlist.pDrmFormatModifiers = mods
        ext = ExternalMemoryImageCreateInfo()
        ext.sType = ST_EXTERNAL_MEMORY_IMAGE
        ext.handleTypes = HANDLE_DMA_BUF
        ext.pNext = vp(modlist)
        ci.pNext = vp(ext)
    ci.imageType = 1                      # VK_IMAGE_TYPE_2D
    ci.format = 44                        # VK_FORMAT_B8G8R8A8_UNORM
    ci.extent = Extent3D(w, h, 1)
    ci.mipLevels = 1
    ci.arrayLayers = 1
    ci.samples = 1
    # Set here, AFTER the external branch above, because the field defaults run last and
    # would otherwise silently stomp the modifier tiling back to OPTIMAL.
    ci.tiling = TILING_DRM_MODIFIER if external else 0
    # TRANSFER_DST is required by vkCmdClearColorImage; without it the clear in the
    # scanout-render arm segfaults inside venus rather than failing cleanly.
    ci.usage = 0x10 | 0x1 | USAGE_TRANSFER_DST  # COLOR_ATTACHMENT | TRANSFER_SRC | TRANSFER_DST
    ci.sharingMode = 0
    ci.initialLayout = 0
    img = C.c_uint64()
    r = vk.vkCreateImage(dev, C.byref(ci), None, C.byref(img))
    return r, img.value


class CommandPoolCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pad", C.c_uint32), ("pNext", P),
                ("flags", C.c_uint32), ("queueFamilyIndex", C.c_uint32)]


class CommandBufferAllocateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pad", C.c_uint32), ("pNext", P),
                ("commandPool", H), ("level", C.c_uint32),
                ("commandBufferCount", C.c_uint32)]


class CommandBufferBeginInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pad", C.c_uint32), ("pNext", P),
                ("flags", C.c_uint32), ("pad2", C.c_uint32),
                ("pInheritanceInfo", P)]


class ImageSubresourceRange(C.Structure):
    _fields_ = [("aspectMask", C.c_uint32), ("baseMipLevel", C.c_uint32),
                ("levelCount", C.c_uint32), ("baseArrayLayer", C.c_uint32),
                ("layerCount", C.c_uint32)]


class ImageMemoryBarrier(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pad", C.c_uint32), ("pNext", P),
                ("srcAccessMask", C.c_uint32), ("dstAccessMask", C.c_uint32),
                ("oldLayout", C.c_uint32), ("newLayout", C.c_uint32),
                ("srcQueueFamilyIndex", C.c_uint32), ("dstQueueFamilyIndex", C.c_uint32),
                ("image", H), ("subresourceRange", ImageSubresourceRange)]


class ClearColorValue(C.Union):
    _fields_ = [("float32", C.c_float * 4), ("int32", C.c_int32 * 4),
                ("uint32", C.c_uint32 * 4)]


class SubmitInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pad", C.c_uint32), ("pNext", P),
                ("waitSemaphoreCount", C.c_uint32), ("pad2", C.c_uint32),
                ("pWaitSemaphores", P), ("pWaitDstStageMask", P),
                ("commandBufferCount", C.c_uint32), ("pad3", C.c_uint32),
                ("pCommandBuffers", P),
                ("signalSemaphoreCount", C.c_uint32), ("pad4", C.c_uint32),
                ("pSignalSemaphores", P)]


def make_recorder(vk, dev):
    """Queue + command pool + one reusable primary command buffer.

    This exists so a scanout surface can be TOUCHED BY THE GPU before it is destroyed. The
    2026-08-07 sentinel run proved every Vulkan/Metal object of ours dies and the kernel
    IOSurface survives anyway, so the suspect is a kernel reference taken on USE. A clear is
    the cheapest possible use: no pipeline, no shaders, no render pass — and it is a real
    write, so the pages become resident (the earlier probe never wrote, which is why its
    footprint stayed flat and its reclaim result said nothing about pages).
    """
    # Prototype the calls that take a 64-bit non-dispatchable handle DIRECTLY as an argument.
    # Without argtypes ctypes passes a Python int as a C int, and the callee reads 64 bits —
    # the upper half is then whatever was in the register. Handles passed inside a struct are
    # unaffected, which is why the barrier survives and only the clear crashes.
    vk.vkCmdClearColorImage.argtypes = [C.c_void_p, C.c_uint64, C.c_uint32, P, C.c_uint32, P]
    vk.vkCmdClearColorImage.restype = None
    vk.vkCmdPipelineBarrier.argtypes = [C.c_void_p, C.c_uint32, C.c_uint32, C.c_uint32,
                                        C.c_uint32, P, C.c_uint32, P, C.c_uint32, P]
    vk.vkCmdPipelineBarrier.restype = None

    vk.vkWaitForFences.argtypes = [C.c_void_p, C.c_uint32, P, C.c_uint32, C.c_uint64]
    vk.vkWaitForFences.restype = C.c_int
    vk.vkResetFences.argtypes = [C.c_void_p, C.c_uint32, P]
    vk.vkResetFences.restype = C.c_int
    vk.vkQueueSubmit.argtypes = [C.c_void_p, C.c_uint32, P, C.c_uint64]
    vk.vkQueueSubmit.restype = C.c_int

    queue = C.c_void_p()
    vk.vkGetDeviceQueue(dev, 0, 0, C.byref(queue))

    pci = CommandPoolCreateInfo()
    pci.sType = ST_COMMAND_POOL_CREATE_INFO
    pci.flags = 0x2  # RESET_COMMAND_BUFFER
    pci.queueFamilyIndex = 0
    pool = C.c_uint64()
    r = vk.vkCreateCommandPool(dev, C.byref(pci), None, C.byref(pool))
    if r != VK_SUCCESS:
        print(f"CHURN FAIL vkCreateCommandPool {r}", flush=True)
        return None

    cai = CommandBufferAllocateInfo()
    cai.sType = ST_COMMAND_BUFFER_ALLOCATE_INFO
    cai.commandPool = pool.value
    cai.level = CMD_LEVEL_PRIMARY
    cai.commandBufferCount = 1
    cmd = C.c_void_p()
    r = vk.vkAllocateCommandBuffers(dev, C.byref(cai), C.byref(cmd))
    if r != VK_SUCCESS:
        print(f"CHURN FAIL vkAllocateCommandBuffers {r}", flush=True)
        return None

    # A fence, for the arm that waits the way a compositor waits. vkQueueWaitIdle drains the
    # WHOLE queue, which is a thing no compositor ever does and which may well be what runs
    # the driver's deferred-release work. A fence waits for this submission only, and leaves
    # the queue busy.
    fci = FenceCreateInfo()
    fci.sType = ST_FENCE_CREATE_INFO
    fence = C.c_uint64()
    r = vk.vkCreateFence(dev, C.byref(fci), None, C.byref(fence))
    if r != VK_SUCCESS:
        print(f"CHURN FAIL vkCreateFence {r}", flush=True)
        return None
    return queue, pool.value, cmd, fence.value


class FenceCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pad", C.c_uint32), ("pNext", P),
                ("flags", C.c_uint32), ("pad2", C.c_uint32)]


def gpu_touch(vk, dev, rec, img, i, use_fence=False):
    """Transition UNDEFINED -> GENERAL and clear the image, then wait for the GPU.

    Waiting matters: an outstanding submission would be an obvious alternative explanation
    for a surface that has not been released yet, and this probe exists to remove exactly
    that kind of confounder.
    """
    queue, _pool, cmd, fence = rec
    bi = CommandBufferBeginInfo()
    bi.sType = ST_COMMAND_BUFFER_BEGIN_INFO
    bi.flags = 0x1  # ONE_TIME_SUBMIT
    r = vk.vkBeginCommandBuffer(cmd, C.byref(bi))
    if r != VK_SUCCESS:
        print(f"CHURN FAIL vkBeginCommandBuffer {r} at {i}", flush=True)
        return False

    rng = ImageSubresourceRange(ASPECT_COLOR, 0, 1, 0, 1)
    bar = ImageMemoryBarrier()
    bar.sType = ST_IMAGE_MEMORY_BARRIER
    bar.srcAccessMask = 0
    bar.dstAccessMask = ACCESS_TRANSFER_WRITE
    bar.oldLayout = LAYOUT_UNDEFINED
    bar.newLayout = LAYOUT_GENERAL
    bar.srcQueueFamilyIndex = 0xFFFFFFFF  # VK_QUEUE_FAMILY_IGNORED
    bar.dstQueueFamilyIndex = 0xFFFFFFFF
    bar.image = img
    bar.subresourceRange = rng
    vk.vkCmdPipelineBarrier(cmd, STAGE_TOP_OF_PIPE, STAGE_TRANSFER, 0,
                            0, None, 0, None, 1, C.byref(bar))

    col = ClearColorValue()
    col.float32 = (C.c_float * 4)(0.1, 0.4, 0.9, 1.0)
    vk.vkCmdClearColorImage(cmd, img, LAYOUT_GENERAL, C.byref(col), 1, C.byref(rng))

    r = vk.vkEndCommandBuffer(cmd)
    if r != VK_SUCCESS:
        print(f"CHURN FAIL vkEndCommandBuffer {r} at {i}", flush=True)
        return False

    cmds = (C.c_void_p * 1)(cmd)
    si = SubmitInfo()
    si.sType = ST_SUBMIT_INFO
    si.commandBufferCount = 1
    si.pCommandBuffers = C.cast(cmds, C.c_void_p)
    r = vk.vkQueueSubmit(queue, 1, C.byref(si), fence if use_fence else 0)
    if r != VK_SUCCESS:
        print(f"CHURN FAIL vkQueueSubmit {r} at {i}", flush=True)
        return False
    if use_fence:
        fences = (C.c_uint64 * 1)(fence)
        r = vk.vkWaitForFences(dev, 1, C.cast(fences, P), 1, 5_000_000_000)
        if r != VK_SUCCESS:
            print(f"CHURN FAIL vkWaitForFences {r} at {i}", flush=True)
            return False
        vk.vkResetFences(dev, 1, C.cast(fences, P))
    else:
        r = vk.vkQueueWaitIdle(queue)
        if r != VK_SUCCESS:
            print(f"CHURN FAIL vkQueueWaitIdle {r} at {i}", flush=True)
            return False
    return True


def first_type(bits):
    for i in range(32):
        if bits & (1 << i):
            return i
    return 0


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else "Venus"
    mode = sys.argv[2] if len(sys.argv) > 2 else "mem"
    iters = int(sys.argv[3]) if len(sys.argv) > 3 else 1000
    live_n = int(sys.argv[4]) if len(sys.argv) > 4 else 4
    w = int(sys.argv[5]) if len(sys.argv) > 5 else 3840
    h = int(sys.argv[6]) if len(sys.argv) > 6 else 2160
    size = w * h * 4

    vk = load_vk()
    r, inst = make_instance(vk)
    if r != VK_SUCCESS:
        print(f"CHURN FAIL vkCreateInstance {r}", flush=True)
        return
    phys = pick_device(vk, inst, want)
    if not phys:
        print(f"NODEV {want}", flush=True)
        return
    dev = make_device(vk, phys, SCANOUT_EXTS if mode.startswith("scanout") else ())
    if not dev:
        return

    print(f"START {mode} {iters} x {size} live={live_n}", flush=True)

    if mode in ("scanout-bind", "scanout-render", "scanout-render-fence"):
        # THE A/B THAT ISOLATES "USE". Both arms create a scanout image, bind dedicated
        # exported memory, then destroy the image AND free the memory — a clean, complete
        # release every iteration, with a live set of one. They differ in exactly one thing:
        # scanout-render clears the image on the GPU first and waits for it.
        #
        # If scanout-render retains and scanout-bind does not, the kernel reference that
        # outlives our CFRelease is taken when the surface is USED, and the storm is
        # reproduced with no compositor, no seated session and no 50 GB/min.
        render = mode in ("scanout-render", "scanout-render-fence")
        use_fence = mode == "scanout-render-fence"
        rec = make_recorder(vk, dev) if render else None
        if render and rec is None:
            return
        made = 0
        for i in range(iters):
            r, img = make_image(vk, dev, w, h, external=True)
            if r != VK_SUCCESS:
                print(f"CHURN FAIL vkCreateImage {r} at {i}", flush=True)
                break
            req = MemoryRequirements()
            vk.vkGetImageMemoryRequirements(dev, img, C.byref(req))
            r, mem = alloc(vk, dev, req.size, first_type(req.memoryTypeBits),
                           dedicated_image=img)
            if r != VK_SUCCESS:
                print(f"CHURN FAIL vkAllocateMemory {r} at {i}", flush=True)
                vk.vkDestroyImage(dev, img, None)
                break
            vk.vkBindImageMemory(dev, img, mem, 0)
            if render and not gpu_touch(vk, dev, rec, img, i, use_fence):
                vk.vkDestroyImage(dev, img, None)
                vk.vkFreeMemory(dev, mem, None)
                break
            vk.vkDestroyImage(dev, img, None)
            vk.vkFreeMemory(dev, mem, None)
            made += 1
            if made % 20 == 0:
                print(f"PROGRESS {made}", flush=True)
        print(f"CHURN DONE {mode} {made} {size}", flush=True)
        # Hold the context open: the host census must be read against a LIVE context, and the
        # process must be SIGKILL-able to ask whether teardown gives anything back.
        print(f"HOLDING pid={os.getpid()}", flush=True)
        sys.stdout.flush()
        while True:
            time.sleep(3600)

    if mode in ("scanout", "scanout-leak"):
        # Create and destroy immediately — the live set is 1. Any host-side growth is
        # therefore retention by construction, with no bookkeeping to argue about.
        #
        # scanout-leak additionally binds a VkDeviceMemory and NEVER frees it, which is the
        # guest-side compositor bug in miniature. It matters because the host's +1 on the
        # IOSurface lives on the MEMORY object (vkr_device_memory.c's imported_iosurface),
        # not the image: plain `scanout` destroys the image, the host credits the ledger,
        # and nothing is retained — measured, 20 charges / 281.2 MiB / clean teardown. So
        # `scanout` is the negative control and `scanout-leak` is the reproduction.
        leak = mode == "scanout-leak"
        made = 0
        held = []
        for i in range(iters):
            r, img = make_image(vk, dev, w, h, external=True)
            if r != VK_SUCCESS:
                print(f"CHURN FAIL vkCreateImage {r} at {i}", flush=True)
                break
            if leak:
                req = MemoryRequirements()
                vk.vkGetImageMemoryRequirements(dev, img, C.byref(req))
                r, mem = alloc(vk, dev, req.size, first_type(req.memoryTypeBits),
                               dedicated_image=img)
                if r != VK_SUCCESS:
                    print(f"CHURN FAIL vkAllocateMemory {r} at {i}", flush=True)
                    vk.vkDestroyImage(dev, img, None)
                    break
                vk.vkBindImageMemory(dev, img, mem, 0)
                # Keep the handle so nothing can argue the guest still owns it, and never
                # free it. The image goes away; the memory does not.
                held.append(mem)
            vk.vkDestroyImage(dev, img, None)
            made += 1
            if made % 200 == 0:
                print(f"PROGRESS {made}", flush=True)
        print(f"CHURN DONE {mode} {made} {size} leaked={len(held)}", flush=True)
        if leak:
            # Stay alive so the host retention can be measured against a live context, and
            # so the process can be SIGKILLed to test whether teardown reclaims it.
            print(f"HOLDING pid={os.getpid()}", flush=True)
            sys.stdout.flush()
            while True:
                time.sleep(3600)
        return

    # A bounded ring of live objects: allocate the new one, then drop the oldest. The live
    # set never exceeds live_n, exactly like a compositor's swapchain.
    ring = []
    done = 0
    for i in range(iters):
        if mode == "image":
            r, img = make_image(vk, dev, w, h)
            if r != VK_SUCCESS:
                print(f"CHURN FAIL vkCreateImage {r} at {i}", flush=True)
                break
            req = MemoryRequirements()
            vk.vkGetImageMemoryRequirements(dev, img, C.byref(req))
            r, mem = alloc(vk, dev, req.size, first_type(req.memoryTypeBits))
            if r != VK_SUCCESS:
                print(f"CHURN FAIL vkAllocateMemory {r} at {i}", flush=True)
                vk.vkDestroyImage(dev, img, None)
                break
            vk.vkBindImageMemory(dev, img, mem, 0)
            ring.append((img, mem))
        else:
            r, mem = alloc(vk, dev, size)
            if r != VK_SUCCESS:
                print(f"CHURN FAIL vkAllocateMemory {r} at {i}", flush=True)
                break
            ring.append((None, mem))

        done += 1
        if len(ring) > live_n:
            img, mem = ring.pop(0)
            if img:
                vk.vkDestroyImage(dev, img, None)
            vk.vkFreeMemory(dev, mem, None)
        if done % 200 == 0:
            print(f"PROGRESS {done}", flush=True)

    for img, mem in ring:
        if img:
            vk.vkDestroyImage(dev, img, None)
        vk.vkFreeMemory(dev, mem, None)

    print(f"CHURN DONE {mode} {done} {size}", flush=True)


if __name__ == "__main__":
    main()
