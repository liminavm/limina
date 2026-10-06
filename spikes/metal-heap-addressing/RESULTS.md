# Metal heap addressing and venus round-trip cost

Two questions behind `spikes/metal-bridge-tally/RESULTS.md`'s prerequisites, and the design
they feed (`SKETCH.md`):

1. Which Metal-chosen values could a guest driver compute instead of asking for them, when
   it sub-allocates out of host-owned placement heaps mapped into the guest?
2. What does one guest → host → guest round trip cost on today's transport, so a count of
   round trips per frame can be read as milliseconds?

Measured 2026-10-06 on an M1 Max with macOS 26.6.2. Two peer VMs were running (4 and 6
vCPUs) and the load average was 6.4. Counts and identities do not depend on load. Timings are
indicative.

## 1. Host probe (`probe.m`, `./build.sh [section...]`, output `runs/probe-r1.txt`)

Heaps use KK's options: placement, `StorageModeShared`, default CPU cache, untracked hazards.
Each heap also gets a buffer aliasing all of it at offset 0, as `kk_bo.c` makes one.

- **Size.** A single heap can be created at up to 16 GiB; 32 GiB returns nil. `maxBufferLength`
  is 18.7 GiB. An alias's contents are 16 KiB-aligned.
- **Mapping into a VM.** `hv_vm_map` maps the alias contents of a 1, 2, 4 or 8 GiB heap into a
  VM in one call each, and `hv_vm_unmap` reverses it. Guest access through such a mapping is
  what the venus host-visible blob path already does per blob
  (`third_party/libkrun/src/devices/src/virtio/gpu/virtio_gpu.rs:4256`). This probe did not repeat
  it.
- **Buffer addresses are arithmetic.** Three 256 MiB heaps held 192 placed buffers of random
  length at aligned offsets. Every one had `gpuAddress` = alias `gpuAddress` + offset and
  `contents` = alias `contents` + offset. That held whether the alias was made before or after
  the buffers. A compute kernel then wrote through 96 addresses computed as base + offset,
  spread over the three heaps. It had no `MTLBuffer` bound for any of them, only `useHeap`. All
  96 writes landed, checked on the CPU.
- **Buffer size and alignment are constants.** For every length from 1 B to 256 MiB, shared
  or private, `heapBufferSizeAndAlignWithLength` returns size = length and alignment 256.
- **Texture size and alignment depend on the descriptor.** The answers are repeatable and
  always 128-aligned. Shared and private agree. Sizes run above the linear byte count: 1080p
  RGBA8 render target 8,486,912 B against 8,294,400 B linear.
- **Resource ids are allocated by the host.**
  - `gpuResourceID` is a small integer from one table shared by the whole process: 16 textures
    got 1–16 and their 8 views got 17–24.
  - Freed slots are reused lowest first. After releasing 2, 5, 9 and the views, the next six
    textures got 3, 6, 10, 17, 18, 19.
  - A texture re-placed at an occupied offset gets a new id.
  - Samplers draw from the same table and are deduplicated by descriptor: an identical
    descriptor returned the existing sampler's id.
- **Shared-heap textures are twiddled.** After `replaceRegion` of an R32Uint ramp into a
  shared-heap texture, the bytes read through the alias are in Morton order within 16×16 tiles. Only 128
  of 65,536 words sit at their linear position. This holds with `allowGPUOptimizedContents` both
  on and off.
- **Shared costs nothing against private here.** A read+write compute pass over 4096² RGBA8,
  run 50 times, took 18.0 ms in both, about 372 GB/s. M1 has no lossless texture compression, so
  re-measure on M2 or later.
- Host-side cost of the queries themselves: `gpuResourceID` 23 ns; creating a texture view and
  reading its id, 1.2 µs.

## 2. Venus round trip in a guest (`rtt.c`, output `runs/rtt1/`)

F44 enhanced clone (provenance in `runs/rtt1/provenance.txt`), 4 vCPUs, host KK from the shared
`build-kk`. Each probe ran with three gaps between calls. Venus's ring thread spins, then backs
off, then parks 1 ms after its last work (`VN_RING_IDLE_TIMEOUT_NS`); a parked ring needs the
doorbell.

- **query**: `vkGetBufferDeviceAddress`, which venus always sends as a synchronous call
  (`vn_buffer.c:434`).
- **fence**: an empty `vkQueueSubmit` with a fence, then `vkWaitForFences`.

| probe | gap | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| query | 0 (hot) | 1.5 µs | 2.0 µs | 11 µs | 0.33 ms |
| query | 200 µs | 47 µs | 334 µs | 353 µs | 0.64 ms |
| query | 5 ms (parked) | 583 µs | 1.35 ms | 7.2 ms | 11.6 ms |
| fence | 0 | 196 µs | 225 µs | 270 µs | 2.0 ms |
| fence | 200 µs | 229 µs | 355 µs | 957 µs | 20.9 ms |
| fence | 5 ms | 891 µs | 1.24 ms | 11.6 ms | 17.8 ms |

Each query really crossed to the host. Over the hot phase's 5 s window the host ring served
20,039 commands at 0.9 µs each (virglrs submit tally in `runs/rtt1/worker.stamped.log`).

## What it says

1. **Every buffer-side round trip can be removed by construction.** That covers the browser's
   ~22 per frame (size/alignment and GPU address for each new buffer). The guest needs one base
   address per heap and the constant 256; the rest is arithmetic the GPU accepts.
2. **Texture and sampler ids cannot be computed by the guest.** They come from one table for
   the whole host process, which reuses freed slots lowest first. Removing those round trips
   needs indirection (guest-assigned handles into a host-filled table) or host write-back.
   `SKETCH.md` §2 proposes indirection.
3. **A round trip is cheap only while both sides are spinning.** Back to back, a query costs
   1.5 µs. With 200 µs of idle between calls it costs 47–334 µs (p50–p90), which is the ring
   thread's backoff schedule. Once the ring has parked it costs 0.6–1.4 ms. In a busy frame the
   ring never idles, so a query waits behind whatever is queued ahead of it instead. That cost is
   bounded by queue depth and was not measured here; running the query loop alongside
   `drawstorm` in the same guest would measure it. A custom transport faces the same tradeoff:
   spin a host core, or pay a wake-up. The design goal is no round trips per frame, not faster
   ones.
4. **Empty fenced submits take ~200 µs even when hot.** That is the floor of a blocking
   CPU wait on GPU work through venus + KK today. A design whose guest CPU waits on GPU
   completion once per frame pays it once per frame.
