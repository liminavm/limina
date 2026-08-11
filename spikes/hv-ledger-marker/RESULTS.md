# hv-ledger-marker: how stage-2 mappings gate MADV_FREE_REUSABLE's ledger debit

**Date**: 2026-08-11. Host: macOS 26.5, M1 Max 32 GB, 16 KiB pages.
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

## Production consequences

1. **release() is optimal as shipped** — no janitor, no rescan, no memset, no MADV_ZERO.
2. Pre-fix builds effectively had *scan-latency-deferred* reclaim; any historical
   measurement of "REUSABLE returned X quickly" on a stage-2-mapped range was really
   measuring the scan, not the madvise.
3. The remaining dogfood gap on the POST-fix build still needs the planned discriminator
   (per-tick `released−remapped` vs the guest-RAM region's billed bytes) — but the limbo
   mechanism no longer applies to released ranges there, so the suspects narrow to live
   content aliased by the oscillation plus anything the balloon never releases.
4. Local probe space is exhausted without a replication; the next replication vehicles are
   the REAL stack: (a) the deployed counters build on dogfood (outstanding-vs-billed
   correlation), (b) a local limina run with the ledger sampler attached under the
   compile-mix A/B workload — full scale, multi-vCPU, GPU churn included.
