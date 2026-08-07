#!/usr/bin/env python3
"""kmschurn — a Vulkan-backed KMS presenter that churns scanout resources.

The vehicle for the 2026-08-07 host-retention bug and its regression test. That bug
(limina `8e00d94`) was an unbounded frame-apply surface cache in the SUPERVISOR: it
retained one whole framebuffer per distinct scanout id it had ever seen, and a
compositor that mints a **fresh scanout buffer every frame** hands it a fresh id every
frame. ~620 surfaces x 14.1 MiB = 8.6 GB in ten seconds.

So the reproducing shape is not "a compositor" — it is precisely **a fresh scanout
resource per flip**, and that is all this presents. It is deliberately the smallest
thing that is still the real path: real gbm allocation, real `drmModeAddFB2`, real
page flips through the guest's virtio-gpu KMS driver, all the way to `SET_SCANOUT_BLOB`
on the host.

Why gbm: synoik (the compositor that found the bug) ran with
`GALLIUM_DRIVER=zink` + `MESA_LOADER_DRIVER_OVERRIDE=zink`, so its gbm buffer objects
ARE venus blobs — gbm is the allocation path and venus is what backs it. Imitating that
means calling gbm, not assembling `VkImage` + `vkGetMemoryFdKHR` by hand. **Run this with
the same three env vars synoik has** (`GALLIUM_DRIVER`, `MESA_LOADER_DRIVER_OVERRIDE`,
`VK_DRIVER_FILES`); without them gbm loads a different gallium driver, the buffers are
not venus blobs, and the probe silently tests a path the bug never lived on. It reports
the env it saw so a log can be audited for that.

Why also raw Vulkan (`-vk`): that env coupling is exactly what synoik moved AWAY from —
since `95c306bf` it allocates its scanout images in venus directly, per its own
`scanout-buffers-via-vulkan.md`, and never calls gbm. The `-vk` modes are that path,
step for step, and they swap the ALLOCATOR ONLY: same modeset, same flips, same release
discipline, so a gbm/vk pair differs in one variable. Two things need them. The first is
that gbm-under-zink creates a linear scanout SHADOW beside the image the client asked
for, which is a candidate explanation for the 2x IOSurfaces per buffer seen in
`spikes/venus-churn-retention/RESULTS.md` §0.5 — the vk arm allocates one image and
settles it. The second is that they depend on no env at all beyond the venus ICD, so
they keep reproducing when the enhanced tier's selectors change again.

The reason smithay churns at all, for the record: it REPLACES a swapchain slot whenever
`Arc::get_mut` fails — which is always, with a frame in flight — so `reset_buffer_ages()`
mints fresh buffers every frame rather than cycling a fixed set.

Modes:
  probe   Report the KMS + gbm environment (connectors, modes, crtc, and one buffer's
          modifier/stride/handle) and exit. Run this first on any new image: it is how
          you learn whether modifiers are live and which mode you will get.
  churn   THE REPRODUCER. A fresh gbm scanout buffer per frame: create, AddFB2, flip,
          then release the PREVIOUS one. Live set is two buffers; distinct scanout ids
          equals the frame count. Anything the host retains beyond two buffers is
          host-side retention, because the guest has provably let go.
  static  THE NEGATIVE CONTROL. A fixed live set (default 2) allocated once and cycled
          round-robin forever. Same flip rate, same pixels, same everything — the ONE
          variable is whether each flip carries a scanout id the host has seen before.
          `churn` grows the host and `static` does not, or the vehicle is not measuring
          what it claims to.

Each mode takes a `-vk` suffix (`churn-vk`, `static-vk`, `probe-vk`) that allocates in
venus instead of gbm. Unlike the gbm modes these also WRITE each buffer (a GPU clear,
waited on a fence): an image that is never written retains address space rather than
resident pages, and the retention oracle counts bytes.

Requires DRM master, so: run as root, with nothing else holding the card
(`systemctl isolate multi-user.target` is enough to evict a session compositor).

Usage: kmschurn.py probe|churn|static[-vk] [frames] [live] [width] [height]

Output (greppable, one fact per line):
    KMS DEV <path> master=<0|1>
    KMS ENV <var>=<value> | KMS ENV <var>=UNSET
    KMS VKDEVICE <name>                            (-vk modes only)
    KMS CONNECTOR <id> type=<n> status=<n> modes=<n>
    KMS MODE <name> <w>x<h>@<hz>
    KMS CRTC <id>
    KMS BUFFER <w>x<h> mod=<0x..> stride=<n> handle=<n> planes=<n>
    CHURN START <mode> frames=<n> live=<n> <w>x<h>
    CHURN PROGRESS <i> created=<n>
    CHURN DONE <mode> flips=<n> created=<n> handles=<n> elapsed=<s>
    CHURN FAIL <stage> <detail>

`created` is the number of buffers allocated: it equals the frame count in `churn` and
stays at `live` in `static`. That is the guest-side statement of intent.

`handles` counts DISTINCT GEM handles and is reported only to be discounted — the kernel
recycles a handle number as soon as its object is destroyed, so a perfect churn run
reports `handles=3`. Do not read it as an identity count, and never use it as the
churn oracle. **The host-side oracle is the `[SCANOUT-LEDGER] ... fresh N` line in the
worker log** (libkrun `bfc332a`, needs `RUST_LOG=krun_devices=debug`): that counts binds
of a resource the display path had not already seen, which is the actual quantity the
supervisor's cache keys on.
"""
import ctypes as C
import os
import struct
import sys
import time

P = C.c_void_p

# fourcc('X','R','2','4') — the format a compositor scans out.
DRM_FORMAT_XRGB8888 = 0x34325258
DRM_FORMAT_MOD_INVALID = 0x00FFFFFFFFFFFFFF
DRM_FORMAT_MOD_LINEAR = 0

GBM_BO_USE_SCANOUT = 1 << 0
GBM_BO_USE_RENDERING = 1 << 2

DRM_MODE_PAGE_FLIP_EVENT = 0x01
DRM_MODE_FB_MODIFIERS = 1 << 1
DRM_EVENT_FLIP_COMPLETE = 0x02
DRM_MODE_CONNECTED = 1

CARD = "/dev/dri/card0"

# The env that decides whether a gbm buffer is a venus blob at all. Reported, never set
# here: the caller owns the configuration, this only records which one it got.
ENV_OF_RECORD = ("GALLIUM_DRIVER", "MESA_LOADER_DRIVER_OVERRIDE", "VK_DRIVER_FILES")


def fail(stage, detail=""):
    print(f"CHURN FAIL {stage} {detail}".rstrip(), flush=True)
    sys.exit(1)


class ModeInfo(C.Structure):
    _fields_ = [
        ("clock", C.c_uint32),
        ("hdisplay", C.c_uint16), ("hsync_start", C.c_uint16),
        ("hsync_end", C.c_uint16), ("htotal", C.c_uint16), ("hskew", C.c_uint16),
        ("vdisplay", C.c_uint16), ("vsync_start", C.c_uint16),
        ("vsync_end", C.c_uint16), ("vtotal", C.c_uint16), ("vscan", C.c_uint16),
        ("vrefresh", C.c_uint32), ("flags", C.c_uint32), ("type", C.c_uint32),
        ("name", C.c_char * 32),
    ]


class Res(C.Structure):
    _fields_ = [
        ("count_fbs", C.c_int), ("fbs", C.POINTER(C.c_uint32)),
        ("count_crtcs", C.c_int), ("crtcs", C.POINTER(C.c_uint32)),
        ("count_connectors", C.c_int), ("connectors", C.POINTER(C.c_uint32)),
        ("count_encoders", C.c_int), ("encoders", C.POINTER(C.c_uint32)),
        ("min_width", C.c_uint32), ("max_width", C.c_uint32),
        ("min_height", C.c_uint32), ("max_height", C.c_uint32),
    ]


class Connector(C.Structure):
    _fields_ = [
        ("connector_id", C.c_uint32), ("encoder_id", C.c_uint32),
        ("connector_type", C.c_uint32), ("connector_type_id", C.c_uint32),
        ("connection", C.c_int), ("mmWidth", C.c_uint32), ("mmHeight", C.c_uint32),
        ("subpixel", C.c_int),
        ("count_modes", C.c_int), ("modes", C.POINTER(ModeInfo)),
        ("count_props", C.c_int), ("props", C.POINTER(C.c_uint32)),
        ("prop_values", C.POINTER(C.c_uint64)),
        ("count_encoders", C.c_int), ("encoders", C.POINTER(C.c_uint32)),
    ]


class Encoder(C.Structure):
    _fields_ = [
        ("encoder_id", C.c_uint32), ("encoder_type", C.c_uint32),
        ("crtc_id", C.c_uint32), ("possible_crtcs", C.c_uint32),
        ("possible_clones", C.c_uint32),
    ]


def load_drm():
    drm = C.CDLL("libdrm.so.2")
    drm.drmSetMaster.argtypes = [C.c_int]
    drm.drmIsMaster.argtypes = [C.c_int]
    drm.drmModeGetResources.argtypes = [C.c_int]
    drm.drmModeGetResources.restype = C.POINTER(Res)
    drm.drmModeFreeResources.argtypes = [C.POINTER(Res)]
    drm.drmModeGetConnector.argtypes = [C.c_int, C.c_uint32]
    drm.drmModeGetConnector.restype = C.POINTER(Connector)
    drm.drmModeFreeConnector.argtypes = [C.POINTER(Connector)]
    drm.drmModeGetEncoder.argtypes = [C.c_int, C.c_uint32]
    drm.drmModeGetEncoder.restype = C.POINTER(Encoder)
    drm.drmModeFreeEncoder.argtypes = [C.POINTER(Encoder)]
    u32x4 = C.c_uint32 * 4
    u64x4 = C.c_uint64 * 4
    drm.drmModeAddFB2WithModifiers.argtypes = [
        C.c_int, C.c_uint32, C.c_uint32, C.c_uint32,
        u32x4, u32x4, u32x4, u64x4, C.POINTER(C.c_uint32), C.c_uint32,
    ]
    drm.drmModeAddFB2.argtypes = [
        C.c_int, C.c_uint32, C.c_uint32, C.c_uint32,
        u32x4, u32x4, u32x4, C.POINTER(C.c_uint32), C.c_uint32,
    ]
    drm.drmModeRmFB.argtypes = [C.c_int, C.c_uint32]
    drm.drmModeSetCrtc.argtypes = [
        C.c_int, C.c_uint32, C.c_uint32, C.c_uint32, C.c_uint32,
        C.POINTER(C.c_uint32), C.c_int, C.POINTER(ModeInfo),
    ]
    drm.drmModePageFlip.argtypes = [C.c_int, C.c_uint32, C.c_uint32, C.c_uint32, P]
    # The import side of the Vulkan arm: a dmabuf fd from vkGetMemoryFdKHR becomes the GEM
    # handle drmModeAddFB2WithModifiers wants, and drmCloseBufferHandle drops that reference
    # again on release.
    drm.drmPrimeFDToHandle.argtypes = [C.c_int, C.c_int, C.POINTER(C.c_uint32)]
    drm.drmCloseBufferHandle.argtypes = [C.c_int, C.c_uint32]
    return drm


def load_gbm():
    gbm = C.CDLL("libgbm.so.1")
    gbm.gbm_create_device.argtypes = [C.c_int]
    gbm.gbm_create_device.restype = P
    gbm.gbm_device_destroy.argtypes = [P]
    gbm.gbm_bo_create.argtypes = [P, C.c_uint32, C.c_uint32, C.c_uint32, C.c_uint32]
    gbm.gbm_bo_create.restype = P
    gbm.gbm_bo_destroy.argtypes = [P]
    gbm.gbm_bo_get_stride.argtypes = [P]
    gbm.gbm_bo_get_stride.restype = C.c_uint32
    gbm.gbm_bo_get_offset.argtypes = [P, C.c_int]
    gbm.gbm_bo_get_offset.restype = C.c_uint32
    gbm.gbm_bo_get_plane_count.argtypes = [P]
    gbm.gbm_bo_get_plane_count.restype = C.c_int
    gbm.gbm_bo_get_modifier.argtypes = [P]
    gbm.gbm_bo_get_modifier.restype = C.c_uint64
    # gbm_bo_handle is an 8-byte union returned in x0; c_uint64 reads it correctly.
    gbm.gbm_bo_get_handle.argtypes = [P]
    gbm.gbm_bo_get_handle.restype = C.c_uint64
    return gbm


class Buffer:
    """One gbm buffer object with the KMS framebuffer bound to it."""

    def __init__(self, drm, gbm, fd, dev, w, h):
        self.drm, self.gbm, self.fd = drm, gbm, fd
        self.bo = gbm.gbm_bo_create(
            dev, w, h, DRM_FORMAT_XRGB8888,
            GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING,
        )
        if not self.bo:
            fail("gbm_bo_create", f"{w}x{h}")
        self.handle = gbm.gbm_bo_get_handle(self.bo) & 0xFFFFFFFF
        self.stride = gbm.gbm_bo_get_stride(self.bo)
        self.modifier = gbm.gbm_bo_get_modifier(self.bo)
        self.planes = gbm.gbm_bo_get_plane_count(self.bo)

        handles = (C.c_uint32 * 4)(self.handle, 0, 0, 0)
        pitches = (C.c_uint32 * 4)(self.stride, 0, 0, 0)
        offsets = (C.c_uint32 * 4)(gbm.gbm_bo_get_offset(self.bo, 0), 0, 0, 0)
        self.fb = C.c_uint32(0)
        if self.modifier != DRM_FORMAT_MOD_INVALID:
            mods = (C.c_uint64 * 4)(self.modifier, 0, 0, 0)
            rc = drm.drmModeAddFB2WithModifiers(
                fd, w, h, DRM_FORMAT_XRGB8888, handles, pitches, offsets, mods,
                C.byref(self.fb), DRM_MODE_FB_MODIFIERS,
            )
        else:
            rc = drm.drmModeAddFB2(
                fd, w, h, DRM_FORMAT_XRGB8888, handles, pitches, offsets,
                C.byref(self.fb), 0,
            )
        if rc:
            fail("drmModeAddFB2", f"rc={rc} handle={self.handle} stride={self.stride}")

    def release(self):
        self.drm.drmModeRmFB(self.fd, self.fb)
        self.gbm.gbm_bo_destroy(self.bo)

    def describe(self, w, h):
        return (f"KMS BUFFER {w}x{h} mod=0x{self.modifier:x} stride={self.stride} "
                f"handle={self.handle} planes={self.planes}")


# ------------------------------------------------------------------------------------
# The Vulkan-direct allocator (`-vk` modes).
#
# Same KMS presentation, different provenance for the buffer: instead of gbm handing out
# something that happens to be a venus blob because of the env, this assembles the scanout
# image in venus by hand. The sequence is synoik's `scanout-buffers-via-vulkan.md`, step
# for step, because the point of this arm is to reproduce *that* allocation path:
#
#   1. vkCreateImage, DRM_FORMAT_MODIFIER-tiled, modifier LIST = [LINEAR],
#      chained VkExternalMemoryImageCreateInfo{handleTypes = DMA_BUF}
#   2. dedicated + exported vkAllocateMemory, vkBindImageMemory
#   3. vkGetMemoryFdKHR(DMA_BUF) -> a dmabuf fd (a prime export of the venus blob GEM)
#   4. vkGetImageSubresourceLayout(MEMORY_PLANE_0) -> the true rowPitch
#   5. drmPrimeFDToHandle + drmModeAddFB2WithModifiers
#
# It exists to answer whether the 2x IOSurfaces per scanout buffer (see
# `spikes/venus-churn-retention/RESULTS.md` §0.5) are real or an artifact of gbm-under-zink,
# which creates a linear scanout shadow beside the image the client asked for.
# ------------------------------------------------------------------------------------

VK_SUCCESS = 0
API_1_1 = (1 << 22) | (1 << 12)
H = C.c_uint64

# Structure types and enums read out of vulkan_core.h, never recalled.
ST_APPLICATION_INFO = 0
ST_INSTANCE_CREATE_INFO = 1
ST_DEVICE_QUEUE_CREATE_INFO = 2
ST_DEVICE_CREATE_INFO = 3
ST_SUBMIT_INFO = 4
ST_MEMORY_ALLOCATE_INFO = 5
ST_FENCE_CREATE_INFO = 8
ST_IMAGE_CREATE_INFO = 14
ST_COMMAND_POOL_CREATE_INFO = 39
ST_COMMAND_BUFFER_ALLOCATE_INFO = 40
ST_COMMAND_BUFFER_BEGIN_INFO = 42
ST_IMAGE_MEMORY_BARRIER = 45
ST_EXTERNAL_MEMORY_IMAGE = 1000072001
ST_EXPORT_MEMORY_ALLOCATE_INFO = 1000072002
ST_MEMORY_GET_FD_INFO = 1000074002
ST_MEMORY_DEDICATED_ALLOCATE_INFO = 1000127001
ST_MODIFIER_LIST = 1000158003

TILING_DRM_MODIFIER = 1000158000  # VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT
FORMAT_B8G8R8A8_UNORM = 44        # pairs with DRM_FORMAT_XRGB8888 (alpha ignored by KMS)
HANDLE_DMA_BUF = 0x200            # VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT
# Under DRM_FORMAT_MODIFIER tiling the layout query must name a MEMORY PLANE, not the
# colour aspect — VK_IMAGE_ASPECT_COLOR_BIT is invalid usage there and the pitch you get
# back is not required to mean anything.
ASPECT_MEMORY_PLANE_0 = 0x80
ASPECT_COLOR = 0x1

USAGE_TRANSFER_DST = 0x2
USAGE_COLOR_ATTACHMENT = 0x10
LAYOUT_UNDEFINED = 0
LAYOUT_GENERAL = 1
STAGE_TOP_OF_PIPE = 0x1
STAGE_TRANSFER = 0x1000
ACCESS_TRANSFER_WRITE = 0x1000
CMD_LEVEL_PRIMARY = 0
QUEUE_FAMILY_IGNORED = 0xFFFFFFFF

VK_DEVICE_EXTS = (
    b"VK_KHR_external_memory",
    b"VK_KHR_external_memory_fd",
    b"VK_EXT_external_memory_dma_buf",
    b"VK_EXT_image_drm_format_modifier",
)


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


class ExternalMemoryImageCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("handleTypes", C.c_uint32)]


class ImageDrmFormatModifierListCreateInfoEXT(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P),
                ("drmFormatModifierCount", C.c_uint32),
                ("pDrmFormatModifiers", C.POINTER(C.c_uint64))]


class MemoryRequirements(C.Structure):
    _fields_ = [("size", C.c_uint64), ("alignment", C.c_uint64),
                ("memoryTypeBits", C.c_uint32)]


class MemoryAllocateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("allocationSize", C.c_uint64),
                ("memoryTypeIndex", C.c_uint32)]


class ExportMemoryAllocateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("handleTypes", C.c_uint32)]


class MemoryDedicatedAllocateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("image", H), ("buffer", H)]


class MemoryGetFdInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("memory", H),
                ("handleType", C.c_uint32)]


class ImageSubresource(C.Structure):
    _fields_ = [("aspectMask", C.c_uint32), ("mipLevel", C.c_uint32),
                ("arrayLayer", C.c_uint32)]


class SubresourceLayout(C.Structure):
    _fields_ = [("offset", C.c_uint64), ("size", C.c_uint64), ("rowPitch", C.c_uint64),
                ("arrayPitch", C.c_uint64), ("depthPitch", C.c_uint64)]


class CommandPoolCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
                ("queueFamilyIndex", C.c_uint32)]


class CommandBufferAllocateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("commandPool", H),
                ("level", C.c_uint32), ("commandBufferCount", C.c_uint32)]


class CommandBufferBeginInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
                ("pInheritanceInfo", P)]


class ImageSubresourceRange(C.Structure):
    _fields_ = [("aspectMask", C.c_uint32), ("baseMipLevel", C.c_uint32),
                ("levelCount", C.c_uint32), ("baseArrayLayer", C.c_uint32),
                ("layerCount", C.c_uint32)]


class ImageMemoryBarrier(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("srcAccessMask", C.c_uint32),
                ("dstAccessMask", C.c_uint32), ("oldLayout", C.c_uint32),
                ("newLayout", C.c_uint32), ("srcQueueFamilyIndex", C.c_uint32),
                ("dstQueueFamilyIndex", C.c_uint32), ("image", H),
                ("subresourceRange", ImageSubresourceRange)]


class ClearColorValue(C.Union):
    _fields_ = [("float32", C.c_float * 4), ("uint32", C.c_uint32 * 4)]


class SubmitInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("waitSemaphoreCount", C.c_uint32),
                ("pWaitSemaphores", P), ("pWaitDstStageMask", P),
                ("commandBufferCount", C.c_uint32), ("pCommandBuffers", P),
                ("signalSemaphoreCount", C.c_uint32), ("pSignalSemaphores", P)]


class FenceCreateInfo(C.Structure):
    _fields_ = [("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32)]


def load_vk():
    vk = C.CDLL("libvulkan.so.1")
    # Handles that are passed DIRECTLY as an argument must be prototyped: ctypes otherwise
    # marshals a Python int as a 32-bit C int and the callee reads 64 bits, so the top half
    # is whatever the register happened to hold. Handles inside a struct are unaffected,
    # which is why leaving these out fails only on some calls and looks like a driver bug.
    for name, args, ret in [
        ("vkCreateInstance", [P, P, P], C.c_int),
        ("vkEnumeratePhysicalDevices", [P, P, P], C.c_int),
        ("vkGetPhysicalDeviceProperties", [P, P], None),
        ("vkCreateDevice", [P, P, P, P], C.c_int),
        ("vkGetDeviceProcAddr", [P, C.c_char_p], P),
        ("vkGetDeviceQueue", [P, C.c_uint32, C.c_uint32, P], None),
        ("vkCreateImage", [P, P, P, P], C.c_int),
        ("vkDestroyImage", [P, H, P], None),
        ("vkGetImageMemoryRequirements", [P, H, P], None),
        ("vkGetImageSubresourceLayout", [P, H, P, P], None),
        ("vkAllocateMemory", [P, P, P, P], C.c_int),
        ("vkFreeMemory", [P, H, P], None),
        ("vkBindImageMemory", [P, H, H, C.c_uint64], C.c_int),
        ("vkCreateCommandPool", [P, P, P, P], C.c_int),
        ("vkAllocateCommandBuffers", [P, P, P], C.c_int),
        ("vkBeginCommandBuffer", [P, P], C.c_int),
        ("vkEndCommandBuffer", [P], C.c_int),
        ("vkCmdPipelineBarrier",
         [P, C.c_uint32, C.c_uint32, C.c_uint32, C.c_uint32, P, C.c_uint32, P,
          C.c_uint32, P], None),
        ("vkCmdClearColorImage", [P, H, C.c_uint32, P, C.c_uint32, P], None),
        ("vkQueueSubmit", [P, C.c_uint32, P, H], C.c_int),
        ("vkCreateFence", [P, P, P, P], C.c_int),
        ("vkWaitForFences", [P, C.c_uint32, P, C.c_uint32, C.c_uint64], C.c_int),
        ("vkResetFences", [P, C.c_uint32, P], C.c_int),
    ]:
        f = getattr(vk, name)
        f.argtypes = args
        f.restype = ret
    return vk


def make_vk_device(vk, want="Venus"):
    """Instance + the venus physical device + a device with the scanout extensions on."""
    app = ApplicationInfo()
    app.sType = ST_APPLICATION_INFO
    app.apiVersion = API_1_1
    ici = InstanceCreateInfo()
    ici.sType = ST_INSTANCE_CREATE_INFO
    ici.pApplicationInfo = vp(app)
    inst = C.c_void_p()
    r = vk.vkCreateInstance(C.byref(ici), None, C.byref(inst))
    if r != VK_SUCCESS:
        fail("vkCreateInstance", f"VkResult={r}")

    n = C.c_uint32(0)
    vk.vkEnumeratePhysicalDevices(inst, C.byref(n), None)
    devs = (C.c_void_p * max(n.value, 1))()
    vk.vkEnumeratePhysicalDevices(inst, C.byref(n), devs)
    phys = None
    for i in range(n.value):
        props = C.create_string_buffer(4096)
        vk.vkGetPhysicalDeviceProperties(devs[i], props)
        name = props.raw[20:20 + 256].split(b"\0")[0].decode()
        print(f"KMS VKDEVICE {name}", flush=True)
        if want in name:
            phys = devs[i]
    if phys is None:
        # Not a "no GPU" failure: it means the venus ICD was not selected, which is the
        # same env trap the gbm arm has, and just as silent if we let it through.
        fail("pick_device", f"no physical device matching {want!r} of {n.value}")

    prio = C.c_float(1.0)
    qci = DeviceQueueCreateInfo()
    qci.sType = ST_DEVICE_QUEUE_CREATE_INFO
    qci.queueCount = 1
    qci.pQueuePriorities = C.pointer(prio)
    names = (C.c_char_p * len(VK_DEVICE_EXTS))(*VK_DEVICE_EXTS)
    dci = DeviceCreateInfo()
    dci.sType = ST_DEVICE_CREATE_INFO
    dci.queueCreateInfoCount = 1
    dci.pQueueCreateInfos = vp(qci)
    dci.enabledExtensionCount = len(VK_DEVICE_EXTS)
    dci.ppEnabledExtensionNames = C.cast(names, C.c_void_p)
    dev = C.c_void_p()
    r = vk.vkCreateDevice(phys, C.byref(dci), None, C.byref(dev))
    if r != VK_SUCCESS:
        fail("vkCreateDevice", f"VkResult={r} exts={[e.decode() for e in VK_DEVICE_EXTS]}")

    # Extension entrypoints come from vkGetDeviceProcAddr; the loader is not required to
    # export them, and dlsym'ing them happens to work today only by accident.
    addr = vk.vkGetDeviceProcAddr(dev, b"vkGetMemoryFdKHR")
    if not addr:
        fail("vkGetDeviceProcAddr", "vkGetMemoryFdKHR missing")
    get_fd = C.CFUNCTYPE(C.c_int, P, P, C.POINTER(C.c_int))(addr)
    return dev, get_fd


class Recorder:
    """A queue, a command buffer and a fence — enough to CLEAR a scanout image.

    The buffers have to be written, not merely allocated: an untouched image retains
    address space rather than resident pages, and the retention oracle counts bytes. A
    clear is the cheapest real write — no pipeline, no shaders, no render pass. The wait
    is on a fence rather than `vkQueueWaitIdle` because draining the whole queue is
    something no compositor does and may itself run deferred-release work.
    """

    def __init__(self, vk, dev):
        self.vk, self.dev = vk, dev
        queue = C.c_void_p()
        vk.vkGetDeviceQueue(dev, 0, 0, C.byref(queue))
        self.queue = queue

        pci = CommandPoolCreateInfo()
        pci.sType = ST_COMMAND_POOL_CREATE_INFO
        pci.flags = 0x2  # RESET_COMMAND_BUFFER
        pool = C.c_uint64()
        if vk.vkCreateCommandPool(dev, C.byref(pci), None, C.byref(pool)):
            fail("vkCreateCommandPool")
        cai = CommandBufferAllocateInfo()
        cai.sType = ST_COMMAND_BUFFER_ALLOCATE_INFO
        cai.commandPool = pool.value
        cai.level = CMD_LEVEL_PRIMARY
        cai.commandBufferCount = 1
        self.cmd = C.c_void_p()
        if vk.vkAllocateCommandBuffers(dev, C.byref(cai), C.byref(self.cmd)):
            fail("vkAllocateCommandBuffers")
        fci = FenceCreateInfo()
        fci.sType = ST_FENCE_CREATE_INFO
        fence = C.c_uint64()
        if vk.vkCreateFence(dev, C.byref(fci), None, C.byref(fence)):
            fail("vkCreateFence")
        self.fence = fence.value

    def clear(self, img, frame):
        vk, cmd = self.vk, self.cmd
        bi = CommandBufferBeginInfo()
        bi.sType = ST_COMMAND_BUFFER_BEGIN_INFO
        bi.flags = 0x1  # ONE_TIME_SUBMIT
        if vk.vkBeginCommandBuffer(cmd, C.byref(bi)):
            fail("vkBeginCommandBuffer", f"frame={frame}")

        rng = ImageSubresourceRange(ASPECT_COLOR, 0, 1, 0, 1)
        bar = ImageMemoryBarrier()
        bar.sType = ST_IMAGE_MEMORY_BARRIER
        bar.dstAccessMask = ACCESS_TRANSFER_WRITE
        bar.oldLayout = LAYOUT_UNDEFINED
        bar.newLayout = LAYOUT_GENERAL
        bar.srcQueueFamilyIndex = QUEUE_FAMILY_IGNORED
        bar.dstQueueFamilyIndex = QUEUE_FAMILY_IGNORED
        bar.image = img
        bar.subresourceRange = rng
        vk.vkCmdPipelineBarrier(cmd, STAGE_TOP_OF_PIPE, STAGE_TRANSFER, 0,
                                0, None, 0, None, 1, C.byref(bar))
        col = ClearColorValue()
        # Vary the colour so a captured frame shows the flips actually landing.
        col.float32 = (C.c_float * 4)(0.1, ((frame % 32) / 32.0), 0.9, 1.0)
        vk.vkCmdClearColorImage(cmd, img, LAYOUT_GENERAL, C.byref(col), 1, C.byref(rng))
        if vk.vkEndCommandBuffer(cmd):
            fail("vkEndCommandBuffer", f"frame={frame}")

        cmds = (C.c_void_p * 1)(self.cmd)
        si = SubmitInfo()
        si.sType = ST_SUBMIT_INFO
        si.commandBufferCount = 1
        si.pCommandBuffers = C.cast(cmds, C.c_void_p)
        if vk.vkQueueSubmit(self.queue, 1, C.byref(si), self.fence):
            fail("vkQueueSubmit", f"frame={frame}")
        fences = (C.c_uint64 * 1)(self.fence)
        if vk.vkWaitForFences(self.dev, 1, C.cast(fences, P), 1, 5_000_000_000):
            fail("vkWaitForFences", f"frame={frame}")
        vk.vkResetFences(self.dev, 1, C.cast(fences, P))


class VkBuffer:
    """One venus-allocated scanout image, prime-exported and bound to a KMS framebuffer.

    Interchangeable with `Buffer`: same fields, same `release()`, same `describe()`.
    """

    seq = 0

    def __init__(self, drm, vkctx, fd, _dev, w, h):
        VkBuffer.seq += 1
        vk, vkdev, get_fd, rec = vkctx
        self.drm, self.vk, self.vkdev, self.fd = drm, vk, vkdev, fd
        self.mem = 0
        self.img = 0
        self.handle = 0
        self.fb = C.c_uint32(0)

        mods = (C.c_uint64 * 1)(DRM_FORMAT_MOD_LINEAR)
        modlist = ImageDrmFormatModifierListCreateInfoEXT()
        modlist.sType = ST_MODIFIER_LIST
        modlist.drmFormatModifierCount = 1
        modlist.pDrmFormatModifiers = mods
        ext = ExternalMemoryImageCreateInfo()
        ext.sType = ST_EXTERNAL_MEMORY_IMAGE
        ext.handleTypes = HANDLE_DMA_BUF
        ext.pNext = vp(modlist)
        ci = ImageCreateInfo()
        ci.sType = ST_IMAGE_CREATE_INFO
        ci.pNext = vp(ext)
        ci.imageType = 1  # VK_IMAGE_TYPE_2D
        ci.format = FORMAT_B8G8R8A8_UNORM
        ci.extent = Extent3D(w, h, 1)
        ci.mipLevels = 1
        ci.arrayLayers = 1
        ci.samples = 1
        ci.tiling = TILING_DRM_MODIFIER
        ci.usage = USAGE_COLOR_ATTACHMENT | USAGE_TRANSFER_DST
        ci.initialLayout = LAYOUT_UNDEFINED
        img = C.c_uint64()
        r = vk.vkCreateImage(vkdev, C.byref(ci), None, C.byref(img))
        if r != VK_SUCCESS:
            fail("vkCreateImage", f"VkResult={r} {w}x{h}")
        self.img = img.value

        req = MemoryRequirements()
        vk.vkGetImageMemoryRequirements(vkdev, self.img, C.byref(req))
        # Dedicated AND exported. Without both, venus suballocates guest-side and the host
        # allocator is never reached — which is exactly the path this arm exists to exercise.
        exp = ExportMemoryAllocateInfo()
        exp.sType = ST_EXPORT_MEMORY_ALLOCATE_INFO
        exp.handleTypes = HANDLE_DMA_BUF
        ded = MemoryDedicatedAllocateInfo()
        ded.sType = ST_MEMORY_DEDICATED_ALLOCATE_INFO
        ded.image = self.img
        ded.pNext = vp(exp)
        ai = MemoryAllocateInfo()
        ai.sType = ST_MEMORY_ALLOCATE_INFO
        ai.allocationSize = req.size
        ai.memoryTypeIndex = first_memory_type(req.memoryTypeBits)
        ai.pNext = vp(ded)
        mem = C.c_uint64()
        r = vk.vkAllocateMemory(vkdev, C.byref(ai), None, C.byref(mem))
        if r != VK_SUCCESS:
            fail("vkAllocateMemory", f"VkResult={r} size={req.size}")
        self.mem = mem.value
        r = vk.vkBindImageMemory(vkdev, self.img, self.mem, 0)
        if r != VK_SUCCESS:
            fail("vkBindImageMemory", f"VkResult={r}")

        gfi = MemoryGetFdInfo()
        gfi.sType = ST_MEMORY_GET_FD_INFO
        gfi.memory = self.mem
        gfi.handleType = HANDLE_DMA_BUF
        dmabuf = C.c_int(-1)
        r = get_fd(vkdev, C.byref(gfi), C.byref(dmabuf))
        if r != VK_SUCCESS:
            fail("vkGetMemoryFdKHR", f"VkResult={r}")

        sub = ImageSubresource(ASPECT_MEMORY_PLANE_0, 0, 0)
        lay = SubresourceLayout()
        vk.vkGetImageSubresourceLayout(vkdev, self.img, C.byref(sub), C.byref(lay))
        self.stride = lay.rowPitch
        self.modifier = DRM_FORMAT_MOD_LINEAR
        self.planes = 1

        handle = C.c_uint32(0)
        rc = drm.drmPrimeFDToHandle(fd, dmabuf, C.byref(handle))
        os.close(dmabuf.value)
        if rc:
            fail("drmPrimeFDToHandle", f"rc={rc}")
        self.handle = handle.value

        handles = (C.c_uint32 * 4)(self.handle, 0, 0, 0)
        pitches = (C.c_uint32 * 4)(self.stride, 0, 0, 0)
        offsets = (C.c_uint32 * 4)(lay.offset, 0, 0, 0)
        modarr = (C.c_uint64 * 4)(DRM_FORMAT_MOD_LINEAR, 0, 0, 0)
        rc = drm.drmModeAddFB2WithModifiers(
            fd, w, h, DRM_FORMAT_XRGB8888, handles, pitches, offsets, modarr,
            C.byref(self.fb), DRM_MODE_FB_MODIFIERS,
        )
        if rc:
            fail("drmModeAddFB2WithModifiers",
                 f"rc={rc} handle={self.handle} stride={self.stride}")
        rec.clear(self.img, VkBuffer.seq)

    def release(self):
        # The whole oracle rests on "the guest has provably let go", so every reference
        # goes, in order: the framebuffer, the imported GEM handle, then our Vulkan
        # objects. A leaked handle would keep the buffer alive guest-side and turn a
        # clean measurement into a confounded one.
        self.drm.drmModeRmFB(self.fd, self.fb)
        self.drm.drmCloseBufferHandle(self.fd, self.handle)
        self.vk.vkDestroyImage(self.vkdev, self.img, None)
        self.vk.vkFreeMemory(self.vkdev, self.mem, None)

    def describe(self, w, h):
        return (f"KMS BUFFER {w}x{h} mod=0x{self.modifier:x} stride={self.stride} "
                f"handle={self.handle} planes={self.planes}")


def first_memory_type(bits):
    for i in range(32):
        if bits & (1 << i):
            return i
    fail("first_memory_type", "memoryTypeBits=0")


def pick_output(drm, fd):
    """The first connected connector with a mode, and a CRTC that can drive it."""
    res = drm.drmModeGetResources(fd)
    if not res:
        fail("drmModeGetResources", "null")
    r = res.contents
    for i in range(r.count_connectors):
        cptr = drm.drmModeGetConnector(fd, r.connectors[i])
        if not cptr:
            continue
        conn = cptr.contents
        print(f"KMS CONNECTOR {conn.connector_id} type={conn.connector_type} "
              f"status={conn.connection} modes={conn.count_modes}", flush=True)
        if conn.connection != DRM_MODE_CONNECTED or conn.count_modes == 0:
            drm.drmModeFreeConnector(cptr)
            continue
        mode = ModeInfo.from_buffer_copy(conn.modes[0])
        crtc = 0
        if conn.encoder_id:
            eptr = drm.drmModeGetEncoder(fd, conn.encoder_id)
            if eptr:
                crtc = eptr.contents.crtc_id
                drm.drmModeFreeEncoder(eptr)
        if not crtc:
            # No encoder bound yet: take the first CRTC any of our encoders can drive.
            for j in range(conn.count_encoders):
                eptr = drm.drmModeGetEncoder(fd, conn.encoders[j])
                if not eptr:
                    continue
                mask = eptr.contents.possible_crtcs
                drm.drmModeFreeEncoder(eptr)
                for k in range(r.count_crtcs):
                    if mask & (1 << k):
                        crtc = r.crtcs[k]
                        break
                if crtc:
                    break
        cid = conn.connector_id
        drm.drmModeFreeConnector(cptr)
        if crtc:
            return cid, crtc, mode
    fail("pick_output", "no connected connector with a usable crtc")


def await_flip(fd):
    """Consume one page-flip completion event off the DRM fd.

    Reading the event header directly beats `drmHandleEvent`: no callback plumbing,
    and the type check is the whole of what we need.
    """
    data = os.read(fd, 4096)
    while data:
        etype, length = struct.unpack_from("=II", data, 0)
        if etype == DRM_EVENT_FLIP_COMPLETE:
            return True
        data = data[length:]
    return False


def main():
    mode_name = sys.argv[1] if len(sys.argv) > 1 else "probe"
    frames = int(sys.argv[2]) if len(sys.argv) > 2 else 300
    live = int(sys.argv[3]) if len(sys.argv) > 3 else 2
    want_w = int(sys.argv[4]) if len(sys.argv) > 4 else 0
    want_h = int(sys.argv[5]) if len(sys.argv) > 5 else 0

    # `-vk` swaps the ALLOCATOR and nothing else: same modeset, same flips, same release
    # discipline, so a gbm/vk pair differs in exactly one variable.
    vulkan = mode_name.endswith("-vk")
    base_mode = mode_name[:-3] if vulkan else mode_name
    if base_mode not in ("probe", "churn", "static"):
        fail("usage", f"unknown mode {mode_name}")
    if live < 2:
        fail("usage", "live must be >= 2 (one buffer on screen, one being built)")

    drm = load_drm()
    gbm = None if vulkan else load_gbm()

    fd = os.open(CARD, os.O_RDWR | os.O_CLOEXEC)
    drm.drmSetMaster(fd)
    print(f"KMS DEV {CARD} master={drm.drmIsMaster(fd)}", flush=True)
    for var in ENV_OF_RECORD:
        print(f"KMS ENV {var}={os.environ.get(var, 'UNSET')}", flush=True)

    conn_id, crtc_id, mode = pick_output(drm, fd)
    print(f"KMS MODE {mode.name.decode()} {mode.hdisplay}x{mode.vdisplay}"
          f"@{mode.vrefresh}", flush=True)
    print(f"KMS CRTC {crtc_id}", flush=True)
    w = want_w or mode.hdisplay
    h = want_h or mode.vdisplay

    if vulkan:
        vk = load_vk()
        vkdev, get_fd = make_vk_device(vk)
        ctx = (vk, vkdev, get_fd, Recorder(vk, vkdev))
        dev = None
        make_buffer = VkBuffer
    else:
        dev = gbm.gbm_create_device(fd)
        if not dev:
            fail("gbm_create_device", "null")
        ctx = gbm
        make_buffer = Buffer

    conns = (C.c_uint32 * 1)(conn_id)

    # The first buffer goes up with a full modeset; every later one is a page flip.
    first = make_buffer(drm, ctx, fd, dev, w, h)
    print(first.describe(w, h), flush=True)
    rc = drm.drmModeSetCrtc(fd, crtc_id, first.fb, 0, 0, conns, 1, C.byref(mode))
    if rc:
        fail("drmModeSetCrtc", f"rc={rc}")

    if base_mode == "probe":
        first.release()
        if gbm:
            gbm.gbm_device_destroy(dev)
        print(f"CHURN DONE {mode_name} flips=0 created=1 handles=1 elapsed=0.0",
              flush=True)
        return

    print(f"CHURN START {mode_name} frames={frames} live={live} {w}x{h}", flush=True)
    handles = {first.handle}
    created = 1
    flips = 0
    started = time.monotonic()

    if base_mode == "static":
        # Allocate the whole live set once, then cycle it. Every flip after the first
        # `live` carries a handle the host has already seen.
        ring = [first] + [make_buffer(drm, ctx, fd, dev, w, h) for _ in range(live - 1)]
        for b in ring:
            handles.add(b.handle)
            created += 1
        for i in range(frames):
            nxt = ring[i % live]
            if drm.drmModePageFlip(fd, crtc_id, nxt.fb, DRM_MODE_PAGE_FLIP_EVENT, None):
                fail("drmModePageFlip", f"frame={i}")
            await_flip(fd)
            flips += 1
            if flips % 100 == 0:
                print(f"CHURN PROGRESS {flips} created={created}", flush=True)
        for b in ring:
            b.release()
    else:
        # THE REPRODUCER: a buffer that has never been scanned out before, per frame.
        # `live` is held only so the one on screen is never freed under the CRTC; the
        # oldest is released the moment it is no longer the scanout target.
        held = [first]
        for i in range(frames):
            nxt = make_buffer(drm, ctx, fd, dev, w, h)
            handles.add(nxt.handle)
            created += 1
            if drm.drmModePageFlip(fd, crtc_id, nxt.fb, DRM_MODE_PAGE_FLIP_EVENT, None):
                fail("drmModePageFlip", f"frame={i}")
            await_flip(fd)
            flips += 1
            held.append(nxt)
            while len(held) > live:
                held.pop(0).release()
            if flips % 100 == 0:
                print(f"CHURN PROGRESS {flips} created={created}", flush=True)
        for b in held:
            b.release()

    elapsed = time.monotonic() - started
    print(f"CHURN DONE {mode_name} flips={flips} created={created} "
          f"handles={len(handles)} elapsed={elapsed:.1f}", flush=True)
    if gbm:
        gbm.gbm_device_destroy(dev)


if __name__ == "__main__":
    main()
