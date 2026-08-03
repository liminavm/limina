# Upstreaming — divergence decisions

Cases from the 2026-08-03 patch audit where **upstream went a different way**, there is an
**alternative-implementation MR in flight**, or our shape is **known-rejected** — i.e. the
patches whose upstreaming is *not* a straight "clean the diff and send it." The clean send-now
queue and the drop/carry roll-up live in `ledger/SUMMARY.md`; this doc is only the divergences,
each framed as the decision it poses.

Sequenced deliberately **after** "bring dependencies up to date with upstream tip" — several
verdicts below are perishable (KK is under weekly LunarG development) and must be re-confirmed at
the rebased base before any of these decisions is acted on.

Five kinds. Only **A** and **B** need a real decision; **C/D/E** are drops or mechanical follow-ups.

---

## A. One capability, upstream chose a different architecture — the real fork

**The macOS Metal resource-sharing model.** Load-bearing: it is how the host renderer hands GPU
memory to Metal, the foundation of the whole venus/vrend tier.

- **Ours:** virgl `0009 / 0011 / 0013` — the krunkit raw-`map_ptr` / blob-map model (an fd you `mmap`).
- **Upstream:** MR **!1617** `VIRGL_RESOURCE_METAL_HEAP` — *typed* Metal handles, not raw pointers.

Two different contracts for the same job, not one patch in two styles. "Converging" is **not**
sending our diff — it is an engineering investment: add `VK_EXT_external_memory_metal` (MTLHEAP) to
KosmicKrisp, then re-express our IOSurface scanout on !1617's handle type.

**Blocking unknown to resolve first (cheap, decides the direction):** our #28 investigation
concluded the fd double-mmap path was *incoherent*, yet upstream ships that exact path at **1770
FPS**. Either our incoherence finding was environment-specific (a KK/MoltenVK-era artifact) or one
of the two models has a real correctness gap. Probe this before committing either way.

**Decision:** invest in KK-MTLHEAP-and-converge on !1617, **or** carry raw-`map_ptr` indefinitely as
a deliberate downstream fork. (Related carries that ride this choice: the rest of virgl cluster A.)

---

## B. In-flight upstream MRs that overlap ours — contribute the delta, don't resubmit

### B1. libkrun #762 — HVF snapshot/restore (per-patch overlap, not uniform)

| ours | vs #762 | move |
|---|---|---|
| 0050 / 0051 / 0052 | redundant (converged; their register set is narrower but no correctness gap found) | drop ours, review theirs |
| **0053** | **ours is better — #762 is single-vCPU-only and explicitly *rejects* multi-vCPU restore** | push ours on top of #762 |
| 0069 | complementary — merges cleanly (ours finer on addresses, theirs finer on device-topology ordering) | contribute |
| **0070** | **design divergence — see below** | decide the model first |
| 0081 / 0084 | pure additions #762 lacks (parallel-lz4 + zero-hole compression; atomic publish) | contribute as deltas |
| 0055 / 0056 / 0058 | add the `reset()` path #762 omits (0058 is a real memory-corruption hardening) | file independently |

**The 0070 model conflict:** #762 *host-restores* device state (pause → save queues → resume),
whereas we deliberately **re-negotiate every device on s2idle thaw**. These two models do not merge
— one has to win. Resolve this before engaging the #762 thread, because it changes what "contribute
the delta" even means for the device-transport section.

### B2. libkrun #794 — balloon reclaim

Our `0033` (MADV_FREE_REUSABLE, debits `phys_footprint` while mapped) vs #794's stage-2 unmap. The
maintainer thread **already favors our approach** (slp flagged #794's VM-exit-per-refault tax, and
noted "not convinced we need [#794]"). Move: propose 0033's madvise switch as the counter, offer the
16k ReclaimCoalescer as a correctness add-on. **Social note:** this means engaging *against another
contributor's open PR* — a judgment call, not just a technical one.

### B3. Clean adopts — upstream is asking for exactly what we have

- **0087** → open **#707** (deflate-on-oom toggle). Same knob; the only debate is default (we argue
  OFF, #707's muvm use case wants ON — one knob satisfies both).
- **0029** → open **#565** + in-flight PR **#560** (advertise only backed capsets). Align with #560's
  `capset_mask` API rather than our helper framing.

---

## C. Same bug, upstream fixed it differently — just drop (mostly informational)

Already in the DROP list; the only nuance is that upstream's *mechanism* differs from ours.

- **mesa 0017** (venus submit freelist): upstream's bounded-cache-at-retire (`09fb7ca8`, MR !43229)
  is *arguably better* than our capacity-field fix. Adopt theirs; ours drops at base ≥ 26.1.5.
- **KK timestamp arc** (0008/0010–0013): upstream is Metal-4-shaped (`MTL4CounterHeap`, per-stage
  writes), ours Metal-3 — "neither ports to the other." Vanishes as part of the **KK-is-a-rewrite-
  not-a-rebase** milestone (see below). Heads-up, not a decision.
- **linux 0005** (balloon FRQ across suspend) → upstream `0b45f69` in `mm/page_reporting.c`; **mesa
  PBO / 0009** → `479773c7e42` / !42528. Clean drops.

---

## D. Our shape is *known-rejected* — change approach, don't submit

**linux 0004** (blob alignment). Upstream merged the **negotiated** mechanism `F_BLOB_ALIGNMENT`;
a *hardcoded*-alignment shape like ours was already declined once (Finkelstein, 2021). So this is a
limina work item, not a send: have **libkrun advertise `VIRTIO_GPU_F_BLOB_ALIGNMENT`** + verify
guest-Mesa rounding — which retires **both** the patch and the DKMS module. (This is also why
libkrun 0043's host-page round-up is *not* superseded by the flag — the flag is guest-kernel-side.)

---

## E. Upstream feature may moot ours — verify at rebase

**mesa 0001** — KK gained upstream `robustness2` (!41313); probe whether it moots our patch at
runtime. If yes, drop. (Cross-listed as a mesa-series work item.)

---

## Cross-cutting: the KK rebase is a rewrite, not a rebase

Upstream KK moved to Metal 4 command encoding and **deleted `kk_encoder.c`**. The next KK base bump
is therefore a series-wide partial rewrite, not a `git rebase`: the timestamp arc and 0005 drop
outright, 0016's core hunks vanish, the monolith (0001) loses its bind-cache premise, and
0003/0006/0017 need re-anchoring. **Plan it as its own milestone**, and treat it as the trigger to
re-confirm every KK verdict here — LunarG reinvented four of our KK patches in five weeks, so
"unchanged since base" verdicts are perishable.

## Prerequisite order

1. Bring each dependency up to current upstream tip (own milestone for KK; ordinary rebase elsewhere).
2. Re-confirm the perishable verdicts against the new base (KK especially; virgl !1617 status; the
   #762/#794 threads' state).
3. Resolve the two blocking unknowns — **A**'s fd-coherence-vs-1770-FPS premise, **B1**'s 0070
   host-restore-vs-renegotiate model conflict.
4. Then act: A (invest-or-fork), B (contribute deltas / engage threads), D (the F_BLOB_ALIGNMENT
   work item).

All tracker engagement (issues, MRs, thread replies) is a **human action** — the audit and this doc
never post.
