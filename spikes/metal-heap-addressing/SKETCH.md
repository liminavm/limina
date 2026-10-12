# Sketch: memory model and value delivery for a Metal-level guest GPU stack

Target: guest GL and Vulkan drivers for Apple Silicon hosts only. They speak Metal-level
commands to a thin host replayer over limina's own transport, instead of venus/vrend. The
transport extends the virtio-gpu device: our own shared memory and doorbell, behind a feature
bit. A DKMS fork of the `virtio_gpu` kernel driver drives it (§6). Rendering never touches
virtio-gpu's queues, rings or blob semantics; display keeps virtio-gpu's protocol. A stock guest
ignores the feature bit and keeps plain virtio-gpu.

End state and staging:
1. **The virtio-gpu extension, the kernel driver fork, the wire protocol and the host replayer,
   with KosmicKrisp and zink in the guest.** KK's Metal bridge (`src/kosmickrisp/bridge/`) is replaced by a protocol encoder.
   The wire is designed from Metal's object model (heaps, placed resources, encoders, guest
   handles), not from the bridge's ~200 calls, which carry KK internals. KK adapts to the wire;
   the wire is what outlives it.
2. **A Rust GLES 3.x + EGL + GBM driver on that wire,** bypassing zink. Compositors and
   browsers use GLES/EGL, so this covers most desktop traffic. Desktop GL compatibility profile
   and GLX stay on Mesa, selected per app through glvnd, until the driver covers them.
3. **A Rust Vulkan driver replacing KK,** reusing the shader compiler built for stage 2.

None of this needs Mesa's drivers. libglvnd vendor libraries, a Vulkan ICD and a libgbm
backend are how NVIDIA's proprietary stack runs compositors (§6 covers how they select ours on a
device named `virtio_gpu`). The long pole is the shader compiler: GLSL and SPIR-V front ends, an MSL back
end, and the lowerings KK's NIR passes do today (geometry and tessellation emulation, transform
feedback, clip distances, provoking vertex, quads, wide lines, polygon mode, logic ops). After
that comes breadth, gated by VK-GL-CTS and piglit.

The two prerequisites from `spikes/metal-bridge-tally/RESULTS.md`: delivering Metal-chosen
values without round trips, and CPU-written GPU state living in memory the guest owns. Facts
cited below are measured in `RESULTS.md` (this directory) unless marked otherwise.

## 1. Memory model: host-owned arenas, mapped into the guest, sub-allocated by the guest

- **The host allocates arenas; the guest never hands memory to Metal.** An arena is a Metal
  placement heap (`StorageModeShared`, untracked), plus one buffer aliasing all of it at offset
  0. The host maps the alias's contents into a guest-physical window with a single
  `hv_vm_map`. This is the KK allocation pattern (`kk_bo.c`, `mtl_heap.m`), and the
  venus host-visible blob path already maps such memory into guests today
  (`third_party/libkrun/src/devices/src/virtio/gpu/virtio_gpu.rs:4256`). What changes is the
  granularity: a few large arenas instead of one heap per BO. Measured: a single 16 GiB heap can be
  created, and an 8 GiB alias maps into a VM in one call.
- **The guest sub-allocates every buffer and image.** It picks (arena, offset) and the host
  places the object there when the async create command arrives; nothing comes back.
  - Buffers: size = length, alignment 256, for every length and storage mode. This is a constant
    the guest knows.
  - Textures: size/alignment depends on the descriptor, is deterministic, and is aligned to 128.
    The guest asks once per distinct descriptor key and caches the answer. A steady-state app
    reuses a handful of keys, so this is a start-up cost, not a per-frame one.
- **GPU addresses are arithmetic.** A placed buffer's `gpuAddress` is the arena alias's
  `gpuAddress` + offset (192/192). The GPU dereferences addresses computed this way with no
  `MTLBuffer` bound for them, only `useHeap` (96/96). The guest learns one base per arena,
  when the arena is created. The browser's ~22 address and size round trips per frame
  therefore disappear by construction.
- **CPU-written GPU state lives in arenas and is written in place**: descriptors, argument
  buffers, push-constant and uniform rings, query results. The host never copies it. This is the
  assumption the step-1 byte counts rest on, and it now holds without `bytesNoCopy` and without a
  page-alignment constraint on the guest. A 4 KiB guest works too, because the guest maps host
  pages rather than the reverse (contrast `spikes/userptr-remap-metal/`).
- **The guest's CPU must not read texel bytes of an optimal-tiling image.** Shared-heap textures
  are twiddled even with `allowGPUOptimizedContents = NO`. That matches Vulkan's contract, where
  only `VK_IMAGE_TILING_LINEAR` images have a CPU-visible layout, and KK already makes those
  textures over buffers.
- **Shared storage costs nothing on M1 Max:** shared and private heap textures copy at the same
  rate. M1 has no lossless texture compression. On M2 and later, private textures compress, and
  render targets may want private arenas that are not guest-mapped. Re-measure there before
  deciding.
- **Owed by the host side.** Arenas are pinned device memory: the balloon must never report them
  free, and they count against the guest's memory budget. Snapshot/restore saves arena contents
  and replays the placements. Residency is per arena (`useHeap` or a residency set), so few arenas
  also retire the one-heap-per-BO region growth (`kk_bo.c` census comment).

## 2. Value delivery: what is still Metal-chosen, and how it reaches the guest without a wait

| value | today (KK on host) | here |
|---|---|---|
| buffer GPU address | `mtl_buffer_get_gpu_address` | arena base + offset, computed by the guest |
| buffer size/alignment | `mtl_heap_buffer_size_and_align_with_length` | constant (length, 256) |
| texture size/alignment | `mtl_heap_texture_size_and_align_with_descriptor` | one query per descriptor key, cached |
| texture/view/sampler id | `mtl_*_get_gpu_resource_id` | guest-assigned handle + host table (below) |
| fence / timeline progress | `mtl_shared_event_wait_until_signaled_value` | seqno in arena memory, interrupt only for blocking waits |

- **Resource ids cannot be predicted.** Metal hands out small integers from one process-wide
  table, reusing the lowest free slot, and it deduplicates samplers by descriptor. The worker
  hosts every context and its own allocations, so no guest can know which slot comes next.
- **Proposed: guest handles plus a host-maintained table.** The guest assigns each image
  view and sampler a dense handle of its own and writes that handle into descriptors on its CPU,
  with no wait. The host keeps a per-context table (an arena-resident array of `MTLResourceID`,
  indexed by handle) and fills an entry when it executes the create. That is ordered on the
  stream, before any later command that can reach a shader. The guest compiler (NIR→MSL)
  emits `table[handle]` where KK emits the id directly. This is the descriptor-heap model,
  costing one dependent load per texture or sampler access. That load is the cost to measure
  before adopting it.
- **Fallback: host write-back.** Keep KK's descriptor format. The create command names a
  guest slot, and the host writes the real id there in stream order. A descriptor update that
  references an id not yet written back is sent to the host as a host-side copy rather than
  written by the guest's CPU. This needs no shader change but complicates
  `vkUpdateDescriptorSets`. Prefer the table unless its load cost shows up.

## 3. Not solved by either prerequisite

- **Reused command buffers** (vkmark: 4.3× venus's bytes). Metal command buffers are one-shot, so
  the host has to keep the protocol stream of a reusable command buffer and re-encode it on each
  submit. The wire then carries one submit, the same as venus. This is host CPU, not wire, and it
  needs its own design for invalidation when referenced objects die.
- **The round-trip floor of the transport**, for what remains synchronous (descriptor-key
  queries, `vkDeviceWaitIdle`, query results read on the CPU). `RESULTS.md` measures venus's
  today. §4's query lane carries these across the process boundary at about 4–10 µs each.
- **Sharing**: display stays on the same device (§6). Guest processes share memory as dma-bufs:
  a shared arena is a host-visible blob resource of its context, which other contexts import as
  the same heap at the same GPU address (`spikes/kk-on-wire/RESULTS.md`). Two parts are left open:
  fences on that memory, which belong in the guest module, and consumers on other renderers
  (video, screen capture, a vrend/venus compositor), which need the host to register these blobs
  with them.

## 4. Process layout: the replayer is its own process from the start

The Metal replayer runs in a separate host process: one per VM, sandboxed, started by launchd
like the worker (`crates/limina-launch`). It is never in the worker. Two reasons:
- Metal is not hardened against a hostile caller, and the replayer is where guest-controlled
  input reaches it.
- A replayer crash then reaches the guest as a lost device, the way a browser's GPU-process
  crash reaches a page as a lost context, instead of killing the VM.

Browsers show the cost of the process boundary can be made rare rather than cheap. Chromium's
command buffer and Dawn's wire, and WebKit's GPU-process stream connection, all put commands in
a shared-memory ring. Only a wake-up crosses the boundary, once per flush. Clients choose object
ids, answer state queries from a cache, and receive errors and readback asynchronously. This
design already has each of those (§1, §2), so the boundary costs one wake per flush, not one
per call. Dawn's wire protocol, with its client-assigned ids, async errors and `mapAsync`, is
the reference to read before designing ours. WebGPU itself is too limited to be the protocol.

- **Two lanes.**
  - **Bulk lane.** The guest writes command streams into a ring in arena memory and rings a
    doorbell page mapped into its userspace. The trap (~1 µs, `spikes/doorbell-exit/`) lands
    on the vCPU thread in the worker. That thread wakes the replayer and resumes the guest at
    once, so the guest never waits on the replayer to deliver work. While busy, the replayer
    drains the ring without being woken again.
  - **Query lane.** The few synchronous calls left (texture size/align per new descriptor key,
    pipeline-cache misses, real CPU waits on the GPU) cross the boundary and wait for the reply.
    They are rare by design, and each costs about 4–10 µs (below). Answers that need no
    Metal state are served by the worker without crossing.
- **Arena memory: the replayer owns it, the worker maps it.** Measured in
  `spikes/replayer-ipc/`. The replayer allocates the placement heap (§1, unchanged) and exports
  the alias range as a Mach memory entry with no special flags. The worker maps it and
  `hv_vm_map`s it. GPU, replayer CPU, worker CPU and guest all see one coherent set of pages,
  textures included. The worker's mapping keeps the pages alive even after the replayer is
  killed with SIGKILL, so the guest never loses mapped memory. A crash loses only the Metal
  objects and texture contents, the lost-device contract, and a restarted replayer can wrap the
  surviving memory as `bytesNoCopy` buffers. Worker-owned memory wrapped with `bytesNoCopy`
  also works, and is the fallback if heaps must not be replayer-owned.
- **The wake primitive is a Mach semaphore,** or a Mach message where the wake carries data.
  A cross-process round trip costs ~3 µs hot and 6–9 µs after 5 ms idle, against 2.5 µs and
  5 µs inside one process. The boundary itself is well under 1 µs. `os_sync_wait_on_address`
  across processes is about twice as slow, with tails in the hundreds of µs. A query-lane call
  therefore costs about 4–10 µs: the doorbell trap plus the hop.
- **Recovery and snapshot share one journal.** The worker keeps a journal of the protocol's
  object creation, the same idea as the virtio-gpu journal for suspend/restore. After a replayer
  crash the guest sees a lost device and recreates its contexts. On a snapshot restore, the
  journal replays into a fresh replayer.

## 5. Other shortcuts from owning the VMM and the host (proposals, unmeasured)

- **Completion through memory.** A Metal completion handler in the replayer writes a sequence
  number into arena memory, and the worker injects an interrupt only for a guest thread that is
  sleeping on it.
- **Zero-copy scanout** (§6). Images meant for scanout are IOSurface-backed. IOSurfaces cross
  processes natively, and the IOSurface's memory is mapped into the guest like an arena. The host
  window presents that surface directly.
- **A persistent, host-wide shader cache.** Compiled shaders (`MTLBinaryArchive`) are stored by
  MSL hash on disk and shared across VMs and boots. The guest sends the hash, and the text only
  on a miss. This is the one file the replayer's sandbox needs to write.
- **Reused command buffers as host recordings.** The replayer keeps the stream and the encoded
  result of each reusable command buffer, as indirect command buffers where they fit. A
  resubmit is one id on the wire.
- **Frame pacing through a shared page.** The host writes vsync and present timestamps into a
  shared page. Guest and host share the CPU timer, so the guest reads the timestamps without a
  round trip or a conversion.
- **Arenas are a VMM memory class.** The balloon excludes them, snapshots save them, and
  accounting charges them to the VM.

## 6. One device: our transport extends virtio-gpu, driven by a `virtio_gpu` fork

There is no second GPU device. The host's virtio-gpu device carries our transport, and the guest
kernel driver is a fork of upstream `virtio_gpu` that speaks both. The guest has one DRM device
with one set of outputs, so display never hands over between devices.

- **Host.** libkrun's virtio-gpu device offers one extra feature bit and extra shared-memory
  regions: arenas (§1), the doorbell page (§4) and the timing page (§5). The protocol runs over
  those, not the virtqueues. Display, meaning modesetting, cursor, EDID and hotplug, resize,
  multi-display, and scanout across suspend/restore, keeps virtio-gpu's existing protocol on
  both sides, so none of it is re-plumbed.
- **Stock guests** ignore the unknown feature bit and see a plain virtio-gpu. The two-tier
  guarantee holds with no extra machinery.
- **Guest kernel.** The DKMS module binds the virtio-gpu device in place of upstream
  `virtio_gpu`. It keeps upstream's display code and adds our rendering path when the feature is
  negotiated. It ships in the initramfs, so it drives the display from early boot: plymouth and
  the greeter run on the same device. A modprobe `install` rule loads stock `virtio_gpu` if ours
  fails to load.
- **Zero-copy scanout.** A scanout resource backed by an IOSurface arena image is internal to the
  host device, and the host window presents that surface directly. Present timestamps come
  through the timing page.
- **Crash isolation is a rule for the fork.** Display state lives in the worker and rendering in
  the replayer (§4). A replayer crash must not reset the virtio device, because that would take
  the display down with it. The fork reports it per context as a lost device while the display
  carries on.
- **The DRM driver name stays `virtio_gpu`, with upstream's ioctls,** so Mesa's venus and vrend
  remain the per-app fallback on the same node. Mesa matches on that exact name (Mesa
  `src/virtio/vulkan/vn_renderer_virtgpu.c:1114`,
  `src/gallium/auxiliary/pipe-loader/pipe_loader_drm.c:159`). The loaders that select by name
  still pick ours:
  - **GBM:** libgbm tries `virtio_gpu_gbm.so` first and falls through to Mesa's built-in backend
    when that backend creates no device (Mesa `src/gbm/main/backend.c:147-165`). Ours declines
    when the extension is not negotiated.
  - **EGL (glvnd):** our vendor accepts a display only when the extension is present.
  - **Vulkan:** the venus ICD and ours both report a GPU on the node. The default is ours,
    through the Vulkan loader's driver ordering or Mesa's device-select layer, and venus stays
    selectable per app.
- **Cost:** an out-of-tree fork of a moderately sized upstream driver. The changes are additions
  behind the feature bit, which keeps them upstreamable. Upstream already carries GPU-specific
  protocols over `virtio_gpu` as native contexts (freedreno, amdgpu, asahi); this follows the
  same pattern, with its own shared memory and doorbell.
- **Rejected: a separate device with its own modesetting.** The guest would see two display
  devices, so the host would have to hand the display between them: report virtio-gpu's output
  disconnected and steer the compositor with a primary-GPU udev tag. Every host display feature
  would also have to be carried over the new protocol.
- **Sharing with other guest consumers** goes through virtio-gpu blob resources and dma-bufs;
  what is still open is in §3.
