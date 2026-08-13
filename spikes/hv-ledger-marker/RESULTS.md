# hv-ledger-marker: how stage-2 mappings gate MADV_FREE_REUSABLE's ledger debit

**Date**: 2026-08-11 (+ the `double` mode 2026-08-12). Host: macOS 26.5, M1 Max 32 GB,
16 KiB pages.
**Question** (from the field arithmetic): the dogfood worker bills ~6.5 GB of
phys_footprint that appears in NO vmmap category — guest content + graphics + malloc
cannot reach the billed total, and the footprint peak hit 37.4 GB on a 24 GiB VM.
hv-ledger-gap round 8 suspected per-mapping (task pmap + HV stage-2 pmap) double-billing.

**Method**: `probe.c` + `payload.S` — a real HVF VM: 3 GiB RAM (`MAP_ANON|MAP_PRIVATE`,
production shape; `shared` mode too), one vCPU. H-range (1 GiB) dirtied by the HOST through
the mmap; G-range (1 GiB) dirtied by the GUEST (one u64 per 16 KiB page, so stage-2 PTEs
exist only for G). Optional ballast phase compresses both. Then the release recipes, with
`task_vm_info` (phys_footprint/compressed) snapshots around every step.

## Results (all four mode combinations agree)

| Step | footprint Δ | takeaway |
|---|---|---|
| host memset H 1G | +1024.5 M | billing is 1× at touch |
| guest dirties G 1G | +1024.7 M | **no static 2× — per-mapping double-billing at touch FALSIFIED** |
| ballast compresses G+H | ±0 | **no phantom minted at compression** (fp flat while comp +512 M) |
| REUSABLE on G, stage-2 PTEs LIVE | **+0.1 M** | **no immediate debit — resident AND compressed shares stay billed** |
| hv_vm_unmap G afterwards | **−1024.5 M** (and −254.8 M comp in the compressed run) | the unmap settles the whole range instantly |
| hv_vm_unmap H (no stage-2 PTEs) | ±0 | stage-2 population is lazy: nothing to tear down for host-only-touched pages |
| REUSABLE on H (mapped, host-faulted) | **−1024.0 M** | with no stage-2 PTEs the kill is immediate, mapping coverage is irrelevant |
| vcpu destroy / hv_vm_destroy / munmap | ~0 | no residue in these shapes (the field graveyards did not reproduce here) |

Mode matrix: {resident, compressed} × {MAP_PRIVATE, MAP_SHARED} — identical behavior.
The discriminating cell (`h-mapped-first`) isolates the condition: **guest-fault-populated
stage-2 PTEs**, not the hv mapping itself.

## The model

- Pages with **no stage-2 PTEs**: `MADV_FREE_REUSABLE` kills immediately — resident pages
  and compressor slots freed, ledger debited synchronously (matches
  spikes/balloon-compressor-zero and the xnu `vm_object_deactivate_pages` read).
- Pages with **live stage-2 PTEs** (guest-faulted): the madvise returns 0 but debits
  NOTHING immediately — the pv-list still holds the EL2 mapping the madvise walk won't
  disconnect. The pages sit in **limbo**: billed until the asynchronous pageout scan gets
  around to disconnecting and processing them (pressure- and time-dependent). This is
  round 7's "limbo"/"scan re-bills" observation, now isolated to its trigger condition.
- **`hv_vm_unmap` is the deterministic settlement**: it tears down the stage-2 PTEs and
  debits the full range (resident + compressed) instantly.

This reconciles everything that looked contradictory:
- The pre-fix L1 FRQ test was green because it polls 90 s — the scan settled the limbo
  within the window. The probe snapshots instantly — it sees the limbo.
- The historical hv-ledger-gap excess (uncategorized multi-GB footprint, the 2.04:1
  ratios, the 37 GB peak): under a constant-pressure, high-churn field workload the
  rolling limbo population is large — releases were only ever settled at the scan's
  leisure, never deterministically.
- The 2026-08-10 unmap fix, shipped for the *trampling* bug, **also fixed the billing
  latency**: release() now unmaps before REUSABLE, so every release settles instantly.
  P4 (unmap → REUSABLE) is exactly the production sequence and is the best of all
  measured recipes.

## Cycle mode (same day): the field loop does NOT reproduce the post-fix excess locally

`cycle` / `cycle-pressure` modes run the actual field loop — release (unmap+REUSABLE,
production shape) → guest re-touches every page → 512 chunked heals (REUSE + 2 MiB remap,
production shape) → release again — 10 times, optionally with 14 G of held ballast.
Result: **zero drift**. Every release debits exactly 1024 M, every re-touch re-bills
exactly 1024 M, teardown returns to baseline. (Caveat: in the pressure run the compressor
never engaged on the target — the 16 G working set still fit. The single-shot compressed
runs cover that cell: unmap debits the compressed share cleanly.)

So the post-fix dogfood excess (~22 G billed vs ~7.4 G guest-visible) is **NOT replicated**
by any local shape tried: single-shot × {resident, compressed} × {private, shared}, and
cyclic × {ambient, pressured}. What the field has that these probes don't: 24 G scale with
192-piece fragmentation, 10 vCPUs healing concurrently against the release lock, the
virtio-gpu SHM-window map/unmap churn interleaved, s2idle/park cycles, and hour-scale
duration. Also unresolved: the 2.07× disagreement observed between the balloon socket's
`target=actual` and the decision-trace's view of the same instant (2026-08-11 field
snapshot) — pin that down before trusting any single instrument.

## The both-touch cell (`double` mode, 2026-08-12): per-pmap double-billing CONFIRMED

The original matrix touched each range from exactly ONE side (H host-only, G guest-only)
— which is why it falsified "double-billing at touch". The cell it never measured is the
**same range touched by both sides**, and that is the production shape of every disk-fed
guest page: virtio-blk writes the buffer through the task mapping, the guest then faults
the same page through stage-2. Field trigger (2026-08-12, dogfood, cold boot + gdb
coredump analysis): ledger internal 33.0 G / phys_footprint 34.2 G while the footprint
tool saw ~17 G real and compressed-billing ≈ 0 — an almost exact 2× on a guest whose
page cache was built by disk reads.

`./probe payload.bin double` (payload gained a retarget mailbox at RAM_BASE+0x10000):

| Step | footprint Δ | takeaway |
|---|---|---|
| D1 host memset H | +1024.7 M | 1× at first touch, as before |
| D2 guest dirties the SAME H | **+1024.5 M** | **CELL A: the second pmap bills again** |
| D3 guest dirties G | +1024.5 M | 1× at first touch |
| D4 host memset the SAME G | **+1024.5 M** | **CELL B: symmetric — order doesn't matter** |
| D5 mprotect(H, PROT_NONE) | **−1024.5 M** | instantly debits exactly the task-pmap share |
| D6 mprotect(H, RW) restore | ±0 | PTE re-population is lazy |
| D7 guest re-touches H | ±0, **0 stage-2 faults** | **the guest never notices the sweep**; content intact |
| D8 host re-memsets H | +1024.5 M | the sweep's steady-state cost on host-hot pages |
| D10/D11 mprotect(H, PROT_READ) round-trip | ±0 | RO downgrade edits PTEs in place — **only the NONE window debits** |
| D9 hv_vm_unmap all / munmap | −2049 / −2049 | perfectly symmetric: each pmap's teardown debits its own share |

**The model, completed**: phys_footprint (and resident_size) bill **per pmap** — the task
pmap and the HV stage-2 pmap each carry a full share for the same physical page. One
toucher = 1×; both touchers = 2× until one side's PTEs go away (hv_vm_unmap, munmap,
mprotect(PROT_NONE), or the compressor's pageout disconnecting both). This retroactively
explains the historical 2.04:1 field ratios and the 08-12 cold-boot 2×: the guest's
entire disk-fed page cache (12.3 G that night) is both-touched by construction, so it
bills ~2×, plus anon + worker overhead ≈ the observed 33 G.

**Remedy facts for the production design**:
- An `mprotect(PROT_NONE → RW)` cycle over guest RAM debits the task share, leaves
  stage-2 (and the guest) completely undisturbed, and preserves content. Zero guest cost.
- The window is the hazard: any WORKER thread touching the range during the NONE window
  takes a SIGBUS/SIGSEGV (virtio queues, blk/net buffers, gpu transfers). A sweep must
  chunk + skip hot ranges, quiesce workers, or field the fault with a retry handler.
- PROT_READ windows don't work (no debit), so reads can't be kept safe that way.
- Host re-touch re-bills 1× (D8): swept pages the worker still serves IO into come back.
  Sweep on cadence/pressure, not once.

## Production consequences

1. **release() is optimal as shipped** — no janitor, no rescan, no memset, no MADV_ZERO.
2. Pre-fix builds effectively had *scan-latency-deferred* reclaim; any historical
   measurement of "REUSABLE returned X quickly" on a stage-2-mapped range was really
   measuring the scan, not the madvise.
3. ~~The remaining dogfood gap on the POST-fix build still needs the planned
   discriminator~~ — RESOLVED by the `double` mode: the post-fix excess IS the both-touch
   2× on disk-fed (and any other worker-written) guest pages. The balloon bounds it only
   by shrinking the cache; released ranges settle because hv_vm_unmap+munmap-side debits
   both shares.
4. The user-facing fix is a task-pmap "settle sweep" (mprotect NONE→RW over guest RAM the
   worker has written), gated by the hazard notes above — mechanism in libkrun, policy in
   limina, same split as the balloon. Alternative doors: Apple's
   MAP_MEM_LEDGER_TAGGED/NO_FOOTPRINT ownership transfer (likely private-entitlement-gated
   — verify, then Radar) or simply reporting honest numbers in limina's own UI while AM
   stays 2×.
