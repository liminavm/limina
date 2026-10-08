# venus: submit only the renderer syncs that were filled in

**Bug.** `vn_queue_submission_count_semaphore` counts a signal semaphore holding a temporarily
imported sync fd as needing a renderer sync. The waits are processed after the count, and waiting
on such a semaphore restores its permanent payload, so when one submission waits on and signals the
same binary semaphore, `vn_queue_submission_init_syncs` skips it while `sync_count` still includes
it. The renderer gets a sync slot nothing wrote. Regressed by `6f3a570d418`, which was also picked
to 26.2 (in 26.2.0 through 26.2.4).

**Fix.** `a3e19b76b3f` on `venus-sync-count` (`kov/mesa` on freedesktop.org), on `main`
`a51a418991f`, followed by `784d6782b0e` (below); the runs used the same commits on
`3b1fece6ff5`, and nothing under `src/virtio` changed between the two. `vn_queue_submission_init_syncs` sets `sync_count` to the number of syncs it
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
value afterwards. `group` adds a `VkDeviceGroupSubmitInfo` carrying the semaphore's device
indices.

To test a Mesa tree on a Fedora guest without installing it (the devenv ICD names the built
library):

    sudo dnf builddep mesa
    meson setup build -Dbuildtype=debugoptimized -Dvulkan-drivers=virtio -Dgallium-drivers= \
        -Dplatforms=x11,wayland
    ninja -C build
    VK_DRIVER_FILES=$PWD/build/src/virtio/vulkan/virtio_devenv_icd.$(uname -m).json \
        ./venus-sync-count [mode]

## Results

QEMU 10.2 + virglrenderer 1.3.0 guest, venus on Intel Iris Plus G7. 3 runs per cell. Measured
2026-10-08. "Follow-up" is `784d6782b0e` on top of the fix.

| Mesa | `sync` | `timeline` | `timeline2` | `group` |
|---|---|---|---|---|
| `main` 3b1fece6ff5 | SIGSEGV 3/3 | SIGSEGV 3/3 | SIGSEGV 3/3 | SIGSEGV 3/3 |
| `main` + fix | completed 3/3 | completed, value 5, 3/3 | completed, value 5, 3/3 | SIGABRT 3/3 |
| `main` + fix + follow-up | completed 3/3 | completed, value 5, 3/3 | completed, value 5, 3/3 | completed 3/3 |

The crash on `main` is `virtgpu_submit` (`vn_renderer_virtgpu.c:815`) reading
`syncs[i]->syncobj_handle` from the slot nothing wrote, called from
`vn_queue_submission_signal_syncs`. Fedora 44's `mesa-vulkan-drivers-26.2.3-1.fc44` fails the
`vkQueueSubmit` instead (measured 2026-10-05: the unwritten slot holds different garbage).

## Follow-up: values and device indices of the restored semaphore

`init_pnext` drops the timeline values and device-group indices of signal semaphores that hold a
sync fd, and runs before the waits; `init_signal_semaphores` drops the semaphores themselves, and
runs after them. A semaphore both waited and signaled loses its value and index but keeps its
place in the signal list. `784d6782b0e` ("venus: drop signal semaphore values and indices after
the waits", same `Fixes:`) moves the signal-side dropping into `init_signal_semaphores`, after the
waits; semaphores not also waited on are unaffected, since the waits change only the payloads of
waited semaphores. clang-format clean.

What venus hands the renderer, under gdb at `vn_queue_submission_do_submit`:

| mode | fix only | fix + follow-up |
|---|---|---|
| `timeline` | 2 signal semaphores, `signalSemaphoreValueCount` 1, values `[5]` | 2 semaphores, 2 values `[0, 5]` |
| `group` | 1 signal semaphore, `VkDeviceGroupSubmitInfo.signalSemaphoreCount` 0 | 1 and 1 |

`timeline` still reads 5 on the fix alone because venus also signals the timeline through its
renderer sync, which `vkGetSemaphoreCounterValue` reads; the host reads the timeline's value past
the end of the array, unobserved here (no validation layer on the rig host). `group` on the fix
alone completes the submission, then the host stops processing the ring and `vkDestroyInstance`
aborts in `vn_relax` waiting on it.

**The host crashes on it, upstream and in limina.** The renderer decodes the zero-length
`pSignalSemaphoreDeviceIndices` as NULL and passes the `VkSubmitInfo` through unvalidated (vkr:
`vkr_dispatch_vkQueueSubmit`; virglrs: `Driver::queue_submit`); Mesa's `vk_common_QueueSubmit`
then reads `pSignalSemaphoreDeviceIndices[i]` for every signal semaphore and faults at address 0.
Upstream, that is a `virgl_render_server` SIGSEGV (one coredump per `group` run on the rig host,
`vk_common_QueueSubmit` in `libvulkan_intel.so`) and only that context's ring dies. In limina the
renderer runs inside `limina-vmm`, so the same fault ends the VM: measured 2026-10-08 on the
dogfood host, `group` from a guest build with the first fix only, worker SIGSEGV
`KERN_INVALID_ADDRESS at 0x0` in `vk_common_QueueSubmit` (`libvulkan_kosmickrisp.dylib`) on a
`virglrs-ring` thread, one second after the run. The guest-side follow-up stops venus from
sending it, but any guest process can still send it, so the renderer has to check the pNext
array counts against the submit's counts (VUID-VkSubmitInfo-pNext-03240/03241, and the
`VkDeviceGroupSubmitInfo` counts) and fail the command like any other malformed one. The
`timeline` mismatch is the same pattern without a NULL: the host driver reads the signal value
past the end of a decoded array. virglrs fixes it in two commits on its main: `fa3b3f6` ("venus: refuse a submit whose pNext counts
disagree with its own") rejects the context on a device-group or timeline count mismatch before
the driver call, and `24af9c0` ("venus: refuse a submit whose chained array is counted but
absent") closes the case it missed, an array sent empty under a nonzero count, which the decoder
also turns into NULL. Not yet run against this reproducer, which needs a limina build pinned to
`24af9c0`. The same class (guest-supplied counts and offsets reaching the host driver
unvalidated) is broad across venus commands on KosmicKrisp; virglrs is addressing it with a
validation layer. A semaphore created with
`sync_fd_export` is dropped from all three lists, so only non-exportable semaphores hit this.

## Is the submission valid?

Nothing forbids one batch from waiting on and signaling the same binary semaphore: no VU on
`VkSubmitInfo` or `vkQueueSubmit` covers it. The rules that apply are
`VUID-vkQueueSubmit-pSignalSemaphores-00067` (unsignaled when the signal executes),
`VUID-vkQueueSubmit-pWaitSemaphores-03238` (the wait has a submitted signal) and
`VUID-vkQueueSubmit-pWaitSemaphores-00068` (no other queue waiting). The wait's second
synchronization scope and the signal's first both cover the batch's commands, and the wait unsignals
the semaphore, so the re-signal finds it unsignaled. Waiting on a temporarily imported payload
removes it and restores the permanent one, which the signal then acts on. gfxreconstruct relies on
this deliberately: `VulkanVirtualSwapchain`'s first-acquire image transition
(`framework/decode/vulkan_virtual_swapchain.cpp`, LunarG `5f06a43`) waits on the application's
acquire semaphore and signals it again in the same `VkSubmitInfo`, with a command buffer and
`ALL_COMMANDS` as the wait stage.

The Khronos validation layer agrees. Measured 2026-10-08 on the rig guest with
`vulkan-validation-layers-1.4.341.0-2.fc44` and `main` + fix + follow-up, with and without
synchronization validation: `sync`, `timeline`, `timeline2` and `group` all complete with no
validation message (loader debug output confirms the layer is in both the instance and device
chains). Controls in `vvl-control.c`, on the same setup: signaling twice without a wait reports
`VUID-vkQueueSubmit-pSignalSemaphores-00067`, and waiting on and signaling a semaphore nothing ever
signaled reports `VUID-vkQueueSubmit-pWaitSemaphores-03238` (and then hangs in
`vkQueueWaitIdle`, so bound it with `timeout`). Signaling it and then waiting on and
signaling it in one batch, without an import, reports nothing.

    cc -o vvl-control vvl-control.c -lvulkan
    VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation ./vvl-control double-signal|unsignaled|resignal

## Tests

venus has no unit tests in Mesa; its CI runs dEQP-VK under crosvm on lavapipe (`venus-lavapipe`,
`DEQP_FRACTION: 60` pre-merge). The test for both bugs belongs in VK-CTS:
`dEQP-VK.api.external.semaphore.sync_fd.import_signaled_temporary`
(`vktApiExternalMemoryTests.cpp`, `testSemaphoreImportSyncFdSignaled`) imports fd -1 temporarily
and only waits; a variant that also signals the semaphore in the same submission, with timeline
and device-group variants, would hit both.

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
