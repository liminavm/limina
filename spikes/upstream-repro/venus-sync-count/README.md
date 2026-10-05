# venus: submit only the renderer syncs that were filled in

**Bug.** `vn_queue_submission_count_semaphore` counts a signal semaphore holding a temporarily
imported sync fd as needing a renderer sync. The waits are processed after the count, and waiting
on such a semaphore restores its permanent payload, so when one submission waits on and signals the
same binary semaphore, `vn_queue_submission_init_syncs` skips it while `sync_count` still includes
it. The renderer gets a sync slot nothing wrote. Regressed by `6f3a570d418`, which was also picked
to 26.2 (in 26.2.0 through 26.2.4).

**Reproducer.** `venus-sync-count.c`: import fd -1 (an already-signaled sync file) temporarily into
a binary semaphore, then submit once with that semaphore as both wait and signal, plus a fence.
Real-world trigger: gfxreconstruct's virtual swapchain forwarding an acquire.

    cc -o venus-sync-count venus-sync-count.c -lvulkan
    VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json ./venus-sync-count

## Results

| Mesa | Setup | Result |
|---|---|---|
| `main` b39d173ca93 | QEMU guest, venus on Intel Iris Plus G7 | SIGSEGV 3/3, in `virtgpu_submit` ← `vn_queue_submission_signal_syncs` |
| `main` + fix | same | `submission completed`, exit 0, 3/3 |
| Fedora 44 `mesa-vulkan-drivers-26.2.3-1.fc44` | same | `vkQueueSubmit` fails (the unwritten slot holds different garbage) |

Measured 2026-10-05.

## MR description (draft)

> **venus: submit only the renderer syncs that were filled in**
>
> A submission that waits on and signals the same binary semaphore, while that semaphore holds a
> temporarily imported sync fd, either crashes in `virtgpu_submit` or fails: `sync_count` is
> taken before the wait restores the semaphore's permanent payload, and
> `vn_queue_submission_init_syncs` then (correctly) skips it, leaving the last slot unwritten.
>
> Reproducer attached (`cc -o repro repro.c -lvulkan`; any venus guest). On main: SIGSEGV in
> `virtgpu_submit` 3/3; with the fix it completes 3/3. Tested under QEMU 10.2 + virglrenderer
> 1.3.0, venus on an Intel host. Hit in practice by gfxreconstruct's virtual swapchain.
>
> 26.2 carries the regressing commit, hence `Cc: mesa-stable`.
