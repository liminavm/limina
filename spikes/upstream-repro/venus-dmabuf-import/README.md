# venus: allocate dma-buf import memory synchronously — NOT sendable alone

**Patch.** `vn_device_memory_import_dma_buf` allocates through the asynchronous path, so
`vkAllocateMemory` returns `VK_SUCCESS` before the renderer has tried the import. The patch makes
the import allocation synchronous so a refusal reaches the caller.

**Why it does not stand alone upstream.** On upstream virglrenderer (vkr) a refused import is a
command-stream error, not a VkResult: `vkr: failed to import resource: invalid res_id N` →
`vkAllocateMemory resulted in CS error` → the ring is dead. Making the call synchronous only moves
where the guest notices, and since a fatal ring aborts the process upstream, the patch turns a
silent ghost allocation into an immediate abort. The patch pays off only against a renderer that
answers a refused import with `VK_ERROR_INVALID_EXTERNAL_HANDLE` (limina's does). To be sendable it
needs a virglrenderer change first: vkr returning a VkResult for an import it cannot satisfy,
instead of failing the command stream.

**Reproducer.** `venus-dmabuf-import.c`: allocate a buffer through GBM (a virgl resource), import its
dma-buf into venus with memory type 0, bind it, then run a fenced empty submit. vkr cannot import a
virgl resource, so this is a host-side refusal on any virglrenderer host. `--query` calls
`vkGetMemoryFdPropertiesKHR` first, which vkr also fails as a CS error
(`failed to query resource props: invalid res_id`), aborting the guest at the query.

    cc -o venus-dmabuf-import venus-dmabuf-import.c -lvulkan -lgbm
    VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json ./venus-dmabuf-import

## Results

| Mesa | Setup | Result |
|---|---|---|
| `main` b39d173ca93 | QEMU guest, venus on Intel Iris Plus G7, virglrenderer 1.3.0 | `vkAllocateMemory` = 0, `vkBindBufferMemory` = 0, fenced submit = 0, exit 0 — while the host logged the import as a CS error |
| `main` + patch | same | abort (SIGABRT) inside `vkAllocateMemory`; host logs the same CS error |
| either, `--query` | same | abort at `vkGetMemoryFdPropertiesKHR` |

Measured 2026-10-05.
