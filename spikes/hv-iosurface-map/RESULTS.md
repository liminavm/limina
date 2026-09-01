# Spike 3: can an IOSurface's memory be mapped into a guest?

The branching spike of `docs/design/blob-decode-targets.md`. The design wants one IOSurface
to *be* the decode target's storage — mapped into the guest as a BO, bound by the GPU, and
handed to scanout by id — and nothing in the stack does that today. venus maps shm-backed
`vkMapMemory` pointers; zero-copy scanout passes surfaces by id and never maps them. IOKit
pages are not obviously anonymous pages.

**Verdict: GREEN. `hv_vm_map` takes an IOSurface's base address, the guest reads and writes
it coherently across the whole allocation, and the mapping survives the host cycling
`IOSurfaceLock` underneath it.** Phase 1 can back the guest BO with the target surface
directly.

Measured 2026-09-01, macOS 26.5, M1 Max (16 KiB host pages), 4K NV12 surface, three
consecutive runs identical.

```
./run.sh          # builds, codesigns with com.apple.security.hypervisor, runs
```

## What was asked

A bare HVF VM — one vCPU at EL1h with the MMU off, so guest virtual equals IPA — with the
surface mapped at a high IPA and MMIO by data-abort. The vehicle is the one from
`spikes/balloon-unmap-fault`; guest code is emitted inline rather than assembled.

```
=== the surface ===
  3840x2160 NV12, id 25, allocSize 12441600, luma bytesPerRow 3840, chroma offset 8294400
  base address 0x104cb0000  (% 16384 = 0, % 4096 = 0)
  mapping 12435456 of 12441600 bytes at IPA 0x400000000

Q1  hv_vm_map(IOSurface base)                       -> HV_SUCCESS
Q2  luma byte 0          off         0  guest saw 0xa5a5000000000000  match
    luma mid-plane       off   4147200  guest saw 0xa5a5000000000001  match
    chroma plane start   off   8294400  guest saw 0xa5a5000000000002  match
    last mapped page     off  12419072  guest saw 0xa5a5000000000003  match
Q3  host reads the guest's store, through the held lock              match
Q4  base address after unlock/lock  0x104cb0000                      unchanged
    guest read after the cycle      0x5a5a5a5a5a5a5a5a               match
```

- **Q1.** The base address came back 16 KiB-aligned, so `hv_vm_map` had no alignment
  objection to raise. IOKit-owned pages are accepted like any other. Only whole granules
  map, so 12435456 of the 12441600-byte allocation is covered; a real implementation sizes
  the surface to a granule multiple rather than leaving a tail out.
- **Q2.** Four probe points, deliberately spread over the start, the middle of luma, the
  chroma plane and the last mapped page. A mapping that only backed its first page would
  pass a single-offset test.
- **Q3.** A guest store is visible to the host through the surface.
- **Q4.** The one most likely to be quietly wrong, because the guest can never take part in
  the `IOSurfaceLock` protocol. The host unlocked and relocked underneath the running guest:
  the base address did not move, the guest's earlier write survived, and the guest went on
  to read a value the host planted after the cycle.

## A trap in the vehicle, not in IOSurface

The first version ran two payloads: emit, run, overwrite the payload at `RAM_BASE`, reset
PC, run again. The second run reported a value from the *wrong offset*, which read exactly
like a coherency failure and would have been written up as one.

It was a stale instruction cache. With the MMU off there is nothing to invalidate the
vCPU's I-cache when the host rewrites guest code underneath it, so the vCPU re-executed the
first payload — whose first instruction reads offset 0, which by then held the guest's own
marker. The reported value was truthful; the question it answered was not the one being
asked.

The fix is one payload with a host rendezvous: the guest parks on an MMIO read, the host
does the lock cycle during that exit, and the guest continues. **Never rewrite guest code
under a live vCPU in these probes** — pick a fresh address or rendezvous instead.

## What this does not cover

The GPU. This proves the *CPU* sides agree: host stores are visible to the guest and guest
stores to the host, through one IOSurface, across a lock cycle. It does not exercise a Metal
texture bound to the same surface writing while the guest reads, which is the arrangement
phase 2's zero-copy present actually needs.

That gap is smaller than it looks — host-visible blob coherency on this path is proven and
shipping (#28, fixed 2026-07-03), and the venus feedback buffers rely on the guest CPU
seeing host GPU writes to a mapped blob every frame. But it is a different allocator, so it
is worth an explicit arm before phase 2 rather than an inference.

Nothing here bears on the per-frame copy: VideoToolbox will not decode into a surface we
supply whatever the mapping does (`spikes/vt-blob-decode-target`), so the host copies the
decoded frame into the target either way, at ~0.10 ms for 1080p and ~0.42 ms for 4K.
