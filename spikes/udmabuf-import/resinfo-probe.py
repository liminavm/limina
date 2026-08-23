#!/usr/bin/env python3
"""What does virtio-gpu's RESOURCE_INFO report for a PRIME-imported udmabuf?

Mesa gates the SET_TYPE command -- the one that makes the host give the blob a
real texture -- on info.blob_mem being nonzero.  This walks the exact path
glupload's DirectDmabuf uploader takes and prints the answer.  Raw ioctls, so
it needs nothing installed in the guest.
"""
import ctypes, fcntl, os, struct

def _ioc(d, t, nr, size):
    return (d << 30) | (size << 16) | (ord(t) << 8) | nr

IOW, IOWR = 1, 3
UDMABUF_CREATE = _ioc(IOW, 'u', 0x42, 24)
DRM_IOCTL_PRIME_FD_TO_HANDLE = _ioc(IOWR, 'd', 0x2e, 12)
DRM_IOCTL_VIRTGPU_RESOURCE_INFO = _ioc(IOWR, 'd', 0x40 + 0x05, 16)

libc = ctypes.CDLL("libc.so.6", use_errno=True)
SIZE = 64 * 64 * 4

mfd = libc.memfd_create(b"probe", 0x0002)          # MFD_ALLOW_SEALING
if mfd < 0:
    raise OSError(ctypes.get_errno(), "memfd_create")
os.ftruncate(mfd, SIZE)
fcntl.fcntl(mfd, 1033, 0x0002)                     # F_ADD_SEALS, F_SEAL_SHRINK

udma = os.open("/dev/udmabuf", os.O_RDWR)
cr = bytearray(struct.pack("=IIQQ", mfd, 0, 0, SIZE))
dbuf = fcntl.ioctl(udma, UDMABUF_CREATE, cr, True)   # mutable buf -> int retval
print(f"udmabuf fd={dbuf} size={SIZE}")

drm = os.open("/dev/dri/renderD128", os.O_RDWR)
buf = bytearray(struct.pack("=IIi", 0, 0, dbuf))
fcntl.ioctl(drm, DRM_IOCTL_PRIME_FD_TO_HANDLE, buf, True)
handle, _, _ = struct.unpack("=IIi", buf)
print(f"imported: gem handle={handle}")

buf = bytearray(struct.pack("=IIII", handle, 0, 0, 0))
fcntl.ioctl(drm, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, buf, True)
_, res_handle, size, blob_mem = struct.unpack("=IIII", buf)
print(f"RESOURCE_INFO: res_handle={res_handle} size={size} blob_mem={blob_mem}")
print(f"mesa sets maybe_untyped={bool(blob_mem)} -> SET_TYPE "
      f"{'IS' if blob_mem else 'is NOT'} emitted")
