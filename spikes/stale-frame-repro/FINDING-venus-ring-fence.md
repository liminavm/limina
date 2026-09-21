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
done" signal, and it is what a Wayland client's present rides on. virglrs retires
it immediately, on the device thread, with no host fence at all:

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
`ctx->sync_queues[ring_idx] = queue`. virglrs never reads that struct --
`rg -n "ringIdx|RingIdx|TimelineInfo|sync_queue" src/` over the whole crate returns
only the `RingIdx` newtype and its fence plumbing -- so there is no ring -> VkQueue
map to submit against.

## The OTHER sync route is not broken, and this is not "no ordering at all"

The `*SemaphoreResourceMESA` route does synchronise, on this host, today. KK's
`vkGetSemaphoreFdKHR` answering `VK_SUCCESS` with `fd = -1` is not a stub: KK
registers `kk_sync_type` as timeline-only, so every binary semaphore is wrapped
in mesa's `vk_sync_binary`, and `vk_sync_binary_export_sync_file`
(`src/vulkan/runtime/vk_sync_binary.c:127`) does

    result = vk_sync_wait(device, &binary->timeline, binary->next_point, 0,
                          OS_TIMEOUT_INFINITE);
    *pFd = -1;

-- a blocking, untimed CPU wait on the Metal shared event, then the "already
signalled" sync file. The payload really did move; it moved into a completed
wait. `vkResetFenceResourceMESA` takes the same route through
`vk_common_GetFenceFdKHR`.

So a guest that hands over a buffer *after* exporting a semaphore is correctly
ordered. The ring fence is the route that is not, and it is the one a Wayland
present rides.

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

## A separate problem the same reading turned up: two hidden inline GPU waits

`src/venus/context.rs:5121` states the rule -- "Four commands block on the GPU,
and none of them blocks in here. A handler runs with the context locked and the
resource table read-locked; a wait that slept in it would hold both for as long
as the GPU took, against every other ring of this context and every VMM resource
write -- and behind that writer, every other context's ring." `vkWaitForFences`,
`vkWaitSemaphores`, `vkDeviceWaitIdle` and `vkQueueWaitIdle` are suspended out of
the batch as a `DriverWait` for exactly that reason.

`vkWaitSemaphoreResourceMESA` and `vkResetFenceResourceMESA` are a fifth and
sixth, and they do sleep in there. Both call straight into `Driver` from the
handler, and on this host both end in the untimed `vk_sync_wait` above. They were
not counted as waits because they read as exports. The venus corpus recorded
71568 of the semaphore one.

This is not the stale-frame fault -- a wait that is too long makes nothing stale
-- but it is a renderer-wide stall of the same shape as the one `7fa5d48`
removed, and it should go through `DriverWait` like the other four.

## Measured after the fix

alface, 2026-09-21, limina main + virglrs `de3687a` (both fixes), seated synoik,
one ghost window running `btm` against `guest-ramp.sh`, `filmstrip.sh 90`, scored
by `regressions.py`. The ordering was live and said so: both contexts logged
`ring N fences are ordered on queue ...` (the compositor and the client), and the
ordered-fence counter passed its first milestone.

    88 frames, cpu 88 unique
    1 regression: frame 023 == frame 022, distance 1, net+procs+disks
    distances: [1]   rate: 1/88

Against the unfixed baseline, same script and same layout:

    strip-10-00-03: 6/57, distances [2, 3]
    (and 8/59 on the arm before it, likewise 2-3)

**Zero hits at the fault's distance.** Every unfixed hit sat at distance 2 or 3 --
the 3-image swapchain's depth -- and there are none. The single distance-1 hit is
the artefact the script exists to warn about: `cpu`, the panel carrying the ramp,
is 88/88 unique and matches nothing, while the three panels that do match are the
three whose own periodicity control already reports 14, 20 and 5 distance-1
self-matches. Two consecutive frames agreeing in three low-signal boxes is
recurrence, not content going backwards.

At the unfixed rate of 10%, 88 frames with no hit has probability ~1e-4.

The renderer logged nothing else: no refused empty submit, no fence retired
unordered, no `stepped back` and no `presents at once`.

**Not measured here:** which of the two fixes did it. This arm carries both, and
taking the sync-fd exports out of the batch changes ring-thread timing as well.
A `4f27e9b`-only control arm would settle it.
