# gpuscore in Firefox: the pool follows the guest, and Firefox holds until it exits

The workload that produced this morning's 40 G footprint ratchet, run again with both ends of
the graphics pool and the whole-VM memory sampled on one clock (15 s cadence). Phase boundaries
are the driver's stated marks (`MARKS.md`), not inferred — GPU work does not move the guest blob
count the way a window open does.

**Verdict: no host-side retention.** Everything the benchmark allocated was held by *Firefox's*
process and released the instant it exited. The host tracked the guest's live set throughout.

## Phases (segment means)

| phase | host regions | guest blobs | footprint MB | compressed MB |
|---|---|---|---|---|
| 1 baseline, no Firefox | 3,685 | 50 | 12,712 | 9,498 |
| 2 Firefox open, idle | 5,734 | 67 | 12,623 | 7,420 |
| 3 page loaded, not started | 5,467 | 67 | 12,079 | 6,863 |
| 4 **benchmark running** | 25,268 | 250 | 16,211 | 6,855 |
| 5 post-benchmark, tab open | 7,647 | 349 | 14,076 | 6,317 |
| 6 tab closed, blank tab | 7,647 | 340 | 13,708 | 6,189 |
| 7 **Firefox quit** | 3,941 | 51 | 10,372 | 5,756 |

Peaks during the run: **64,414 regions** at 21:21:45 (17× baseline), **415 guest blobs** at
21:22:33, **19,688 MB footprint** at 21:21:45.

## What the phases separate

**Loading the page costs nothing.** Phase 3 sits *below* phase 2 — 5,467 regions against 5,734 —
so the launch transient is larger than the loaded page. Page load and GPU work are cleanly
separable here, which is what the two holds were for.

**The host retires during the run, not after.** Peak regions (64,414) came 48 s *before* peak
guest blobs (415), and by 21:23:05 regions had fallen to 7,912 while the guest still held 349
blobs. The two peaks move in opposite directions: the host is actively retiring mid-workload
rather than accumulating monotonically to a high-water mark.

**Closing the tab returns almost nothing — and that is Firefox, not us.** Phase 5 → 6 released
9 blobs of 349 and moved regions not at all, holding flat for over three minutes. But the
*guest's own* counter also stayed at 340, so nothing is stranded host-side: Firefox's GPU/content
process is still holding those allocations and the host is faithfully mirroring that. A reused
content process or a GPU process keeping allocations warm both fit.

**Quitting Firefox returns all of it, in one sample.** Phase 6 → 7: 340 blobs → 51 against a
baseline of 50, regions 7,647 → 3,941 against a baseline of 3,685. Same shape as the `gdm` drain
(pool to 16 regions within 20 s of the last DRM client exiting) and the window A/B (seven windows,
+2 regions). Three independent routes, one answer.

## Amplification is a distribution, not a constant

The marginal host cost per guest blob varies by more than 10× across this run:

| transition | Δ blobs | Δ regions | regions per blob |
|---|---|---|---|
| Firefox launch + page load | +17 | +1,782 | 105 |
| benchmark allocations (3 → 5) | +282 | +2,180 | 7.7 |
| font-viewer window (`../ghost-ab-round2/`) | +15 | +385 | 26 |

So "~26 host regions per guest blob" is a property of *what kind of allocation it is*. Big
benchmark textures are comparatively cheap per blob; small compositor and browser-chrome surfaces
are expensive. The expensive end is small surfaces, not the large GPU workloads one would suspect
— worth knowing before anyone tries to reduce a single multiplier.

## The 40 G ratchet was the balloon, not the pool

This morning the same benchmark drove the footprint to 40 G. Here it peaked at **19.7 G** and the
balloon never collapsed (16,351 MB at its lowest during the run, against 16,757 at baseline).
Two demand sweeps and the build's first idle scrub fired mid-run. So the morning's ratchet was
the balloon being driven by the workload's memory demand plus the double-billing the settle sweep
now debits — not the graphics pool failing to retire.

## Caveat

One sample at 21:20:24 read all zeros (the balloon-trace tail read failed) and is excluded from
the means. It sits in the gap between phases 3 and 4 and touches no segment boundary.
