# Spike: madvise reclaim vs phys_footprint on an HVF-mapped region

**Date:** 2026-05-30  **Host:** macOS 26.5 (25F71), Apple M1 Max, 32 GB, arm64.
**Code:** `footprint.c` (+ `hv.entitlements`, `run.sh`). Region = 1 GiB MAP_ANON,
host page = 16 KiB, mapped at guest IPA 0x40000000 via `hv_vm_map` with R/W/X.

## Question

Does reclaiming guest RAM with `madvise()` actually lower `phys_footprint` (the
number macOS bills the process / shows in Activity Monitor) **while the region is
mapped into a VM via `hv_vm_map`** — i.e. is virtio-balloon reclaim viable on
macOS/HVF at all, and is libkrun's existing `MADV_DONTNEED` the right call?

## Results (footprint after reclaim; committed ≈ 1026 MiB, baseline ≈ 1.5 MiB)

| madvise mode        | hv_vm_map'd | unmap first | after reclaim | reclaimed? |
|---------------------|:-----------:|:-----------:|--------------:|:----------:|
| MADV_FREE_REUSABLE  | no          | –           | 2.0 MiB       | ✅ full    |
| MADV_DONTNEED       | no          | –           | 1026.0 MiB    | ❌ none    |
| MADV_FREE           | no          | –           | 1026.0 MiB    | ❌ lazy    |
| MADV_FREE_REUSABLE  | **yes**     | no          | 2.0 MiB       | ✅ full    |
| MADV_DONTNEED       | **yes**     | no          | 1026.1 MiB    | ❌ none    |
| MADV_FREE           | yes         | no          | 1026.1 MiB    | ❌ lazy    |
| MADV_FREE_REUSABLE  | yes         | yes         | 2.0 MiB       | ✅ full    |

All `madvise` calls returned `rc=0` (success) in every row — including the ones
that freed nothing. **A 0 return from `madvise` does not mean memory came back.**

## Findings

1. **`MADV_DONTNEED` does NOT drop `phys_footprint` on macOS 26.5** — mapped or
   not, the full 1 GiB stays billed to the process (rc=0 regardless). This
   **confirms** doc 08's original claim and means libkrun's current reclaim call
   (`balloon/device.rs:100`) returns *nothing* to the host on macOS. The earlier
   "DONTNEED already works on macOS" assertion (a mid-spike misread of this very
   table) is **wrong and retracted.**

2. **`MADV_FREE_REUSABLE` is the correct fix.** It drops the full 1 GiB → ~0,
   **even while the region is actively `hv_vm_map`'d**, and **without** needing
   `hv_vm_unmap` first (the unmap-first row is identical to the no-unmap row).
   This is exactly the libmalloc primitive macOS accounts against `phys_footprint`.

3. **`hv_vm_map` does not pin/wire the pages.** The map itself doesn't change the
   footprint, and reclaim works on the mapped region. So **balloon-style dynamic
   memory is viable on macOS/HVF** — we do *not* have to unmap/remap the IPA range
   to return RAM; the right `madvise` on the backing `MAP_ANON` is sufficient.

4. **`MADV_FREE` is lazy** — footprint holds until real memory pressure. Not
   usable for prompt, accountable return; only a "soft" hint.

Net: the open question "does reclaim work while `hv_vm_map`'d?" is **yes**, and
the right primitive is **`MADV_FREE_REUSABLE`/`MADV_FREE_REUSE`**, not the
`MADV_DONTNEED` libkrun ships. doc 08's Phase-1 task #1 stands, now confirmed.

## NOT tested here (still open)

- **4 KiB ↔ 16 KiB granularity.** This spike reclaimed the whole region, perfectly
  16 KiB-aligned. Real free-page reporting hands sub-ranges; whether stock Fedora
  reports in ≥16 KiB-aligned runs (so `madvise` actually frees) is untested — still
  the key alignment risk in doc 08.
- **Re-fault / re-validate cost** of `MADV_FREE_REUSE` on the deflate path
  (guest re-touches reclaimed pages) — not measured.
- **Inflate/target mechanism.** Independent of the reclaim primitive, the
  inflate/deflate handlers and a host-driven `num_pages` target are still missing
  in libkrun (`event_handler.rs:14-40`); they must be built regardless.

## Method note (why the first write of this file was wrong)

The conclusions in an earlier draft of this file were written from *expectation*
before the matrix output was read carefully, and inverted findings 1–2. The table
above is read directly from two independent `run.sh` executions (both agree). The
`MADV_FREE` rows holding at 1026 MiB prove the measurement discriminates — it is
not always reporting baseline. Lesson: read the numbers, then write the claim.

## Repro

```
./run.sh            # builds, ad-hoc codesigns with com.apple.security.hypervisor,
                    # runs the 7-case matrix
./footprint <dontneed|free|reusable> <use_hvf 0|1> <unmap_before 0|1>
```
