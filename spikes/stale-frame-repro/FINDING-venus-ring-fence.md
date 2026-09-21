# The stale frame: a venus ring fence retires without waiting for the GPU

## The symptom

A ghost terminal under synoik presents a frame 2-3 client ticks old, 10-14% of
presented frames (6/57 and 8/59, scored against a never-repeating CPU ramp so
recurrence is excluded by construction). The host present path is clean
(`PresentOrder` reports zero `stepped back` and zero `presents at once`), the
client damages the whole surface on every commit (821/821), and its
`wl_surface.attach` sequence is a perfect 3-cycle over its three swapchain
images. The client attaches the right `wl_buffer` every frame; that buffer
carries the previous rotation's pixels.

## The cause

A venus context fence with `ring_idx > 0` is the guest's "this queue's work is
done" signal. Mesa's cross-context handshake rests entirely on it: the
`*SemaphoreResourceMESA` trio is a no-op by design (the exporter's payload
becomes a virtgpu ring fence, the importer CPU-waits that fence and then sends
`vkImportSemaphoreResourceMESA(resourceId = 0)` = "signalled now", which is why
virglrs only serves id 0). So the host's whole contribution to cross-context
ordering is *when it retires that ring fence*.

virglrs retires it immediately, on the device thread, with no host fence at all:

    src/renderer.rs:1240  Renderer::context_create_fence
        // A venus fence carries its waits inside the command stream, so reaching
        // here is already its answer, and it retires straight away.
        _ => self.fences.retire_context(ctx, ring, fence),

`Retirement::retire_context` (`src/fence.rs:93`) only queues the callback for
delivery on another thread. Nothing waits for the GPU.

The C reference does the opposite. `vkr_context_submit_fence` retires directly
only for `ring_idx == 0`; for any other ring it calls `vkr_queue_sync_submit`
(`third_party/virglrenderer/src/venus/vkr_queue.c:130`), which submits an empty
`vkQueueSubmit(queue, 0, NULL, sync->fence)` on the VkQueue bound to that ring
and hands the fence to that queue's sync thread, which retires it only once
`vkWaitForFences` returns. Vulkan queue ordering is what makes that correct: the
empty submit sits behind everything the guest already submitted on that queue, so
the fence cannot signal before that work has completed on the GPU.

The binding the C uses has no counterpart here either: `ring_idx` reaches the
host in `VkDeviceQueueTimelineInfoMESA.ringIdx`, in the pNext of
`vkGetDeviceQueue2`, and `vkr_queue_assign_ring_idx` records
`ctx->sync_queues[ring_idx] = queue`. virglrs never reads that struct -- `rg -n "ringIdx|RingIdx|TimelineInfo|sync_queue" src/` over the whole crate returns
only the `RingIdx` newtype and its fence plumbing -- no `ringIdx` and no queue binding -- so
there is no ring -> VkQueue map to submit against.

## The same bug was already fixed on the other branch of this function

`728e64c` ("vrend: finish GL work before retiring a classic fence", 2026-09-08) fixed exactly
this on the *classic* arm of `context_create_fence`, from the same symptom: a synoik desktop
showing other tabs' contents, on the premise that "Nothing here submits GPU work yet, so every
fence is already satisfied". Its message states the mechanism outright -- "When it is a venus
compositor importing the surface, Metal does not order its Vulkan queue against this renderer's
GL queue, and it samples a buffer whose renders have not run." The venus arm of the same `match`
still retires on arrival.

## Why the census re-lock scored 0/56 is NOT established

Serialising dispatch takes the measured rate from 10-14% to 0/56, and it is tempting to write
that up as "serial dispatch left no window". That chain is not verified: the guest's own
`vkWaitRingSeqnoMESA` already orders the fence behind the client's *dispatched* submit on the
CPU, so what serial dispatch actually delays, and why that hides an early fence, is unknown.
Record it as an unexplained correlation. The test of the diagnosis is the fence fix measured with
the census re-lock **off**.

## The fix

Give a venus ring fence a real host fence:

1. Serve `VkDeviceQueueTimelineInfoMESA` in `vkGetDeviceQueue2` and record the
   `ringIdx -> VkQueue` binding on the context.
2. In `Renderer::context_create_fence`, for `ring_idx != 0` on a venus context,
   submit an empty `vkQueueSubmit` with a fresh `VkFence` on that queue and
   retire the guest's fence only when that fence signals, off the device thread.
3. `ring_idx == 0` keeps retiring directly, as the C does.

## Verifying

`filmstrip.sh` + `regressions.py` against the ramp, 60 s with ghost and btm
running, scored beside the 10-14% unfixed baseline. The fix must also not
reinstate the stall the census re-lock cost.
