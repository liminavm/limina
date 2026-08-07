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

Why gbm and not raw Vulkan: synoik (the compositor that found the bug) runs with
`GALLIUM_DRIVER=zink` + `MESA_LOADER_DRIVER_OVERRIDE=zink`, so its gbm buffer objects
ARE venus blobs — gbm is the allocation path and venus is what backs it. Imitating that
means calling gbm, not assembling `VkImage` + `vkGetMemoryFdKHR` by hand. **Run this with
the same three env vars synoik has** (`GALLIUM_DRIVER`, `MESA_LOADER_DRIVER_OVERRIDE`,
`VK_DRIVER_FILES`); without them gbm loads a different gallium driver, the buffers are
not venus blobs, and the probe silently tests a path the bug never lived on. It reports
the env it saw so a log can be audited for that.

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

Requires DRM master, so: run as root, with nothing else holding the card
(`systemctl isolate multi-user.target` is enough to evict a session compositor).

Usage: kmschurn.py probe|churn|static [frames] [live] [width] [height]

Output (greppable, one fact per line):
    KMS DEV <path> master=<0|1>
    KMS ENV <var>=<value> | KMS ENV <var>=UNSET
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

    if mode_name not in ("probe", "churn", "static"):
        fail("usage", f"unknown mode {mode_name}")
    if live < 2:
        fail("usage", "live must be >= 2 (one buffer on screen, one being built)")

    drm = load_drm()
    gbm = load_gbm()

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

    dev = gbm.gbm_create_device(fd)
    if not dev:
        fail("gbm_create_device", "null")

    conns = (C.c_uint32 * 1)(conn_id)

    # The first buffer goes up with a full modeset; every later one is a page flip.
    first = Buffer(drm, gbm, fd, dev, w, h)
    print(first.describe(w, h), flush=True)
    rc = drm.drmModeSetCrtc(fd, crtc_id, first.fb, 0, 0, conns, 1, C.byref(mode))
    if rc:
        fail("drmModeSetCrtc", f"rc={rc}")

    if mode_name == "probe":
        first.release()
        gbm.gbm_device_destroy(dev)
        print("CHURN DONE probe flips=0 created=1 handles=1 elapsed=0.0", flush=True)
        return

    print(f"CHURN START {mode_name} frames={frames} live={live} {w}x{h}", flush=True)
    handles = {first.handle}
    created = 1
    flips = 0
    started = time.monotonic()

    if mode_name == "static":
        # Allocate the whole live set once, then cycle it. Every flip after the first
        # `live` carries a handle the host has already seen.
        ring = [first] + [Buffer(drm, gbm, fd, dev, w, h) for _ in range(live - 1)]
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
            nxt = Buffer(drm, gbm, fd, dev, w, h)
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
    gbm.gbm_device_destroy(dev)


if __name__ == "__main__":
    main()
