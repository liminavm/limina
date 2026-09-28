# wildbrush.vercel.app stalls the VM

**Symptom.** Painting on https://wildbrush.vercel.app/ in the guest Firefox makes the whole VM slow
down and then freeze. Closing the tab brings it back, but only after a delay. On the host itself
(the user's Mac, natively) the page runs smoothly at about 45% GPU.

**Vehicle.** `cargo xtask run --disk wildbrush-poke.raw` (a `cp -c` clone of the F44 enhanced image,
Firefox 150, kernel 7.1.8-limina16k.4) on the M1 Max dev Mac, with the standard poke channels.
Samplers: `guest-mon.sh` (in the guest, 1 Hz: PSI, meminfo, `top -H`), `host-mon.sh <worker-pid>`
(worker CPU/RSS, host `vm_stat` swap-outs and compressor, `vmmap` footprint every 10 s), and
`sample-on-growth.sh`, which runs `sample` on the worker when the KK allocator pool first passes
200 allocators. Raw logs from all three runs are in `run1/`..`run3/` (not committed).

## What happens (measured 2026-09-28)

1. **The GL path.** Firefox's contexts are `init=0x2` (virgl2), so WebGL goes guest virgl → host
   **vrend → host zink → KosmicKrisp**, not venus. Every vrend context shares one host zink screen
   and therefore one KK `VkDevice` and one `LIMINA-ALLOC-POOL`. gnome-shell is on that same device
   and on the same virtio-gpu worker thread.
2. **Our stack multiplies the page's work.** The page draws with triangle fans. Metal has no fans,
   so KK unrolls each fan draw with a precompiled compute dispatch hoisted ahead of the pass
   (`kk_dispatch_precomp` → `cs_get_compute`). Run 2's KK counters over the runaway:
   `unroll triggers: fan` went from 4,270 to 40,853, `compute_during_pass(pregfx)` reached 33,234,
   and `render_pass_starts` reached 37,937. Each new Metal encoder is expensive on the CPU. In the
   run-2 `sample`, Firefox's zink driver thread spent 1,602 of 5,429 samples in `memset` inside AGX
   `renderCommandEncoderWithDescriptor`. The compute-encoder path pays `IOGPUResourceCreate`, a
   kernel round trip, 476 samples. The Metal submission queue sat 3,199 samples inside
   `IOGPUCommandQueueSubmitCommandBuffers`.
3. **Nothing bounds the in-flight depth.** Host zink throttles only past **5,000** batch states per
   context (`zink_batch.c` `post_submit`). KK's pool mints a new allocator whenever none has
   drained, on the premise in `kk_device.c` `kk_alloc_pool_acquire` that "the in-flight depth … is
   already bounded by the client's own fencing". This page breaks that premise with a *healthy*
   device; the WebGL antialias device loss broke it earlier. From the pool's `resets` counters
   between two 10 s reports, about 2,000 render and 1,800 compute command buffers were begun per
   second.
4. **Host memory turns it into a machine-wide stall.**

   | | Run 1 | Run 2 | Run 3 (`LIMINA_ZINK_NO_FANS=1`) |
   |---|---|---|---|
   | Pool peak, render / compute | 1,224 / 996 | 656 / 631 | 789 / **23** |
   | Worker footprint peak | **22.4 GB** (5.1 GB idle) | 13.3 GB (10 s samples) | 10.0 GB |
   | Longest control-queue drain | 8.5 s | **31.0 s**, then 23.6 s | 4.1 s |
   | Host swap-outs (16 KiB pages) | — | +61,760 | +188,400 |

   The host compressor already held about 11.7 GB before the page opened. During a drain the
   virtio-gpu worker thread is blocked in host zink `tc_sync`, reached from vrend's
   `TexSubImage2D` transfer writes, fence creation and batch flushes, and "nothing else on the
   worker ran meanwhile". So gnome-shell's frames stop too, and the desktop freezes.
5. **Recovery is slow, and incomplete.** With only one tab open, closing it quits Firefox. The
   guest journal shows a clean scope exit each time, with no coredump and no OOM kill. The
   contexts are destroyed at that moment and new work stops, but the backlog of submitted command
   buffers still has to drain. Surplus allocators are then
   retired at most one per `acquire`, after a 2 s decay. After run 1 recovered, with Firefox
   gone, the worker still had a **13.9 GB footprint and 37,518 `IOAccelerator (graphics)` regions**
   (4,722 regions in total before the page). No `teardown` pool report ever printed, so Firefox's
   contexts never tore the shared device down.

## The A/B

With `LIMINA_ZINK_NO_FANS=1` (host zink lowers fans on the CPU) there were **zero** fan unrolls and
zero compute-during-pass, and the compute pool peaked at 23 instead of 996. The user rated it
"better but slows": the render pool still ran away to 789, footprint reached 10 GB, and drains
reached 4 s. So fan unrolling roughly doubles the encoder churn, but it is not the whole story.
Plain render-pass volume (65,480 `render_pass_starts` in run 3), combined with the unbounded depth,
still outruns completion.

## Open

- **Why the render half costs the GPU so much more here than under Safari.** In run 2 the
  runaway was both completion-bound (Metal submits blocked in the kernel) and encode-heavy (zink
  driver thread about 71% busy). The run-3 sample at pool 200 showed no CPU saturation: the zink
  driver thread was 81% idle and submit was blocked for only 639 of 5,659 samples. So without fans
  the remaining runaway looks GPU-bound. The number to compare against native is render passes per
  frame. A `LIMINA_KK_POOL_SNAPSHOT` taken *during* the runaway (per-allocator `in_use` against
  `draining`/`pending`) would confirm it. The snapshots on disk were written after recovery.
- Why the page needs about 2,000 render passes per second through vrend/zink. It may be ping-pong
  FBO painting that the guest virgl driver splits further. A `LIMINA_TRACE_SUBMIT3D` count per
  context against the host pass count would tell.
- The retained 13.9 GB and 37k regions after Firefox quit: is the shared device's pool simply
  never shrinking (no `acquire` traffic), or is something leaking?
