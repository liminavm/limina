#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkclearrect — guest vehicle for the empty-VkClearRect host-VMM-abort guard.

A guest venus stream can encode vkCmdClearAttachments with a zero-extent
VkClearRect (rect.extent = {0,0}, invalid usage per
VUID-vkCmdClearAttachments-rect-02682/-02683, but guest-controlled). It flows
guest-mesa-venus (encode, no validation) -> host virglrenderer vkr (decode) ->
KosmicKrisp, which replays it at vkQueueSubmit into vk_meta_clear_attachments ->
setup_viewport_scissor, whose assert(x0 < x1 && y0 < y1) aborts the whole
worker/VMM process (a guest-triggerable host DoS).

This vehicle reproduces exactly that from inside the guest, over the guest's
venus ICD: a 64x64 color-attachment render pass, then ONE vkCmdClearAttachments
with a VALID sub-rect {8,8 16x16} + an EMPTY rect {0,0 0x0} (the poison), then
submit + wait. It clears no real work otherwise.

Oracle is host-side (crates/limina-test/tests/venus.rs):
  RED  (unfixed host stack): the worker dies with signal 6 during the submit.
  GREEN (fixed: vkr Fix A drops the rect / KK Fix B skips it): the vehicle prints
        CLEARRECT PASS and exits 0, and the worker/VM stays healthy.

Reuses vkfdcycle.py (staged next to it in /tmp) for the ctypes plumbing.
Speaks to libvulkan.so.1 directly (no vulkan-tools / gcc on the guest).
Structured output lines the harness asserts on:
    INSTANCE OK | INSTANCE ERR <r>
    DEVICE <name> / NODEV <substring>
    CLEARRECT FAIL <stage> <r>
    CLEARRECT SUBMIT      (emitted right before the poisoned submit)
    CLEARRECT PASS

Usage: vkclearrect.py [device-substring]   (default: Venus)
"""
import ctypes as C
import sys

sys.path.insert(0, "/tmp")
from vkfdcycle import (  # noqa: E402
    VK_SUCCESS,
    H,
    MemoryAllocateInfo,
    MemoryRequirements,
    P,
    first_type,
    load,
    make_instance,
    pick_device,
    vp,
)

# --- structs not in vkfdcycle -------------------------------------------------


class Extent3D(C.Structure):
    _fields_ = [("width", C.c_uint32), ("height", C.c_uint32), ("depth", C.c_uint32)]


class ImageCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 14
        ("pNext", P),
        ("flags", C.c_uint32),
        ("imageType", C.c_uint32),  # 1 = 2D
        ("format", C.c_uint32),  # 37 = R8G8B8A8_UNORM
        ("extent", Extent3D),
        ("mipLevels", C.c_uint32),
        ("arrayLayers", C.c_uint32),
        ("samples", C.c_uint32),  # 1
        ("tiling", C.c_uint32),  # 0 = OPTIMAL
        ("usage", C.c_uint32),  # 0x10 = COLOR_ATTACHMENT
        ("sharingMode", C.c_uint32),
        ("queueFamilyIndexCount", C.c_uint32),
        ("pQueueFamilyIndices", P),
        ("initialLayout", C.c_uint32),  # 0 = UNDEFINED
    ]


class ComponentMapping(C.Structure):
    _fields_ = [("r", C.c_uint32), ("g", C.c_uint32), ("b", C.c_uint32), ("a", C.c_uint32)]


class ImageSubresourceRange(C.Structure):
    _fields_ = [
        ("aspectMask", C.c_uint32),  # 1 = COLOR
        ("baseMipLevel", C.c_uint32),
        ("levelCount", C.c_uint32),
        ("baseArrayLayer", C.c_uint32),
        ("layerCount", C.c_uint32),
    ]


class ImageViewCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 15
        ("pNext", P),
        ("flags", C.c_uint32),
        ("image", H),
        ("viewType", C.c_uint32),  # 1 = 2D
        ("format", C.c_uint32),
        ("components", ComponentMapping),
        ("subresourceRange", ImageSubresourceRange),
    ]


class AttachmentDescription(C.Structure):
    _fields_ = [
        ("flags", C.c_uint32),
        ("format", C.c_uint32),
        ("samples", C.c_uint32),  # 1
        ("loadOp", C.c_uint32),  # 1 = CLEAR
        ("storeOp", C.c_uint32),  # 0 = STORE
        ("stencilLoadOp", C.c_uint32),  # 2 = DONT_CARE
        ("stencilStoreOp", C.c_uint32),  # 1 = DONT_CARE
        ("initialLayout", C.c_uint32),  # 0 = UNDEFINED
        ("finalLayout", C.c_uint32),  # 2 = COLOR_ATTACHMENT_OPTIMAL
    ]


class AttachmentReference(C.Structure):
    _fields_ = [("attachment", C.c_uint32), ("layout", C.c_uint32)]


class SubpassDescription(C.Structure):
    _fields_ = [
        ("flags", C.c_uint32),
        ("pipelineBindPoint", C.c_uint32),  # 0 = GRAPHICS
        ("inputAttachmentCount", C.c_uint32),
        ("pInputAttachments", P),
        ("colorAttachmentCount", C.c_uint32),
        ("pColorAttachments", P),
        ("pResolveAttachments", P),
        ("pDepthStencilAttachment", P),
        ("preserveAttachmentCount", C.c_uint32),
        ("pPreserveAttachments", P),
    ]


class RenderPassCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 38
        ("pNext", P),
        ("flags", C.c_uint32),
        ("attachmentCount", C.c_uint32),
        ("pAttachments", P),
        ("subpassCount", C.c_uint32),
        ("pSubpasses", P),
        ("dependencyCount", C.c_uint32),
        ("pDependencies", P),
    ]


class FramebufferCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 37
        ("pNext", P),
        ("flags", C.c_uint32),
        ("renderPass", H),
        ("attachmentCount", C.c_uint32),
        ("pAttachments", P),
        ("width", C.c_uint32),
        ("height", C.c_uint32),
        ("layers", C.c_uint32),
    ]


class Offset2D(C.Structure):
    _fields_ = [("x", C.c_int32), ("y", C.c_int32)]


class Extent2D(C.Structure):
    _fields_ = [("width", C.c_uint32), ("height", C.c_uint32)]


class Rect2D(C.Structure):
    _fields_ = [("offset", Offset2D), ("extent", Extent2D)]


class ClearColorValue(C.Structure):
    _fields_ = [("float32", C.c_float * 4)]


class ClearValue(C.Structure):
    # union { VkClearColorValue color; VkClearDepthStencilValue ds; } — 16 bytes.
    _fields_ = [("color", ClearColorValue)]


class RenderPassBeginInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 43
        ("pNext", P),
        ("renderPass", H),
        ("framebuffer", H),
        ("renderArea", Rect2D),
        ("clearValueCount", C.c_uint32),
        ("pClearValues", P),
    ]


class ClearAttachment(C.Structure):
    _fields_ = [
        ("aspectMask", C.c_uint32),  # 1 = COLOR
        ("colorAttachment", C.c_uint32),
        ("clearValue", ClearValue),
    ]


class ClearRect(C.Structure):
    _fields_ = [
        ("rect", Rect2D),
        ("baseArrayLayer", C.c_uint32),
        ("layerCount", C.c_uint32),
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
    vk.vkCreateImage.argtypes = [P, P, P, P]
    vk.vkGetImageMemoryRequirements.argtypes = [P, H, P]
    vk.vkBindImageMemory.argtypes = [P, H, H, C.c_uint64]
    vk.vkCreateImageView.argtypes = [P, P, P, P]
    vk.vkCreateRenderPass.argtypes = [P, P, P, P]
    vk.vkCreateFramebuffer.argtypes = [P, P, P, P]
    vk.vkCreateCommandPool.argtypes = [P, P, P, P]
    vk.vkAllocateCommandBuffers.argtypes = [P, P, P]
    vk.vkBeginCommandBuffer.argtypes = [P, P]
    vk.vkCmdBeginRenderPass.argtypes = [P, P, C.c_uint32]
    vk.vkCmdClearAttachments.argtypes = [P, C.c_uint32, P, C.c_uint32, P]
    vk.vkCmdEndRenderPass.argtypes = [P]
    vk.vkEndCommandBuffer.argtypes = [P]
    vk.vkQueueSubmit.argtypes = [P, C.c_uint32, P, H]
    vk.vkQueueWaitIdle.argtypes = [P]
    for f in (
        vk.vkCreateImage,
        vk.vkCreateImageView,
        vk.vkCreateRenderPass,
        vk.vkCreateFramebuffer,
        vk.vkCreateCommandPool,
        vk.vkAllocateCommandBuffers,
        vk.vkBeginCommandBuffer,
        vk.vkBindImageMemory,
        vk.vkEndCommandBuffer,
        vk.vkQueueSubmit,
        vk.vkQueueWaitIdle,
    ):
        f.restype = C.c_int


def make_basic_device(vk, phys):
    """A plain graphics device — no extensions, no features (the clear path is core 1.0)."""
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
    dev = C.c_void_p()
    r = vk.vkCreateDevice(phys, C.byref(dci), None, C.byref(dev))
    if r != VK_SUCCESS:
        print(f"CLEARRECT FAIL vkCreateDevice {r}", flush=True)
        return None
    return dev


W = H_ = 64
RGBA8 = 37
COLOR = 1


def run_clear(vk, dev):
    """Build a 64x64 color render pass and issue the poisoned vkCmdClearAttachments."""
    # image
    ici = ImageCreateInfo()
    ici.sType = 14
    ici.imageType = 1
    ici.format = RGBA8
    ici.extent = Extent3D(W, H_, 1)
    ici.mipLevels = 1
    ici.arrayLayers = 1
    ici.samples = 1
    ici.tiling = 0
    ici.usage = 0x10  # COLOR_ATTACHMENT
    ici.initialLayout = 0
    image = H()
    r = vk.vkCreateImage(dev, C.byref(ici), None, C.byref(image))
    if r != VK_SUCCESS:
        return f"vkCreateImage {r}"

    req = MemoryRequirements()
    vk.vkGetImageMemoryRequirements(dev, image, C.byref(req))
    mai = MemoryAllocateInfo()
    mai.sType = 5
    mai.allocationSize = req.size
    mai.memoryTypeIndex = first_type(req.memoryTypeBits)
    mem = H()
    r = vk.vkAllocateMemory(dev, C.byref(mai), None, C.byref(mem))
    if r != VK_SUCCESS:
        return f"vkAllocateMemory {r}"
    r = vk.vkBindImageMemory(dev, image, mem, 0)
    if r != VK_SUCCESS:
        return f"vkBindImageMemory {r}"

    # view
    vci = ImageViewCreateInfo()
    vci.sType = 15
    vci.image = image.value
    vci.viewType = 1
    vci.format = RGBA8
    vci.subresourceRange = ImageSubresourceRange(COLOR, 0, 1, 0, 1)
    view = H()
    r = vk.vkCreateImageView(dev, C.byref(vci), None, C.byref(view))
    if r != VK_SUCCESS:
        return f"vkCreateImageView {r}"

    # render pass: single color attachment, loadOp=CLEAR
    att = AttachmentDescription()
    att.format = RGBA8
    att.samples = 1
    att.loadOp = 1  # CLEAR
    att.storeOp = 0  # STORE
    att.stencilLoadOp = 2  # DONT_CARE
    att.stencilStoreOp = 1  # DONT_CARE
    att.initialLayout = 0  # UNDEFINED
    att.finalLayout = 2  # COLOR_ATTACHMENT_OPTIMAL
    cref = AttachmentReference(0, 2)  # COLOR_ATTACHMENT_OPTIMAL
    sub = SubpassDescription()
    sub.pipelineBindPoint = 0
    sub.colorAttachmentCount = 1
    sub.pColorAttachments = vp(cref)
    rpci = RenderPassCreateInfo()
    rpci.sType = 38
    rpci.attachmentCount = 1
    rpci.pAttachments = vp(att)
    rpci.subpassCount = 1
    rpci.pSubpasses = vp(sub)
    rp = H()
    r = vk.vkCreateRenderPass(dev, C.byref(rpci), None, C.byref(rp))
    if r != VK_SUCCESS:
        return f"vkCreateRenderPass {r}"

    # framebuffer
    view_arr = (H * 1)(view.value)
    fbci = FramebufferCreateInfo()
    fbci.sType = 37
    fbci.renderPass = rp.value
    fbci.attachmentCount = 1
    fbci.pAttachments = C.cast(view_arr, C.c_void_p)
    fbci.width = W
    fbci.height = H_
    fbci.layers = 1
    fb = H()
    r = vk.vkCreateFramebuffer(dev, C.byref(fbci), None, C.byref(fb))
    if r != VK_SUCCESS:
        return f"vkCreateFramebuffer {r}"

    # command buffer
    queue = C.c_void_p()
    vk.vkGetDeviceQueue(dev, 0, 0, C.byref(queue))
    cpci = CommandPoolCreateInfo()
    cpci.sType = 39
    cpci.queueFamilyIndex = 0
    pool = H()
    r = vk.vkCreateCommandPool(dev, C.byref(cpci), None, C.byref(pool))
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

    clearv = ClearValue()
    clearv.color.float32[:] = [0.0, 0.0, 0.0, 1.0]
    rbi = RenderPassBeginInfo()
    rbi.sType = 43
    rbi.renderPass = rp.value
    rbi.framebuffer = fb.value
    rbi.renderArea = Rect2D(Offset2D(0, 0), Extent2D(W, H_))
    rbi.clearValueCount = 1
    rbi.pClearValues = vp(clearv)
    vk.vkCmdBeginRenderPass(cmd, C.byref(rbi), 0)  # CONTENTS_INLINE

    # THE POISON: one valid rect + one empty rect in the same clear call.
    clear_att = ClearAttachment()
    clear_att.aspectMask = COLOR
    clear_att.colorAttachment = 0
    clear_att.clearValue.color.float32[:] = [1.0, 0.0, 0.0, 1.0]  # red
    rects = (ClearRect * 2)(
        ClearRect(Rect2D(Offset2D(8, 8), Extent2D(16, 16)), 0, 1),  # valid
        ClearRect(Rect2D(Offset2D(0, 0), Extent2D(0, 0)), 0, 1),    # empty
    )
    vk.vkCmdClearAttachments(cmd, 1, C.byref(clear_att), 2, rects)

    vk.vkCmdEndRenderPass(cmd)
    r = vk.vkEndCommandBuffer(cmd)
    if r != VK_SUCCESS:
        return f"vkEndCommandBuffer {r}"

    si = SubmitInfo()
    si.sType = 4
    si.commandBufferCount = 1
    si.pCommandBuffers = C.cast(C.byref(cmd), C.c_void_p)
    print("CLEARRECT SUBMIT", flush=True)  # the worker replays KK's clear here
    r = vk.vkQueueSubmit(queue, 1, C.byref(si), 0)
    if r != VK_SUCCESS:
        return f"vkQueueSubmit {r}"
    r = vk.vkQueueWaitIdle(queue)
    if r != VK_SUCCESS:
        return f"vkQueueWaitIdle {r}"
    return None


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
    dev = make_basic_device(vk, phys)
    if dev is None:
        return

    err = run_clear(vk, dev)
    if err:
        print(f"CLEARRECT FAIL {err}", flush=True)
        return
    print("CLEARRECT PASS", flush=True)


if __name__ == "__main__":
    main()
