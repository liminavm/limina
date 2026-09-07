# The KosmicKrisp command-allocator pool, measured

Instrumentation raised for the dogfood SIGSEGV that has now killed `limina-vmm` four times: a
store through a NULL pointer inside AGX `prepareForEnqueue`, reached from a guest GL texture
upload (`vrend_renderer_transfer_write_iov` → zink → `kk_CmdCopyBufferToImage2` →
`blitCDMBufferToTexture`). The allocator behind the encoder is ours (`kk_device.c`), so the
question this spike asks is whether we destroyed, reset, or overfilled one under a live encoder.

**The answer is no, and the fault is not about allocators at all** — see the next section. The
measurements below stand as what the pool actually does under real workloads, and the vehicle
requirements at the end still hold for anything that wants to exercise this code.

## What the fault actually is

Disassembly of AGXMetalG16X puts the fault at `prepareForEnqueue+672`, `str x8, [x9, #0x98]` with
`x9` loaded from `ComputeContext+0x918`. That field is the compute-pass state block, and its only
writer in the whole binary is `beginComputePass` (`newCommand(...)+0xc0`, which cannot be zero,
and with no early return before it). AGX runs `beginComputePass` from
`-[AGXG16XFamilyComputeContext_mtlnext initWithCommandBuffer:allocator:...]`, i.e. at
`[cmd_buf computeCommandEncoder]` — so every encoder KK is handed has been begun, and a NULL there
means a dispatch reached a compute context whose pass was never opened.

`x13`/`x14`/`x15` (`0x20000`, `0x10001f`, `0x100000`) are invariant across all four occurrences and
were once read as a pool cursor one step past a 1 MiB segment boundary. They are register state
left by the sampler-heap `addToResourceList` call, not the faulting operand; the fault is one
thing, `0x918 == NULL`. The dispatch that faulted is likewise ordinary — the crash-surviving ring
(`spikes/kk-dispatch-trace/`) named it as a `45x32` glyph upload indistinguishable from its
neighbours. **Nothing about size, count, or pool occupancy is the variable.**

What is left is the encoder's lifetime. The faulting pointer appeared 59 times in the ring, which
reads as one long-lived encoder — but a seated desktop recycles encoder *addresses* within tens of
encoders (measured: generation 17 → 21 on one address across three dispatches, 271 → 312 on
another), so those entries spanned several incarnations and the pointer carried no identity. A
stale pointer used across a recycle produces exactly this state, and a liveness flag alone cannot
see it because the new tenant is perfectly live.

So KK now stamps a **generation** per encoder address (`mtl_encoder.m`), `kk_encoder_state`
remembers the generation it was handed, and `cs_get_compute` refuses to hand out an encoder whose
address has been re-tenanted. Every compute record entry point checks liveness as well, so a use
after `endEncoding` or after release is named at the use site instead of segfaulting inside
Apple's driver several operations later. `LIMINA_KK_ENC_GUARD=abort` takes a core there.

`kk_alloc_pool_report()` prints per class on the release path
(`[LIMINA-ALLOC-POOL]`, every 2000 encoder closes and at device teardown):

    live / peak / retired | size hiwater vs budget, retirement count | peak ops per command buffer
    tombstones, use-after-destroy

A destroyed allocator keeps its struct, stamped `KK_PA_DEAD`, so any stale pointer is named at the
call that used it. `LIMINA_KK_ALLOC_GUARD=abort` turns the report into a core dump there.

## Baseline: F44 enhanced.synoik, seated desktop, glmark2 on vrend/zink-on-KK

`baseline-2026-08-31.txt` holds all 699 report lines. Two workloads, one boot:

| workload | class | live/peak | destroyed | size hiwater | retirements | peak ops/cmdbuf |
|---|---|---|---|---|---|---|
| desktop + 1 glmark2 | render | 3 / 3 | 0 | 4997 KiB | ~1 per close | 5 |
| desktop + 1 glmark2 | compute | 1 / 1 | 0 | 1285 KiB | **0** | 1 |
| desktop + 12 glmark2 | render | 13 / 13 | 16 | 4997 KiB | ~1 per close | 5 |
| desktop + 12 glmark2 | compute | 1 / 1 | 0 | 1285 KiB | **0** | 1 |

Budget is 4096 KiB per allocator; the floor is 8 live per class.

## What this settles

**The destroy path is unreachable below the floor.** At one GL client the pool holds three render
allocators, so nothing is ever destroyed however long it runs. Twelve concurrent clients push it to
thirteen and sixteen destructions follow. Any probe meant to exercise destruction must therefore
drive **more than eight live allocators of the class under test** — a single-client reproducer
cannot reach the code at all, and would read as a clean exoneration.

**Destruction itself is clean here.** Sixteen destroys, zero use-after-destroy.

**The compute class never retires under this workload.** Its allocator peaks at 1285 KiB against a
4096 KiB budget, so it never drains, is never reset, and is never destroyed. The crash was in a
*compute* encoder, so whatever the dogfood workload does, it is not what glmark2 does: reaching the
suspect code on the compute class needs an allocator driven past 4 MiB.

**Command buffers are small.** Peak 5 operations per render command buffer under glmark2, 384 at
device teardown under the desktop, 1 for compute. Nothing resembling an unbounded encoder.

## Wesnoth, played for ~25 minutes: the workload does not match

`wesnoth-2026-08-31.txt` is the timestamped stream (402 reports). Wesnoth 1.19.24 (RPM) on the
same seated synoik desktop, a human loading saved games — a save load is a bulk texture re-upload
and is what ratchets the numbers.

| class | live/peak | destroyed | size hiwater | retirements | peak ops/cmdbuf |
|---|---|---|---|---|---|
| render | 8 / **41** | **87** | 5317 KiB | 249236 | **5402** |
| compute | 4 / 4 | 0 | 4485 KiB | 13496 | 584 |

`use-after-destroy=0` throughout, across 87 destructions and a peak of 41 live render allocators.

So the destroy path *is* heavily exercised by a real workload — it was not merely untested — and
the detector stayed silent. That is evidence against use-after-destroy, not absence of evidence.

**But this run does not reproduce the crashed workload's dominant traffic.** Comparing the KK
counts block against the crashed dogfood run:

| | dogfood (crashed) | this run |
|---|---|---|
| `unroll_geometry calls` | 426,335 (all triangle fans) | 1 |
| `compute_during_pass` (pregfx) | 421,045 | 1 |
| `render_pass_starts` | 3,061,393 | 142,394 |

Normalised for the 21x difference in total work that is still four orders of magnitude, so it is
not a scale artifact. The dogfood worker spent its time issuing compute *inside an open render
pass* through the geometry-unroll path — the route `cs_get_compute` itself calls "the dangerous
route", because pre_gfx work is submitted BEFORE the draws recorded earlier in the same pass, on a
different command buffer and therefore a different allocator. This run essentially never takes it.

Dogfood was also running Firefox Nightly, and its Wesnoth was the **flatpak** (its own bundled
guest mesa) rather than the RPM used here. Either could be the source of the triangle fans.

## What the next vehicle has to do

1. **Hit the midpass-unroll path.** Target the dogfood ratio of ~0.14 `unroll_geometry` calls per
   render pass. Without it the vehicle is not running the code the crash ran.
2. **Attribute the copies.** "The compute encoder is busy" is not evidence that
   `kk_CmdCopyBufferToImage2` ran; the `copies: buf->img=…` line added afterwards answers that
   directly, and any future claim about reaching the path should cite it.
3. Drive the compute class past its floor of eight if the destroy path is to be tested there —
   though on this evidence destruction is a bystander, not the cause.

Note that the pool instrumentation was armed and silent *through* the fourth occurrence: 4445
growth events in the retained log, no `KK_PA_DEAD`, no unmatched discharge, no guard trip. That is
evidence against allocator misuse from a second direction, independent of the disassembly.
