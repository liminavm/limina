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

## Results

All three vehicles run on the crashing hardware itself (Mac16,8, Apple M4 Pro, `AGXMetalG16X`)
after an earlier ramp on an M1 Max proved worthless — that host loads `AGXMetalG13X`, a
different driver binary, so it never executed the code under test. The vehicle was right and
the host was wrong.

| vehicle | scale reached | result |
|---|---|---|
| blits on ONE encoder | 4,194,304 | survived |
| encoders on one allocator, NO reset | 200,000 (`allocatedSize` 55.8 GB) | survived |
| reset/reuse cycles on one allocator | 200,000 (`allocatedSize` flat at 549,488) | survived |

So the fault is **not** blit count, **not** encoder count, and **not** allocator reuse. AGX grows
and recycles this pool correctly under all three. Incidentally confirmed: `allocatedSize` never
shrinks without a reset (55.8 GB after 200k encoders), and `[alloc reset]` returns it exactly to
its steady state.

## What the registers say, which is more than the vehicles did

```
cursor before the failing request   1,016,348      (0xf821c)
space left in the 1 MiB segment        32,228
the request (x1 = x2 = x3)             32,259      (0x7e03)
overflow                                   31 bytes
cursor after                        1,048,607      (0x10001f) = 1 MiB + 31
next segment (x9)                        NULL      -> str x8, [x9, #0x98]
```

The failing allocation **straddles the end of the segment by 31 bytes**. It is not that the pool
was exhausted — the vehicles show exhaustion is handled — it is that a request which does not fit
the current segment's remainder takes a path where the next segment pointer is NULL and AGX
stores through it without checking.

That explains both properties at once: **deterministic**, because the same request size against
the same fill state reproduces the same arithmetic to the byte; and **rare**, because it needs the
cursor to land in the narrow window where a ~32 KB request no longer fits. A 1 MiB segment holds
only ~32.5 allocations of this size, so this is a large, occasional allocation — not the ~few
hundred bytes a plain blit consumes, which is why 4.2M plain blits never came near it.

## Where that leaves it

This looks like an Apple defect in `AGX::ComputeContext::prepareForEnqueue` — an unchecked
next-segment pointer on the straddling path — and is Radar material. What is not yet known is
what KK submits that asks for 32,259 bytes of data buffer in one dispatch; the dogfood workload
reaches this via the geometry-unroll route that submits `pre_gfx` compute inside an open render
pass (421,045 times in the crashed run, against ~1 for a Wesnoth session), which none of these
vehicles model.

**Next vehicle, if it is worth building:** interleave compute into an open render pass the way
`cs_get_compute(cmd, pre_gfx)` does, rather than issuing compute alone. That is the one axis of
the crashed workload still unmodelled, and it is where a large per-dispatch allocation would
plausibly come from.
