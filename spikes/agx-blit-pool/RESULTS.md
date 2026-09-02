# Does AGX's compute data-buffer pool die at a fixed blit count on one encoder?

## The question

The dogfood VMM (Mac16,8 — M4 Max, `AGXMetalG16X`) has SIGSEGV'd twice inside AGX with a
**byte-identical register state**, seven hours apart, in different processes:

```
AGX::ComputeContext::prepareForEnqueue   ->  str x8, [x9, #0x98],  x9 = NULL
x15 = 0x100000    segment size, 1 MiB
x14 = 0x10001f    cursor, 31 bytes PAST that segment
x9  = 0x0         the next segment: absent, and AGX does not check it
x1 = x2 = x3 = 0x7e03      x8 = x11 = 0x10000018500
esr = 0x92000046 (byte write)   far = 0x98
```

Every register that differs between the two crashes is an address, each differing by a constant
per-region slide (`0x7994000` for the AGX image, `0x1c84000` for the stack). Every register
holding a *value* is identical. Reached from `kk_CmdCopyBufferToImage2` →
`mtl_copy_from_buffer_to_texture`, i.e. one `copyFromBuffer:…toTexture:` on a compute encoder.

A cursor landing on `1 MiB + 31` twice is a **capacity boundary reached deterministically**, not
memory pressure — pressure faults at varying counters. Hence the hypothesis: KK packs an
unbounded number of blits into one compute encoder, and crossing AGX's 1 MiB data-buffer segment
finds no next segment.

## The vehicle

`repro.m` — raw Metal 4, mirroring exactly what KK does (`newMTL4CommandQueue`,
`newCommandAllocator`, `beginCommandBufferWithAllocator:`, `computeCommandEncoder`, then N
`copyFromBuffer:sourceOffset:sourceBytesPerRow:sourceBytesPerImage:sourceSize:toTexture:…`).
No VM, no guest, no virglrenderer, and it touches neither `third_party/virgl-prefix` nor
`/Volumes/mesa-cs/build-kk`.

## Result: NOT REPRODUCED — and the run does not test the hypothesis

Ramp on the dev Mac, doubling from 64: **survived 4,194,304 blits on a single encoder**, clean
exit, no fault at any step.

**That number is worth nothing against this bug, because the GPU is wrong.** The crash is on
`AGXMetalG16X` (Mac16,8, M4 Max). The dev Mac is an M1 Max and loads `AGXMetalG13X` — a
different driver binary, a different generation's data-buffer pool. The ramp exercised code
that is not the code that faults.

This is the "identical results across many configs mean the differential is not reaching the
system under test" trap, met before running rather than after: the vehicle was right and the
*host* was wrong. Both M-series machines available here are M1 Max, so no local
host can run this test.

## What this does and does not establish

- **Does not** falsify the 1 MiB-boundary hypothesis. It has not been tested.
- **Does** establish that plain repeated blits on one encoder are fine on G13X to 4.2M — so if
  the same test passes on a G16X, the trigger needs something this vehicle does not model
  (encoder reuse across allocator resets, mixed render/compute in one buffer, a particular copy
  geometry, or the midpass `pre_gfx` compute the geometry-unroll path submits).

## To settle it

Run `./repro` on a G16X (M4) host. It is a few seconds per step and needs no VM — but it is a
program *designed* to trip a GPU driver fault, so it should not be run casually on a machine
someone is working on. Ask first.
