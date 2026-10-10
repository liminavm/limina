#!/usr/bin/env python3
"""scanoutres -- which virtio-gpu resource each CRTC is scanning out, sampled over time.

The guest-side oracle for the frame capture's `guest_resource` field: what a compositor can
learn about its own flip, read the way it would read it. For every CRTC with a framebuffer:
DRM_IOCTL_MODE_GETCRTC gives the fb id, DRM_IOCTL_MODE_GETFB2 the GEM handle behind it (filled
in only for a privileged caller: run as root), DRM_IOCTL_VIRTGPU_RESOURCE_INFO the host
resource id (`res_handle`) behind that handle. The handle GETFB2 made is closed again.

Raw ioctls, no libdrm: the struct layouts are the uapi's (include/uapi/drm/drm_mode.h,
virtgpu_drm.h), and a wrong size changes the ioctl number, so the kernel refuses it rather
than reading garbage.

Usage: scanoutres.py [samples] [interval_ms]
Prints one line per CRTC per sample:
    SCANOUTRES card=<n> crtc_index=<i> crtc=<id> fb=<fb id> res=<res_handle>
and `SCANOUTRES FAIL <what>` for a card or step that could not be read.
"""

import ctypes
import fcntl
import glob
import struct
import sys
import time


def iowr(nr, size):
    # _IOWR('d', nr, size): dir=3 (read|write), size, type 'd', nr.
    return (3 << 30) | (size << 16) | (ord("d") << 8) | nr


CARD_RES = "=QQQQIIIIIIII"  # struct drm_mode_card_res, 64 bytes
CRTC = "=QIIIIIII68s"  # struct drm_mode_crtc, 104 bytes (68-byte drm_mode_modeinfo)
FB_CMD2 = "=IIIII4I4I4I4x4Q"  # struct drm_mode_fb_cmd2, 104 bytes (u64 modifiers aligned)
RES_INFO = "=IIII"  # struct drm_virtgpu_resource_info, 16 bytes

DRM_IOCTL_MODE_GETRESOURCES = iowr(0xA0, struct.calcsize(CARD_RES))
DRM_IOCTL_MODE_GETCRTC = iowr(0xA1, struct.calcsize(CRTC))
DRM_IOCTL_MODE_GETFB2 = iowr(0xCE, struct.calcsize(FB_CMD2))
DRM_IOCTL_VIRTGPU_RESOURCE_INFO = iowr(0x40 + 0x05, struct.calcsize(RES_INFO))
DRM_IOCTL_GEM_CLOSE = 0x40086409  # _IOW('d', 0x09, {u32 handle, u32 pad})


def crtc_ids(fd):
    """The card's CRTC ids, in index order (the virtio-gpu scanout order)."""
    res = bytearray(struct.pack(CARD_RES, *([0] * 12)))
    fcntl.ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, res)
    n = struct.unpack(CARD_RES, bytes(res))[5]
    if n == 0:
        return []
    buf = (ctypes.c_uint32 * n)()
    res = bytearray(struct.pack(CARD_RES, 0, ctypes.addressof(buf), 0, 0, 0, n, 0, 0, 0, 0, 0, 0))
    fcntl.ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, res)
    return list(buf)


def scanout_res(fd, crtc_id):
    """(fb id, res_handle) for the CRTC, or None when it shows nothing."""
    crtc = bytearray(struct.pack(CRTC, 0, 0, crtc_id, 0, 0, 0, 0, 0, b"\0" * 68))
    fcntl.ioctl(fd, DRM_IOCTL_MODE_GETCRTC, crtc)
    fb_id = struct.unpack(CRTC, bytes(crtc))[3]
    if fb_id == 0:
        return None
    fb = bytearray(struct.calcsize(FB_CMD2))
    struct.pack_into("=I", fb, 0, fb_id)
    fcntl.ioctl(fd, DRM_IOCTL_MODE_GETFB2, fb)
    handle = struct.unpack(FB_CMD2, bytes(fb))[5]
    if handle == 0:
        raise PermissionError("GETFB2 gave no handle (not privileged?)")
    try:
        info = bytearray(struct.pack(RES_INFO, handle, 0, 0, 0))
        fcntl.ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, info)
        return fb_id, struct.unpack(RES_INFO, bytes(info))[1]
    finally:
        fcntl.ioctl(fd, DRM_IOCTL_GEM_CLOSE, struct.pack("=II", handle, 0))


def main():
    samples = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    interval = (int(sys.argv[2]) if len(sys.argv) > 2 else 100) / 1000.0
    cards = []
    for path in sorted(glob.glob("/dev/dri/card*")):
        try:
            fd = open(path, "rb+", buffering=0)
            cards.append((path[len("/dev/dri/card"):], fd, crtc_ids(fd)))
        except OSError as e:
            print(f"SCANOUTRES FAIL {path} {e}", flush=True)
    for _ in range(samples):
        for card, fd, crtcs in cards:
            for index, crtc in enumerate(crtcs):
                try:
                    got = scanout_res(fd, crtc)
                except OSError as e:
                    print(f"SCANOUTRES FAIL card={card} crtc={crtc} {e}", flush=True)
                    continue
                if got:
                    fb_id, res = got
                    print(
                        f"SCANOUTRES card={card} crtc_index={index} crtc={crtc} fb={fb_id} "
                        f"res={res}",
                        flush=True,
                    )
        time.sleep(interval)


if __name__ == "__main__":
    main()
