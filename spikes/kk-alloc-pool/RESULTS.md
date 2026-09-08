# The KosmicKrisp command-allocator pool, measured

Instrumentation raised for the dogfood SIGSEGV that has now killed `limina-vmm` five times: a
store through a pointer AGX read out of its own encoder state, reached from a guest GL texture
upload (`vrend_renderer_transfer_write_iov` → zink → `kk_CmdCopyBufferToImage2` →
`mtl_copy_from_buffer_to_texture`). The allocator behind the encoder is ours (`kk_device.c`), so
the question this spike asks is whether we destroyed, reset, or overfilled one under a live
encoder.

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

**The fault site is not fixed, and that is the discriminating evidence.** Four occurrences landed
at `prepareForEnqueue+672`; the fifth landed at
`MSLBlitDispatchContext::blitCDMTextureToTexture+840`, a different structure entirely:

    +824  ldr  x9,  [x19]           ; the blit dispatch context's encoder state — writable, ours
    +828  ldr  x10, [x9, #0x18]     ; base pointer  -> 0
    +832  ldr  w9,  [x9, #0x4]      ; 32-bit offset -> 0x21f6d26f
    +836  add  x9,  x10, x9
    +840  str  x8,  [x9, #0x8]      ; FAULT at 0x21f6d277, storing the uber-blit pipeline address

A single uninitialised field would keep faulting in the same place. Two unrelated fields in two
unrelated structures, selected by which shape the copy takes (a CDM texture-to-texture path here,
a direct buffer-to-texture path in the other four), is what a stale encoder looks like: whatever
AGX touches first after the handout is what faults. Note also that the block at `[x19]` is
mapped and writable — the three `stp xzr, xzr` stores just before the fault land in it — and the
offset field holds an odd ~568 MB value. Its contents are no longer the encoder's; whether that is
foreign data or a partial teardown does not change the conclusion.

`x13`/`x14`/`x15` (`0x20000`, `0x10001f`, `0x100000`) are invariant across the four
`prepareForEnqueue` occurrences and
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
remembers the generation it was handed, and both `cs_get_compute` and `kk_stop_encoder` refuse to
act when the address has been re-tenanted. Every compute record entry point checks liveness as
well, so a use after `endEncoding` or after release is named at the use site instead of
segfaulting inside Apple's driver several operations later. `LIMINA_KK_ENC_GUARD=abort` takes a
core at any of them.

**The stop site is the one with teeth.** `kk_stop_encoder`'s `mtl_end_encoding` + `mtl_release`
on a re-tenanted address ends *someone else's live encoder* and drops a retain they still hold, so
their object dies under them and their next dispatch lands on a freed or re-inited context. A
stale pointer therefore had a way to manufacture this exact fault **in a thread that did nothing
wrong** — which is why every crash report named AGX and none of them named the code responsible.
The general lesson: a stale reference should fail on its own, rather than oblige every call site
to remember to test. NULL checks at the callers, the first fix that suggests itself here, would
have left this path fully intact.

Two details that are easy to get backwards. A generation of 0 means the table evicted that slot,
not that anything is wrong, so it must **pass** — reading it as stale drops real work. And the
stale-handout log is rate-limited and carries `__builtin_return_address(0)`: a stale pointer stays
stale until `cs_end`, and the record path runs millions of times a session, so an unpaced line
there is a flood — the same mistake as the `clamped=1` ERROR flood two sections down.

**One residual, known and not closed.** All three checks key on the address. If the generation
table has evicted the slot for a stale address — `limina_enc_find` misses, generation reads 0, the
bridge check returns `UNKNOWN` — every one of them passes and the fault reproduces unchanged. That
needs all sixteen probe slots of one bucket occupied by live entries, which is unlikely at 8192
slots, but the probability grows with uptime and uptime is exactly where these cluster. Closing it
means a table that cannot evict a live encoder, not a bigger table.

**Silence is no longer evidence**, so the guard states its own liveness. `[LIMINA-ALLOC-POOL]
… encoder guard:` carries, every report, the number of checks performed, the per-state bad counts,
both refusal counts, encoders seen, and table occupancy — so `grep -E 'LIMINA-ENC|is stale|refusing
to close'` coming back empty can be read as evidence rather than inferred from. A rising `checks`
with zeros beside it means the guard is working; `checks` frozen means the needle is dead.

That line was first put on the `[LIMINA] KK counts:` block, and a smoke boot caught it: that block
is driven by `kk_CmdPipelineBarrier2`, and a seated F44 desktop running Firefox and glmark2 issued
so few barriers that it printed **three times in a whole boot**. The guard's totals sat at
`checks=2` for the entire session — indistinguishable from a guard that was not running. Moved
onto the pool report, which is paced by encoder closes, the same workload reads `checks=4861
untracked=0 … encoders=374 table=63/8192`. The generalisation is one step past the cadence rule
below it: **a diagnostic must be paced by something that moves with what it measures**, or it
reports a stale value with the confidence of a fresh one. (Note the rest of that block —
`copies:`, the unroll counts, the pass-start histogram — is paced the same way and was equally
frozen on this workload; the dogfood runs it was designed against are barrier-heavy, so its
numbers there are sound, but it is not a general-purpose reporter.)

Table occupancy of 63/8192 on a full desktop session also puts a number on the eviction residual
above: it is nowhere near the pressure needed to lose a live encoder's slot.

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

1. **Hit the midpass-unroll path** — but do not treat any particular rate as the threshold. The
   fifth crash came out of a run at 0.0065 `unroll_geometry` calls per render pass (35,288 over
   5.4 M pass starts in 6 h 18 m), 21x below the ratio measured on the run that crashed before it.
   What both have in common is the *route*, not the rate: every copy that crashed came through
   `cs_get_compute(cmd, true)`, the pre_gfx path. A vehicle that takes it at all is exercising the
   code; one that never takes it is not.
2. **Attribute the copies.** "The compute encoder is busy" is not evidence that
   `kk_CmdCopyBufferToImage2` ran; the `copies: buf->img=…` line added afterwards answers that
   directly, and any future claim about reaching the path should cite it.
3. Drive the compute class past its floor of eight if the destroy path is to be tested there —
   though on this evidence destruction is a bystander, not the cause.

Note that the pool instrumentation was armed and silent *through* the fourth and fifth
occurrences: 4445 growth events in the retained log the first time, and `use-after-destroy=0`
`unmatched-discharge=0` at the last report before the fifth crash. No `KK_PA_DEAD`, no guard trip
either time. That is evidence against allocator misuse from a second direction, independent of the
disassembly.

## Catching the cause, and why a sanitizer is only half an answer

There are two ways an encoder can stop being ours, and they need different tools:

- **Freed and the address reused.** A memory error, and every malloc-level tool sees it.
- **`endEncoding` called, the object still allocated and internally reset.** *Not* a memory error.
  No sanitizer will say a word; only a state table like this one can.

The four `prepareForEnqueue` faults read as the second (a NULL pass-state field, not garbage); the
fifth reads as the first (a mapped block holding foreign values). So both are probably in play, and
"turn on ASan" cannot be the whole plan.

In rough order of cost, cheapest first:

1. **`MallocScribble=1`** in the VM's environment. No rebuild, negligible cost. Freed blocks are
   filled with `0x55`, so the next crash's registers answer the question outright: `0x5555...`
   means freed-and-not-yet-reused, plausible garbage means freed-and-reallocated, zeros mean the
   object was reset rather than freed. Every outcome is informative, which is rare.
2. **`MallocStackLoggingNoCompact=1`** beside it, so `malloc_history` can name the alloc and free
   stacks for the faulting address — that is "who freed it", the cause itself.
3. **`NSZombieEnabled=1`** catches a message to a freed encoder by class and selector. Precise,
   but zombies are never freed, so it suits a bounded dev-Mac repro, not a multi-day session.
4. **ASan on KK only.** Its runtime interposes malloc process-wide, so AGX's own allocations are
   covered too, and it gives alloc + free stacks at the bad access. But it needs library
   validation disabled, costs 2-3x, and above all it needs a *reproducer* — which is the thing we
   do not have. It is the tool for after a vehicle exists, not for catching this in the wild.
5. **Guard Malloc** is decisive and far too heavy for a seated desktop.

**But the guard has already made the cheapest oracle the best one.** It converts what used to be a
crash into a *refusal* — a live, non-fatal moment at which both the code holding the stale pointer
and the code that took the address still exist. So the table records the frames that created and
ended each incarnation, and a refusal prints both. That is cause attribution with no sanitizer, no
slowdown, and nothing that a post-mortem crash report could ever have supplied, because by then
one of the two parties is gone. **A guard that survives the fault is worth more than a tool that
describes it afterwards.**

The dispatch ring was **not** armed on the fifth run — `LIMINA_KK_POOL_SNAPSHOT` was unset, so no
`.dispatch.*` file exists beside the logs — which cost the one piece of evidence that named the
faulting pointer's history last time. A dogfood launch should set it.
