# venus: fall back to the stub instance when ring setup fails

**Bug.** After the renderer connects, a failure to create the instance ring is returned from
`vn_CreateInstance` as `VK_ERROR_OUT_OF_HOST_MEMORY`. The loader treats that error from any one
driver as fatal for the whole `vkCreateInstance`, so the guest loses every Vulkan driver, lavapipe
included — where a renderer version mismatch already falls back to a stub instance that just
enumerates nothing.

**Reproducer.** No program needed: boot a QEMU guest with a host-visible region too small for the
ring (`-device virtio-gpu-gl-pci,blob=true,venus=true,hostmem=64K`) and run vulkaninfo with both
ICDs:

    VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json:/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
        vulkaninfo --summary

The original trigger was a 16 KiB-page host with a 4 KiB-page guest kernel; upstream `4cf0989083d`
("venus: honor the virtio-gpu blob alignment") fixes that case on kernels that report
`VIRTGPU_PARAM_BLOB_ALIGNMENT` (Linux 7.2+), and older kernels still hit it.

## Results

| Mesa | Setup | Result |
|---|---|---|
| `main` b39d173ca93 | QEMU guest, `hostmem=64K` | `vkCreateInstance failed with ERROR_OUT_OF_HOST_MEMORY` — no devices at all |
| `main` + fix | same | `llvmpipe` enumerates; venus enumerates nothing |

Measured 2026-10-05. VN_DEBUG=init logs nothing on this path; that the failure is the ring
allocation is from the code (`vn_instance_init_ring` returns OOM when `vn_ring_create` fails), not
from a log line.

## MR description (draft)

> **venus: fall back to the stub instance when ring setup fails**
>
> When the venus ring cannot be set up after the renderer connects, `vkCreateInstance` fails with
> `VK_ERROR_OUT_OF_HOST_MEMORY`, and the loader fails the whole instance — the guest loses lavapipe
> too. A version mismatch already falls back to a stub instance; this treats ring and version-query
> failures the same way.
>
> Reproduce under QEMU with `hostmem=64K`: vulkaninfo with the venus and lavapipe ICDs fails before
> and lists llvmpipe after. Also hit by 4 KiB-page guest kernels on 16 KiB-page hosts without the
> blob-alignment param (see 4cf0989083d).
>
> A reviewer may ask whether masking a genuine OOM is right: on this path the guest is not out of
> memory — the ring is what failed — and failing the instance only removes the drivers that work.
