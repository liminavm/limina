# Asynchronous hardware video decode

Status: **phases 1 and 2 implemented; 3 and 4 proposed** · Scope: virglrs (the decode), libkrun
(snapshot drain), mesa-guest (one fence) · Backlog: *Hardware decode: what is still synchronous,
and the waits nothing counts* (`docs/hardening-backlog.md`, Video)

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

Enqueueing a job marks each of its target's textures with a **pending ticket**
(`third_party/virglrs/src/vrend/video/pending.rs`). The ticket lives on the `Texture`, the one
object every reader already reaches, including a compositor in a context that never decoded. It
holds the job's shared `Landing` and the render-thread half of delivery, and never the target
itself, so no reference cycle keeps a picture alive. The job lands when the thread has decoded
and, for composite targets, written the planes. **Settling** a texture waits for the landing,
then does that half:

- **Composite target:** the planes are already written, so settling records that they moved
  (`Planes::delivered`). The conversion into the base texture runs where it always did: in
  `fill_composites` after a command, which skips a target whose picture is still in flight so it
  never converts half a frame.
- **Per-plane target** (the stock tier's shape): the thread hands back the `Picture`, and each
  plane's texture uploads its own plane when it settles. The upload runs in whatever GL context is
  current, pins the unpack state and puts back the `GL_TEXTURE_2D` binding it borrows; nothing in
  vrend binds a pixel-unpack buffer, so that is all a mid-command upload has to restore. The
  VideoToolbox wait leaves the control thread; the upload, which is cheap, stays on it.
- **Where the barrier sits:** every resource lookup a context command makes (`Host::resource`,
  `bound_resource`, `resource_mut`, `resource_to_transfer`) settles the texture it returns, so a
  read or a write of a target -- a clear or a copy into one must land on top of the picture, not
  under it -- waits without each command knowing which resources might be targets. Two things a
  lookup does not reach are settled before draws, dispatches and clears (`Context::settle_bound`):
  the framebuffer's attachments, which hold their textures directly, and a composite view's
  conversion, which has to run before the draw samples it and never inside a sampler bind. The
  control-queue paths settle too: transfers, scanout surfaces, cursor readback and exports
  (`Vrend::settle`), and flush what they delivered, since their reader is not the context the
  upload ran in. `replicate_into` settles both of its targets, and the re-read of a blob that
  copies guest pages settles before it, so the pages land on top of the picture.
- **Nothing keeps a decoder buffer unread:** a landed per-plane picture still holds the
  VideoToolbox pool buffer it came in, so every END_FRAME first delivers, without waiting, the
  pictures that have already landed in that video's targets (`Video::deliver_landed`). A target
  the guest decodes into and never reads cannot drain the pool.
- **Cost when idle:** a renderer-wide count of unsettled tickets, kept by the tickets themselves,
  gates every barrier. With nothing in flight each lookup pays one atomic load.

With the barrier in place, the render thread blocks only when something actually reads a picture
that has not landed. A compositor that samples frame *n* while *n+1* decodes does not wait at all.
Each read that does wait is counted: `VIRGLRS_SUBMIT_STATS` prints `vrend video: N reads waited
for a picture`, which is the old stall in miniature.

A second decode into a target nothing has read yet delivers the first picture before it replaces
it, so the target holds the first picture if the second decode fails, as it did synchronously.

### Fences wait for the pictures before them

A context fence created after an END_FRAME must not signal before that picture lands. Otherwise
guarantee 2, and the kernel's "everything at or below this id is done", is broken.

- `fence_context` and `present_fence` hand the waiter the newest landing of each of the context's
  codecs. A codec's jobs land in order, so its newest landing covers all of them.
- The waiter waits for those landings before it waits on the GL syncs. With no waiter, the fence is
  answered inline and waits for them there.
- A global fence that names no context, or one the renderer does not have, waits for every
  context's in-flight decodes, as its inline answer finishes every context's GL work. A decode
  thread is idle between frames, so that is rarely more than one picture a codec.

This costs nothing when nothing is pending. It is also what makes the guest fence below mean
something.

### Where venus comes in

Venus is not in the decode path on either tier. VA-API always travels as `VIRGL_CCMD_*_VIDEO`
on the virgl stream, so decode runs in a vrend context whatever the guest's 3D tier is
(`docs/graphics.md` §4.5). The common consumers ride vrend too: guest GL goes through vrend on
both tiers, and zink-on-venus as the guest's GL driver is unsupported (§1). Firefox's WebRender
and gnome-shell import the exported dmabuf into a vrend context, so they meet the barriers above.

The exception is a guest **Vulkan** consumer that imports the decode target, such as a
GStreamer Vulkan sink or libplacebo. A composite target's IOSurface can be lent to a venus context
(`Storage::lent`), whose reads are commands on a venus ring. virglrs runs those on a thread per
ring (`third_party/virglrs/src/venus/ring_thread.rs`), not on the control thread, and never parses
them per resource; venus fences also bypass the vrend fence waiter. No host barrier can see such a
read. A per-plane target has no surface to lend, and virglrs never writes decoded pictures into
guest memory, so a lent composite target is the one guest-visible case.

### Guest-visible storage: the one case that needs the guest's help

A lent surface is read with no vrend command in between, and no barrier catches it. Nothing
orders that read today either: the guest queues END_FRAME with no fence, and a venus import is not
ordered against the control queue. Asynchronous decode would widen that window from "until the
host processes the submit" to "until the picture lands".

- **Host rule (implemented):** a surface lent to venus is marked when it is lent
  (`Surface::mark_lent`, set by the only constructor of a venus `Storage::Texture` share), and a
  composite target whose surface carries the mark decodes synchronously. A decode already queued
  when the lend happens is not waited for; with synchronous decode that read was not ordered
  either. The same holds for a per-plane target any of whose plane textures has a surface of its
  own: its picture arrives by an upload the fence does not cover. None does today (surfaces are
  minted only for 32-bit colour textures, planes are R8 or RG8), and the rule keeps a future
  plane surface from making the fence a lie.
- **Guest fix (mesa-guest, upstream candidate):** `virgl_video_end_frame` flushes with a real
  fence and hands it back through `picture->out_fence`, which makes `vaSyncSurface` wait. This is
  correct on any host: a synchronous host retires the fence after decoding, and this design's
  host retires it when the picture lands. It fixes the existing race whatever the host does.
  To be verified in phase 3: a venus consumer honours the fence either through `vaSyncSurface`
  or through implicit sync on the BO, which the fenced submit makes available.
- **Lifting the rule:** a lent target decodes asynchronously once its codec was created by a
  guest that fences END_FRAME. The fixed mesa says so with a flag bit in `CREATE_VIDEO_CODEC`.
- **Stock guests lose nothing:** their per-plane targets cannot be lent, so every read goes
  through the host and the barrier. They get the full benefit with no guest change, and so does an
  enhanced guest whose targets are never imported into venus -- Firefox's included. A partially
  upgraded guest gets it per codec, as `docs/graphics.md` §3.4 asks.

The order to ship in is the one `blob-decode-targets.md` records: host first (it honours the
flag but nothing sends it yet), then the guest.

### Snapshot, restore and teardown

- `snapshot_gpu_payload` (`third_party/libkrun/src/devices/src/virtio/gpu/virtio_gpu.rs`) first
  calls rutabaga's `limina_settle_video`, which reaches `Renderer::settle_video()`: every codec's
  newest job is waited for, then every pending ticket in the resource table is settled on ctx0.
  libkrun is its only consumer (`limina-virglrs-linux-port`: no Rust-API CI), so the two land
  together.
- A codec destroy, and a context teardown through it, joins the codec's thread after the queued
  decodes finish, so no thread outlives what it writes into. A restore rebuilds codecs empty, as it
  does now.

### A hazard that is not new

The CPU `write_plane` into an IOSurface for frame *n+1* can overlap the GPU's `fill_composites`
blit of frame *n* from the same surface. That already happened with synchronous decode, because
the write never waited for the earlier blit's GPU work. The writer moving to another thread does
not widen it, and `fill_composites` no longer converts a target whose next picture is in flight.

## Tests

Following the repo's RED-first convention:

1. **Instrument first.**
   - A virglrs stat (next to `VIRGLRS_SUBMIT_STATS`, `third_party/virglrs/src/stats.rs`) that
     records, per END_FRAME, the time the submitting thread spends in `end_frame`.
   - A test-only knob, `VIRGLRS_DECODE_DELAY_MS`, that sleeps inside the decode. It makes the
     race window wide and deterministic. Without it, a fast media engine hides every ordering bug.
2. **RED, then GREEN: the control thread is free** (`l2_video_decode_off_thread`).
   - Vehicle: a VP9 clip decoded through VA-API on the stock guest, with the delay set to 40 ms.
   - Assertions: the worst single video command on the submitting thread stays under 20 ms, and
     the hardware pictures are byte-identical to the software decoder's.
   - With the decode inside END_FRAME every command that decodes takes at least the delay, so the
     test is RED before the change.
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
2. virglrs + libkrun: decode threads, tickets, read barriers, fence gating, drains, the lend
   mark, and the `settle_video` call from the snapshot path. This covers every target not lent to
   venus: the whole stock tier, and the enhanced tier's targets that only vrend reads, which is
   the dogfood Firefox case. Items 2 and 3 are green. Item 5's first half holds on real playback:
   Firefox playing VP9 at 25 fps spent 0.03 ms a frame in END_FRAME (worst command 0.2 ms), which
   no decode fits inside (measured 2026-09-23). The flush-to-present half is not measured.
3. mesa-guest: END_FRAME emits a fence and sets the codec flag. It goes through the usual
   delivery chain (`scripts/export-mesa-guest-patches.sh` → RPM → `deliver-payload.sh`) and is
   an upstream candidate. Item 4 goes green.
4. virglrs: lent targets decode asynchronously when their codec carries the flag, so a guest
   Vulkan consumer of decoded video gets the benefit too.

## Open questions

- **Upload per-plane pictures on the decode thread too?** That needs a GL context of the
  thread's own, like the fence waiter's. Readers would then use `glWaitSync` rather than a CPU
  settle, which moves the upload off the control thread as well. Whether zink-on-KK makes a
  shared-context upload plus `glWaitSync` both correct and cheap needs a spike first, so it is
  kept out of the first cut.
- **The flag's carrier:** a spare bit in `CREATE_VIDEO_CODEC`, or a new capset-style handshake.
  The bit is cheaper. It needs checking that the command has an unused field upstream would
  accept.
