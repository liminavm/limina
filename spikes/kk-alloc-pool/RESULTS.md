# The KosmicKrisp command-allocator pool, measured

Instrumentation raised for the dogfood SIGSEGV that has now killed `limina-vmm` six times: a
store through a pointer AGX read out of its own compute-context state, reached from a guest GL
texture upload (`vrend … transfer write` → zink `zink_copy_image_buffer` →
`kk_CmdCopyBufferToImage2` → `mtl_copy_from_buffer_to_texture`). The allocator behind the encoder
is ours (`kk_device.c`), so the question this spike first asked is whether we destroyed, reset, or
overfilled one under a live encoder.

**The pool is not misused, and neither is the encoder KK can see.** Both KK-side guards were armed
and silent through the latest crash. What is left is the AGX compute context *behind* the encoder
— see the next section. The measurements further down stand as what the pool actually does under
real workloads, and the vehicle requirements at the end still hold for anything that wants to
exercise this code.

## What the fault actually is

Disassembly of AGXMetalG16X puts the fault at `prepareForEnqueue+672`, `str x8, [x9, #0x98]` with
`x9` loaded from `ComputeContext+0x918` (`x19` is the context). That field is the compute-pass
state block. Across the whole arm64e slice (build `84D26FE7`), the **only** instructions that store
to `+0x910` or `+0x918` on a context are the two in `beginComputePass` (one per HAL variant:
`newCommand(...)` into `+0x910`, that `+0xc0` into `+0x918`, which cannot be zero); no end, reset
or teardown path clears either field. AGX runs `beginComputePass` at `[cmd_buf
computeCommandEncoder]`, so every encoder KK is handed has been begun. A NULL there therefore means
the context's memory was zeroed or re-initialised **wholesale**, and nothing has begun a pass on it
since.

Five occurrences landed at that site. One landed at
`MSLBlitDispatchContext::blitCDMTextureToTexture+840`, a different structure:

    +824  ldr  x9,  [x19]           ; the blit dispatch context's encoder state — mapped, writable
    +828  ldr  x10, [x9, #0x18]     ; base pointer  -> 0
    +832  ldr  w9,  [x9, #0x4]      ; 32-bit offset -> 0x21f6d26f
    +836  add  x9,  x10, x9
    +840  str  x8,  [x9, #0x8]      ; FAULT at 0x21f6d277, storing the uber-blit pipeline address

A single uninitialised field would keep faulting in the same place. Two unrelated fields in two
unrelated structures, selected by which shape the copy takes, is what AGX state that is no longer
the encoder's looks like: whatever AGX touches first is what faults.

### The context is not the object KK holds

The faulting context is a **separate allocation** behind the ObjC encoder. On the run that had the
crash-surviving dispatch ring armed, the copy in flight at the fault used encoder `0xb612cb660`;
the faulting context `x19` was `0xb5fa74000`, and appears nowhere in the ring. Every faulting `x19`
is page-aligned (`0xb5fa74000`, `0xbe31dc000`); no encoder pointer ever recorded is.

### The encoder KK passed was its own, live, current incarnation

KK stamps a **generation** per compute-encoder address (`mtl_encoder.m`): set at
`mtl_new_compute_command_encoder`, moved to ENDED at `mtl_end_encoding` and to RELEASED before the
last `mtl_release`. `kk_encoder_state` records the generation it was handed, `cs_get_compute` and
`kk_stop_encoder` refuse when the address now holds a different one, and every compute record
entry point — including `mtl_copy_from_buffer_to_texture`, immediately before the faulting
`copyFromBuffer:` — checks the address is present and LIVE.

The sixth crash ran on that build for 2 d 2 h 54 m. Its last guard line, from the final seconds:

    encoder guard: checks=249801960 untracked=0 | bad: null=0 ended=0 released=0 |
                   refused: handout=0 close=0 | encoders=63706059 table=2140/8192

So on the faulting call KK's encoder was in the table (the eviction hole was not involved), LIVE,
and the same incarnation `cs_get_compute` had handed out.

The crash-surviving dispatch ring for that run (`dogfood-2026-09-11/dispatch-ring-72232.bin`) says
the same from the other side, and adds one fact. The copy in flight — a 266x54 upload — was the
**first copy recorded on a brand-new encoder**: generation 63706106, LIVE, at address
`0xbed4b9cc0`, which generation 63706105 had occupied for the eleven copies before it. That is not
rare in itself — the last 4096 copies ran through only three encoder addresses and 318
generations, so 7.8% of all copies are the first on their encoder — but it fits a context that
never had its pass begun, or had it undone, between encoder creation and first use. **KK never ended, released, or re-created
the encoder it passed**, and never closed anyone else's either. A stale KK-side pointer used across
an address recycle is excluded for this crash.

The rule that falsification leaves behind: **a liveness table proves the calls it hooks did not
fire; it says nothing about the calls it does not hook — and it can only key on the object it can
see.** Here the object that went bad is one level below it.

KK's other lifecycle paths were read against this and are clean: `kk_reset_cmd_buffer_internal`
closes lingering encoders only through `cs_end` → `kk_stop_encoder` (tracked);
`mtl_end_command_buffer` is called only in `kk_stop_encoder`, after `mtl_end_encoding`; the pool
resets an allocator only when it is neither borrowed nor has pending work, and
`kk_alloc_pool_take_surplus` never picks one that is.

### What remains

- **AGX re-initialises or re-issues a compute context while an encoder still refers to it.**
  Nothing KK does is known to cause that, and a page-aligned context is consistent with it living
  in heap memory AGX manages — possibly the command allocator's, which KK reuses legally the
  moment a command buffer ends.
- **A second party on the same encoder or context** that the crash reports do not show. The guard
  has a check-then-use window, so a concurrent end between the check and the call would leave
  every counter at zero. No thread in any report is on a stop path at the fault, so this is a
  limit of the evidence, not a lead.

Concurrent KK recording on *different* command buffers is normal here and is not evidence of
either: the sixth crash had a zink driver thread in `kk_draw` (`kk_flush_dynamic_state`, cmd
`0x399ef8000`, which appears nowhere in the faulting thread's registers), and the second-site
crash had a zink flush thread in `reset_batch_state_internal`. The other three reports in the
repo show no second thread in KK at all.

The discriminating next step is to make the context visible: record, per encoder, the context AGX
attached at creation and compare it at each use; and record which allocator each open encoder is
on, so an allocator reset or re-begin can be checked against open encoders.

The route every crashing copy took is unchanged: `kk_CmdCopyBufferToImage2` →
`cs_get_compute(cmd, true)`, the pre_gfx slot.

### Guard liveness, and what pacing it wrong looked like

`[LIMINA-ALLOC-POOL] … encoder guard:` carries, every report, the checks performed, the count that
missed the table, the per-state bad counts, both refusal counts, encoders seen, and table
occupancy. A rising `checks` with zeros beside it is evidence; a frozen `checks` means the needle
is dead. A refusal prints the frames that created and ended the incarnation.

That line was first put on the `[LIMINA] KK counts:` block, which is driven by
`kk_CmdPipelineBarrier2`; a seated F44 desktop running Firefox and glmark2 issued so few barriers
that it printed **three times in a whole boot**, reading `checks=2` all session. On the pool
report, paced by encoder closes, the same workload reads `checks=4861 … encoders=374
table=63/8192`. **A diagnostic must be paced by something that moves with what it measures**, or
it reports a stale value with the confidence of a fresh one. The rest of that block (`copies:`,
unroll counts, pass-start histogram) is paced the same way; the dogfood runs are barrier-heavy
enough that its numbers there are sound, but it is not a general-purpose reporter.

Table occupancy stays far from eviction pressure: 63/8192 on a short desktop session, 2140/8192
after two days of dogfood, `untracked=0` throughout.

`kk_alloc_pool_report()` prints per class on the release path (`[LIMINA-ALLOC-POOL]`, on the clock
and at device teardown):

    live / peak / retired | size hiwater vs budget, retirement count | peak ops per command buffer
    tombstones, use-after-destroy

A destroyed allocator keeps its struct, stamped `KK_PA_DEAD`, so any stale pointer is named at the
call that used it. `LIMINA_KK_ALLOC_GUARD=abort` turns the report into a core dump there;
`LIMINA_KK_ENC_GUARD=abort` does the same for the encoder guard.

`x13`/`x14`/`x15` (`0x20000`, `0x10001f`, `0x100000`) are invariant across the `prepareForEnqueue`
occurrences and are register state left by the sampler-heap `addToResourceList` call, not the
faulting operand. The dispatch in flight at a fault is ordinary — the ring named a `45x32` glyph
upload indistinguishable from its neighbours. **Nothing about size, count, or pool occupancy is
the variable.**

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

1. **Hit the midpass-unroll path** — but do not treat any particular rate as the threshold. One
   crash came out of a run at 0.0065 `unroll_geometry` calls per render pass (35,288 over 5.4 M
   pass starts in 6 h 18 m); the sixth out of one at 0.15 (20.7 M over 138 M). What they share is
   the *route*, not the rate: every copy that crashed came through `cs_get_compute(cmd, true)`, the
   pre_gfx path. A vehicle that takes it at all is exercising the code; one that never takes it is
   not.
2. **Attribute the copies.** "The compute encoder is busy" is not evidence that
   `kk_CmdCopyBufferToImage2` ran; the `copies: buf->img=…` line answers that directly, and any
   future claim about reaching the path should cite it.
3. Drive the compute class past its floor of eight if the destroy path is to be tested there —
   though on this evidence destruction is a bystander, not the cause.

The pool instrumentation was armed and silent through the fourth, fifth and sixth occurrences:
`use-after-destroy=0`, `unmatched-discharge=0`, no `KK_PA_DEAD`, no guard trip. That is evidence
against allocator misuse from a second direction, independent of the disassembly.

## Catching the cause

The KK-side guards are now spent as oracles for this fault: they name misuse of the encoder and
the allocator, and neither is misused. What they cannot see is the AGX context, so the tools that
remain are the ones that see memory:

1. **`MallocScribble=1`** in the VM's environment. No rebuild, negligible cost. If the context is
   a malloc block, a freed one is filled with `0x55`, so the next crash's context contents separate
   freed-not-reused from reallocated from reset-in-place. If it is not malloc memory (a page-aligned
   context suggests it may not be), the scribble stays out of it, and that is itself an answer.
2. **`MallocStackLoggingNoCompact=1`** beside it, so `malloc_history` can name the alloc and free
   stacks for the faulting address.
3. **ASan on KK**, which interposes malloc process-wide — for after a reproducer exists.
4. Guard Malloc is decisive and far too heavy for a seated desktop.

The dispatch ring lands at `<LIMINA_KK_POOL_SNAPSHOT>.dispatch.<pid>`. The supervisor points that
at the VM bundle's `logs/` only when the variable is unset; an explicit value set through
`launchctl setenv` wins, and the ring is then wherever that names — look there before concluding
it was not armed.
