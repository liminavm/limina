# venus: submit only the renderer syncs that were filled in

**Bug.** `vn_queue_submission_count_semaphore` counts a signal semaphore holding a temporarily
imported sync fd as needing a renderer sync. The waits are processed after the count, and waiting
on such a semaphore restores its permanent payload, so when one submission waits on and signals the
same binary semaphore, `vn_queue_submission_init_syncs` skips it while `sync_count` still includes
it. The renderer gets a sync slot nothing wrote. Regressed by `6f3a570d418`, which was also picked
to 26.2 (in 26.2.0 through 26.2.4).

**Fix.** `17c1e773a30` on `venus-sync-count` (`kov/mesa` on freedesktop.org), on `main`
`3b1fece6ff5`: `vn_queue_submission_init_syncs` sets `sync_count` to the number of syncs it
filled in, behind `assert(sync_index <= sync_count)`. The assert holds: a payload is
`VN_SYNC_TYPE_SYNC` only on a semaphore created with `sync_fd_export` (`vn_sync.c`
`vn_semaphore_init_payloads`), which `vn_semaphore_is_sync_fd` counts; temporary imports are
`IMPORTED_SYNC_FD`, which `init_syncs` never writes; a timeline payload does not change during
the waits. The semaphore the wait restored stays in the batch's signal list, so the renderer
signals its permanent payload.

`Fixes: 6f3a570d418` is on `main` (checked on a full clone) and reached 26.2 as `c0ceec78eb8`:
26.2.0 through 26.2.4 and `staging/26.2` carry it, 26.1 does not. No issue or MR reports the
crash, and no open MR touches `vn_queue.c` (searched 2026-10-08).

**Reproducer.** `venus-sync-count.c`: import fd -1 (an already-signaled sync file) temporarily into
a binary semaphore, then submit once with that semaphore as both wait and signal, plus a fence.
Real-world trigger: gfxreconstruct's virtual swapchain forwarding an acquire.

    cc -o venus-sync-count venus-sync-count.c -lvulkan
    VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json ./venus-sync-count [mode]

`sync` (default) is that submission. `timeline` makes it through `vkQueueSubmit` and also signals
a timeline semaphore to 5 through `VkTimelineSemaphoreSubmitInfo`; `timeline2` does the same
through `vkQueueSubmit2`, where each value travels with its semaphore. Both print the timeline's
value afterwards.

## Results

QEMU 10.2 + virglrenderer 1.3.0 guest, venus on Intel Iris Plus G7. 3 runs per cell. Measured
2026-10-08.

| Mesa | `sync` | `timeline` | `timeline2` |
|---|---|---|---|
| `main` 3b1fece6ff5 | SIGSEGV 3/3 | SIGSEGV 3/3 | SIGSEGV 3/3 |
| `main` + fix | completed 3/3 | completed, value 5, 3/3 | completed, value 5, 3/3 |

The crash is `virtgpu_submit` (`vn_renderer_virtgpu.c:815`) reading `syncs[i]->syncobj_handle`
from the slot nothing wrote, called from `vn_queue_submission_signal_syncs`. Fedora 44's
`mesa-vulkan-drivers-26.2.3-1.fc44` fails the `vkQueueSubmit` instead (measured 2026-10-05: the
unwritten slot holds different garbage).

**Left by the fix: the pNext arrays disagree with the signal list.** `init_pnext` drops the
timeline values and device-group indices of signal semaphores that hold an imported sync fd, and
runs before the waits; `init_signal_semaphores` drops the same semaphores from the signal list,
and runs after them. A semaphore both waited and signaled loses its value but keeps its place.
Under gdb at `vn_queue_submission_do_submit` in `timeline` on the fix build: `signalSemaphoreCount`
2 (the restored binary semaphore and the timeline), `signalSemaphoreValueCount` 1, values `[5]`.
The host is handed a `VkSubmitInfo` that violates VUID-VkSubmitInfo-pNext-03241 and reads the
timeline's value past the end of the array. The guest still reads 5 because venus also signals
the timeline through its renderer sync, which `vkGetSemaphoreCounterValue` reads; the host-side
effect was not observed (no validation layer on the rig host). The device-group index path has the
same shape and is not exercised. A semaphore created with `sync_fd_export` is dropped from both,
so only non-exportable semaphores hit it. Same `Fixes:`; a follow-up, not part of this MR.

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
