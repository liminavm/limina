# balloon-compressor-zero: does MADV_ZERO beat MADV_FREE_REUSABLE for released ranges?

**Date**: 2026-08-11. Host: macOS 26.5, M1 Max 32 GB, 16 KiB pages.
**Question** (from the dogfood post-fix field snapshot): the worker bills ~14 GB of
compressor/swap-held dirty pages while the balloon holds ~18 GiB released; hypothesis was
that `MADV_FREE_REUSABLE` never touches compressor-held copies, and `MADV_ZERO` might be the
"contents are garbage, stop billing" primitive the release path wants.

**Method**: `probe.c` — dirty a 2 GiB anonymous region, force ~25% of it into the compressor
(`MADV_PAGEOUT` is refused (`ENOTSUP`) for a normal process; 22 GiB of transient ballast does
it), then apply a different recipe per 512 MiB quarter with `task_vm_info`
(phys_footprint / compressed) snapshots around every step, `mincore` residency counts, and
post-advice content probes.

## Result (run 2026-08-11)

Pre-advice state per quarter: ~385 MiB resident + ~127 MiB compressed.

| Quarter | Recipe | footprint Δ | compressed Δ | reads after |
|---|---|---|---|---|
| Q1 | FREE_REUSABLE only (current release()) | **−512.0 MiB** | **−128.2 MiB** | mostly pattern (not yet scavenged) |
| Q2 | ZERO only | **±0** | **±0** | all zero |
| Q3 | ZERO → FREE_REUSABLE | −385.1 MiB | **±0 (stuck!)** | all zero |
| Q4 | FREE_REUSABLE → ZERO | −512.0 MiB | −126.1 MiB | all zero |

madvise cost: 2–5 ms per 512 MiB for every variant.

## Findings

1. **The hypothesis is FALSIFIED as stated: `MADV_FREE_REUSABLE` DOES free compressor-held
   copies** — immediately, no pageout-scan latency: Q1's full quarter (resident + compressed)
   left phys_footprint the instant the call returned, and task `compressed` dropped by the
   quarter's whole compressed share. (Residency per mincore is unchanged — reusable pages
   stay resident until scavenged, they just stop being billed; content survives until then.)
2. **`MADV_ZERO` is a semantic wipe whose billing never clears.** Contents read as zero
   afterwards, but phys_footprint and compressed are both **unchanged**. Per xnu source
   (below): the compressor *slot* is actually freed, but ZERO does no pmap/ledger fix-up,
   so the stale compressed-marker PTE keeps billing the task until that VA is re-faulted.
   It answers "make this range zeros cheaply", not "stop billing me".
3. **ZERO *before* REUSABLE is actively harmful**: Q3's compressed share survived both calls
   and stayed billed (−385 = resident share only). Mechanism (source-confirmed): ZERO frees
   the slot silently; REUSABLE's deactivate walk then finds the page neither resident nor
   paged-out — nothing to process — so the stale billing marker is never cleared. Do not put
   MADV_ZERO in the release path.
4. `MADV_PAGEOUT` is unusable from a normal process (ENOTSUP) — a kernel-build gate
   (DEVELOPMENT/DEBUG only), not an entitlement; nothing unlocks it on release kernels.

## xnu source corroboration (agent read at tag xnu-12377.121.6, macOS 26.x era)

Full report: session scratchpad `xnu-compressor-semantics.md`. The probe and the source agree:

- `MADV_FREE_REUSABLE` → `vm_object_deactivate_pages(kill_page=1, reusable=TRUE)`: for a
  paged-out page it calls `vm_object_compressor_pager_state_clr` (→ `vm_compressor_free`,
  the slot genuinely freed) **and** `pmap_remove_options(PMAP_OPTIONS_REMOVE)` which clears
  the compressed-marker PTE and debits internal_compressed/phys_footprint in the same pass.
  The `all_reusable` whole-object path is compiled out; subranges always take this walk.
- `MADV_ZERO` (macOS 14.4+): frees the slot for paged-out pages, zeroes resident pages **in
  place** (wired included), no ledger fix-up — billing sticks until re-fault. No entitlement,
  but refuses shadowed/copy objects (EPERM/ENOTSUP).
- Subrange `mach_vm_deallocate`: pmap markers cleared (caller's billing fixed) but the
  object's pages and slots in that offset range are NOT freed while sibling entries hold
  references — they linger object-wide until the last ref drops. The graveyard pattern.

**Caveats from the same read that now drive the field investigation** (dogfood: 13.9 GB
compressor-held on a released-up VM — the probe's simple shape cleans compressed pages
perfectly, so the excess needs a differential):

- **Mid-compression skip**: pages in `vmp_laundry`/`vmp_cleaning` are silently skipped by
  the REUSABLE walk and their slots never cleared. Under the dogfood oscillation (constant
  pressure, releases racing active compression), every cycle can strand a slice — an
  accumulating leak with exactly the field's shape. Mitigation candidate: a low-frequency
  janitor pass re-issuing REUSABLE over the still-released set (the exact range set already
  lives in `ReleasedRam`).
- **Silent no-op conditions**: the kill is a KERN_SUCCESS no-op on shadowed objects or
  iokit_acct/!use_pmap entries — madvise returning 0 proves nothing. Verify state, not rc.
- **Second-pmap billing** (hv-ledger-gap round 8): REUSABLE cleans only the calling pmap's
  markers; anything billed via the stage-2 pmap (closed-source side) is untouched. Post-fix
  we `hv_vm_unmap` first, which *should* clear stage-2 markers — unverifiable from source.
- **Lazy swapfile compaction**: freed slots release swapfile space only when c_segs compact,
  so a pinned 5 GB swapfile is NOT evidence the slots are still allocated.
- The dogfood 13.9 GB could also partly be non-guest-RAM untagged allocations (venus ring
  shmem etc.) — worth a vmmap region-level attribution pass before deeper probing.

## Verdict for the release path

Keep `release()` exactly as it is (`hv_vm_unmap` + `MADV_FREE_REUSABLE`). MADV_ZERO adds
nothing (Q4) and placed defensively-early it strands compressed billing (Q3).
