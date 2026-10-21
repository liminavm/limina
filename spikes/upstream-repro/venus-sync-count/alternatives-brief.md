# Bug brief: venus, a semaphore waited on and signaled in one submission

Upstream Mesa `main` at `3b1fece6ff5`. Driver: venus (`src/virtio/vulkan/`), mainly `vn_queue.c`,
`vn_sync.c`, `vn_sync.h`, and the virtgpu renderer `vn_renderer_virtgpu.c`.

## Trigger

A binary semaphore (not created sync-fd exportable) gets a **temporary** import of a sync fd
(`vkImportSemaphoreFdKHR`, `VK_SEMAPHORE_IMPORT_TEMPORARY_BIT`, handle type `SYNC_FD`, fd -1 = an
already-signaled sync file). Then one `vkQueueSubmit` (or `vkQueueSubmit2`) uses that same
semaphore as both a wait semaphore and a signal semaphore, plus a fence. Real-world trigger:
gfxreconstruct's virtual swapchain forwarding an acquire.

Reproducer modes (each run 3 times on a QEMU 10.2 + virglrenderer 1.3.0 guest, venus on Intel):

| mode | what it adds | result on `3b1fece6ff5` |
|---|---|---|
| `sync` | the submission above | SIGSEGV 3/3 in `virtgpu_submit` |
| `timeline` | also signals a timeline semaphore to 5 via `VkTimelineSemaphoreSubmitInfo` (`vkQueueSubmit`) | SIGSEGV 3/3 |
| `timeline2` | the same through `vkQueueSubmit2`, value travels with its semaphore | SIGSEGV 3/3 |
| `group` | adds a `VkDeviceGroupSubmitInfo` carrying the semaphore's device indices | SIGSEGV 3/3, and a host renderer crash |
| `export` | the semaphore is created sync-fd exportable instead, then its fd is exported after the submit | completes 3/3 |

A fix must make `sync`, `timeline`, `timeline2` and `group` complete with correct results (the
timeline reads 5; the host receives a well-formed submit with matching array lengths), and keep
`export` working.

## Mechanism

Submission preparation in `vn_queue.c` runs in stages. Some stages run before the wait semaphores
are processed and some after, and waiting on a semaphore that holds a temporarily imported sync fd
restores its permanent payload (temporary import semantics), which changes the semaphore's
current payload in the middle of submission preparation.

- `vn_queue_submission_count_semaphore` (before the waits) counts a signal semaphore whose current
  payload is the imported sync fd as needing a renderer sync, so `sync_count` includes it.
- `vn_queue_submission_init_syncs` (after the waits) sees the restored payload and does not write
  a renderer sync for it. The renderer gets a sync slot nothing wrote, and `virtgpu_submit`
  (`vn_renderer_virtgpu.c`) reads `syncs[i]->syncobj_handle` from it, called from
  `vn_queue_submission_signal_syncs`.
- `vn_queue_submission_init_pnext` (before the waits) drops the timeline value and device-group
  index of signal semaphores that hold a sync fd, while `vn_queue_submission_init_signal_semaphores`
  (after the waits) keeps the restored semaphore in the signal list. The host then gets a signal
  semaphore list longer than the `VkTimelineSemaphoreSubmitInfo` values and the
  `VkDeviceGroupSubmitInfo` indices chained to it.

The invariant that breaks: every stage of one submission must agree on which signal semaphores
carry a renderer sync and which are dropped, so that the sync count matches the syncs written and
the signal list matches the arrays chained to it.

Regressing commit: `6f3a570d418` (also in 26.2.0 through 26.2.4).

Paths to read: every caller of the sync-fd checks for semaphores in `vn_queue.c`, the payload
types and their transitions in `vn_sync.c` / `vn_sync.h` (permanent vs temporary payload, which
payload types exist, when each is set and reset), and the submit path down to `virtgpu_submit`.
