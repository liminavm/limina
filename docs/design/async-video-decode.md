# Asynchronous hardware video decode

Status: **proposal, for review** · Scope: virglrs (the decode), libkrun (snapshot drain),
mesa-guest (one fence) · Backlog: *Hardware decode blocks the virtio-gpu control thread for the
length of every frame* (`docs/hardening-backlog.md`, Video)

## The problem

Every VA-API picture is decoded on the thread that serves the whole virtio-gpu control queue, and
that thread waits for VideoToolbox to finish before it does anything else. The chain, all on
libkrun's `gpu worker` thread (`third_party/libkrun/src/devices/src/virtio/gpu/worker.rs:152`):

- `process_queue` → `VirtioGpu::submit_command` (`third_party/libkrun/src/devices/src/virtio/gpu/virtio_gpu.rs:3624`)
- → rutabaga `submit_all`, which takes the shared renderer lock (`third_party/libkrun/src/rutabaga_gfx/src/virgl_renderer.rs:224`)
- → `Renderer::submit_cmd` (`third_party/virglrs/src/renderer.rs:1299`) → `Vrend::submit`
  → `Context::submit`
- → `Command::EndFrame` (`third_party/virglrs/src/vrend/context.rs:1974`) → `Video::end_frame`
  (`third_party/virglrs/src/vrend/video/mod.rs:1540`) → `HostDecoder::decode` (`:865`)
- → `Session::decode` (`third_party/virglrs/src/videotoolbox.rs:892`). It calls
  `VTDecompressionSessionDecodeFrame` with `decodeFlags: 0` and blocks until the output callback
  has parked the picture (`:938-944`).

So every other context's submits, flushes, `SET_SCANOUT`, cursor updates and fences wait behind
one frame's hardware decode. On the dogfood Mac a stack sample of a worker playing Firefox video
while Moonlight also used the media engine had this thread in `Session::decode` for 1094 of 4485
samples (measured 2026-09-23, M4 Pro; that run was also under the Game Mode clamp, and without
it the figure was 229 of 3834). Any slow decode, whether from a contended media engine or a large
AV1 frame, stalls the desktop's present along with it.

## What synchronous decode quietly guarantees today

Moving the decode off the thread breaks these guarantees, so the design has to replace each one.

1. **Every read sees the picture.** Nothing in vrend waits on a decode target. Every consumer is
   correct only because the decode finished before the next command ran. The consumers:
   - `fill_composites` (`third_party/virglrs/src/vrend/context/blit.rs:586`) runs after every
     command in a submit (`third_party/virglrs/src/vrend/context.rs:1414`). It blits a composite
     target's IOSurface planes into the texture that samplers read.
   - Draws that sample a decoded texture, in the decoding context or in another one. The guest
     compositor and Firefox's GL context import the exported dmabuf.
   - Transfers from per-plane targets. Composite targets refuse transfers
     (`third_party/virglrs/src/vrend/transfer.rs:471`, `:795`).
   - The frame-drop gate's `replicate_into` (`third_party/virglrs/src/vrend/video/mod.rs:1639`),
     which copies the previous picture into a target that got no fresh decode.
2. **A fence covers the decode.** A context fence (`third_party/virglrs/src/renderer.rs:1233`)
   is ordered behind the GL work before it, and a decode already counts as done before any fence
   can be created. The waiter's FIFO (`third_party/virglrs/src/vrend/waiter.rs`) keeps the
   property the guest kernel relies on: a fence signals only after every older one has.
3. **A snapshot captures a finished picture.** `WorkerCmd::Snapshot`
   (`third_party/libkrun/src/devices/src/virtio/gpu/worker.rs:58`) runs with the vCPUs quiesced,
   and with no decode in flight that is enough. The journal keeps codecs and buffers but no
   frame commands (`third_party/virglrs/src/vrend/journal.rs:206-212`), so what a restore sees
   is the target storage as it was captured.
4. **The borrowed bitstream stays alive.** The `CMBlockBuffer` wraps the bitstream with
   `kCFAllocatorNull` (`third_party/virglrs/src/videotoolbox.rs:893-909`). That is sound only
   because the unit outlives a synchronous call.

The guest does **not** rely on any of this. `virgl_video_end_frame` flushes with a NULL fence and
never fills `picture->out_fence` (`/Volumes/mesa-cs/mesa-guest/src/gallium/drivers/virgl/virgl_video.c:1060-1072`).
So `vaSyncSurface` finds no fence and returns at once (`src/gallium/frontends/va/surface.c`,
`_vlVaSyncSurface`: "No outstanding operation: nothing to do"). The guest believes a picture is
ready as soon as it has *queued* the END_FRAME.

## Design

### A decode thread per codec

Each `Codec` (one per guest `pipe_video_codec`, per context:
`third_party/virglrs/src/vrend/video/mod.rs:781`, held by `Video` in `context.rs:1239`) gets a
thread that owns its VideoToolbox `Session`. `Session` is `Send` but not `Sync`
(`videotoolbox.rs:241`, `:714`), which matches single ownership.

- **What moves:** only the `Session::decode` call, plus composite delivery. Composite delivery
  is `deliver_composite` (`mod.rs:376-419`), a CPU `write_plane` into the IOSurface that touches
  no GL. The planes are held as `Arc<dyn Held>`, and `Held: Send + Sync`
  (`third_party/virglrs/src/surface.rs:202`), so the surface can be shared with the thread.
- **What stays on the render thread:**
  - All parsing and per-codec state: the HEVC `RefPicSets`, the AV1 serializer, the frame-drop
    gate, and the superres withhold decision. It evolves in submit order, as it does today.
  - `end_frame` still does everything up to "here is a complete access unit and a destination".
    It then hands the job to the thread instead of decoding inline.
- **The job owns its bytes:** it carries the unit as a `Vec<u8>`, so the thread's call to
  `Session::decode(&unit)` is synchronous *on that thread*. The `kCFAllocatorNull` borrow keeps
  its invariant unchanged (guarantee 4).
- **Why a thread and not VideoToolbox's asynchronous mode:**
  `kVTDecodeFrame_EnableAsynchronousDecompression` still needs somewhere to wait and somewhere to
  deliver. A thread that runs today's synchronous code keeps frame ordering obvious. The queue is
  FIFO per codec, so pictures land in decode order, and the reference state in the VT session is
  never contended.
- **Backpressure:** the queue is bounded (a few jobs). A full queue blocks the render thread
  exactly as today, so the worst case is today's behaviour.

### A pending ticket per target, and a barrier at every read

Enqueueing a job marks its target resource with a **pending ticket**. The ticket completes when
the thread has decoded and, for composite targets, written the planes. Every read site from
guarantee 1 first **settles** the target: it waits for the ticket, then does the render-thread
half of delivery.

- **Composite target:** settling is the `fill_composites` blit. `fill_composites` stops running
  unconditionally after every command and runs when a completed ticket is found. Found means
  either drained without blocking at the top of the next submit, or forced by a read.
- **Per-plane target** (the stock tier's shape): the thread returns the decoded `Picture` and the
  render thread does the `tex_sub_image_2d_padded` uploads (`deliver_per_plane`, `mod.rs:429-467`)
  when it settles. The VideoToolbox wait leaves the control thread; the upload, which is cheap,
  stays on it.
- **Read sites to wrap:** sampler-view use of a decode target (in any context, since tickets
  live on the resource), transfers, `replicate_into` (its source), `fill_composites`, and resource
  or codec destruction (which also drains, so the thread never outlives its target).

With the barrier in place, the render thread blocks only when something actually reads a picture
that has not landed. A compositor that samples frame *n* while *n+1* decodes does not wait at all.

### Fences wait for the pictures before them

A context fence created after an END_FRAME must not signal before that picture lands. Otherwise
guarantee 2, and the kernel's "everything at or below this id is done", is broken.

- `fence_context` records the context's newest pending ticket in the waiter job.
- The waiter thread waits for that ticket before it waits on the GL sync.
- The waiter is already FIFO across contexts, so ordering across contexts still holds.

This costs nothing when nothing is pending. It is also what makes the guest fence below mean
something.

### Guest-visible storage: the one case that needs the guest's help

With blob decode targets (`docs/design/blob-decode-targets.md`), the enhanced tier's targets are
memory the guest can map. A consumer can read the pixels with the CPU and no host command in
between, and no barrier catches that. Nothing orders that read today either. The guest queues
END_FRAME with no fence, so a mapped read can already beat the host to the submit.
Asynchronous decode widens that window from "until the host processes the submit" to "until the
picture lands".

- **Guest fix (mesa-guest, upstream candidate):** `virgl_video_end_frame` flushes with a real
  fence and hands it back through `picture->out_fence`, which makes `vaSyncSurface` wait. This is
  correct on any host: a synchronous host retires the fence after decoding, and this design's
  host retires it when the picture lands. It fixes the existing race whatever the host does.
- **Host rule:** a target with guest-visible storage is decoded asynchronously only if its codec
  was created by a guest that fences END_FRAME. The fixed mesa says so with a flag bit in
  `CREATE_VIDEO_CODEC`. Every other guest-visible target decodes synchronously, as today.
- **Stock guests lose nothing:** their targets have no guest-visible storage (stock mesa never
  asks for the blob shape), so every read of a stock target goes through the host and the
  barrier. Stock guests get the full benefit with no guest change. Enhanced guests get it once
  the fixed mesa is delivered. A partially upgraded guest gets it per codec, as `docs/graphics.md`
  §3.4 asks.

The order to ship in is the one `blob-decode-targets.md` records: host first (it honours the
flag but nothing sends it yet), then the guest.

### Snapshot, restore and teardown

- `WorkerCmd::Snapshot` drains every codec's queue and settles every pending ticket before
  `snapshot_gpu_payload` (`virtio_gpu.rs:1297`) serializes blob contents. That needs one new
  virglrs call, `Renderer::settle_video()` or similar. libkrun is its only consumer
  (`limina-virglrs-linux-port`: no Rust-API CI), so the change and its caller land together.
- A context reset or destroy, and a codec destroy, drain and join their threads before freeing
  anything. A restore rebuilds codecs empty, as it does now.

### Hazards to check during phase 2, not new ones

The CPU `write_plane` into an IOSurface for frame *n+1* can overlap the GPU's `fill_composites`
blit of frame *n* from the same surface. That can already happen today, because the synchronous
write does not wait for the earlier blit's GPU work. Moving the write to another thread does not
widen it. It narrows it if the settle-time fill moves later. Phase 2 checks that `write_plane`'s
IOSurface lock discipline still holds with the writer on a different thread.

## Tests

Following the repo's RED-first convention:

1. **Instrument first.**
   - A virglrs stat (next to `VIRGLRS_SUBMIT_STATS`, `third_party/virglrs/src/stats.rs`) that
     records, per END_FRAME, the time the submitting thread spends in `end_frame`.
   - A test-only knob, `VIRGLRS_DECODE_DELAY_MS`, that sleeps inside the decode. It makes the
     race window wide and deterministic. Without it, a fast media engine hides every ordering bug.
2. **RED, then GREEN: the control thread is free.**
   - Vehicle: the `l2_video_vaapi` test with the delay set to 50 ms.
   - Assertion: the p99 of render-thread END_FRAME time is far below 50 ms.
   - Today it is at least 50 ms, so the test is RED before the change.
3. **Correctness under the wide window.**
   - `l2_video_vaapi`'s pixel landmarks, with the delay knob on, prove the barriers.
   - `l2_video_vaapi_restore` with the knob on proves the snapshot drain.
4. **Guest fence (enhanced).**
   - A small VA client (the `spikes/va-dmabuf-size` reproducer is most of it) decodes a frame,
     calls `vaSyncSurface`, maps the exported dmabuf and checks a pixel. Run it with the delay knob.
   - RED on current mesa-guest: the read beats the picture.
   - GREEN with the fence fix, on the synchronous host and on the asynchronous one.
5. **The backlog's own check.**
   - A stack sample during playback shows no `Session::decode` under `process_gpu_command`.
   - `LIMINA_GPU_TRACE` flush-to-present latency on a seated desktop does not move when a video
     starts.

## Phases

1. virglrs: the END_FRAME stat, the delay knob, and the RED test (item 2).
2. virglrs + libkrun: decode threads, tickets, read barriers, fence gating, drains, and the
   `settle_video` call from the snapshot path. This phase covers targets with no guest-visible
   storage, which is the whole stock tier. Items 2, 3 and 5 go green.
3. mesa-guest: END_FRAME emits a fence and sets the codec flag. It goes through the usual
   delivery chain (`scripts/export-mesa-guest-patches.sh` → RPM → `deliver-payload.sh`) and is
   an upstream candidate. Item 4 goes green.
4. virglrs: guest-visible targets decode asynchronously when their codec carries the flag. The
   enhanced tier, and the dogfood Firefox case, get the benefit.

## Open questions for review

- **Upload per-plane pictures on the decode thread too?** That needs a GL context of the
  thread's own, like the fence waiter's. Readers would then use `glWaitSync` rather than a CPU
  settle, which moves the upload off the control thread as well. Whether zink-on-KK makes a
  shared-context upload plus `glWaitSync` both correct and cheap needs a spike first, so it is
  kept out of the first cut.
- **Where should a forced settle show up?** A forced settle blocks the render thread, which is
  the old stall, now only when a reader outruns the decoder. It should be counted in the stat, so
  a regression shows up as settles rather than as a mystery.
- **The flag's carrier:** a spare bit in `CREATE_VIDEO_CODEC`, or a new capset-style handshake.
  The bit is cheaper. It needs checking that the command has an unused field upstream would
  accept.
