# Sketch: memory model and value delivery for a Metal-level guest GPU stack

Target: a guest Vulkan driver on Apple Silicon hosts that speaks Metal-level commands to a thin
host replayer over its own device and DKMS kernel driver, instead of venus/vrend over
virtio-gpu. In practice the guest userspace is KosmicKrisp's Vulkan layer with the Metal bridge
(`src/kosmickrisp/bridge/`) replaced by a protocol encoder. It still uses NIR→MSL, and GL still
comes from zink, so "replacing Mesa" means forking KK out of Mesa's tree, not writing a new
driver. GBM/EGL for the guest compositor is the piece with no answer here yet. The new device is
enhanced-tier only: a stock guest ignores it and keeps virtio-gpu.

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
  today. A custom device would use the same mechanism: a shared-memory ring polled by a host
  thread, with a doorbell once it parks.
- **Display and sharing**: exporting arena-backed images as dma-bufs for the compositor and KMS
  scanout, and GBM/EGL in the guest.
