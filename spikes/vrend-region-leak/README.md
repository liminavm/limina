# vrend GL region-leak repro

Repro oracle for the **open** regression found in the 2026-08-08 perf pass: the worker's
`IOAccelerator (graphics)` allocations ratchet ~9–12k regions / ~1.3 GB per GL workload
open/close cycle and never return. Full results and the scoping A/B:
`perf/2026-08-08-remeasure.md` §Memory.

## Running it

Boot the enhanced tier with `--net` and a seated desktop (see
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`), then:

```bash
spikes/vrend-region-leak/memcycle.sh        # shipped GL (vrend)   -> RATCHETS
spikes/vrend-region-leak/memcycle-zink.sh   # GL forced to zink->venus -> CLEAN
```

Each drives a 5 000-fish WebGL aquarium through `systemd-run --user --unit=ff-bench`, snapshots
`vmmap --summary` on the worker (and, in `memcycle.sh`, the supervisor) at open and at closed,
and settles 30 s after each close. Both assume ssh on port **2222** — `limina --net`
auto-allocates from 2222 up, so read the real port off the supervisor log and edit `SSHO`/`SSH`
if you are running more than one VM.

## Reading the output — the one trap

**Compare closed-to-closed, never open-to-closed.** Within a single cycle the close *looks*
like it releases (the numbers drop), which hides the ratchet completely. The signal is that
each successive *closed* state sits ~9–12k regions above the previous one:

```
closed 129 666 -> closed 138 876 -> closed 147 679 -> closed 160 028
```

A cache fills once and plateaus; this is linear. The fresh-cold-boot control is **3 851 regions
/ 600 MB / 4.3 GB footprint** — take one after a reboot to anchor the arithmetic.

`vmmap` takes minutes per snapshot once the region count is high, so a full three-cycle run is
slow — that slowness is itself part of the symptom.

## What is already ruled out

- **Not the 08-07 IOSurface scanout leak** — IOSurface counts return cleanly every cycle on both
  the worker and the supervisor.
- **Not KosmicKrisp/Metal, not virglrenderer core** — `memcycle-zink.sh` exercises both just as
  hard and returns every byte (159 973 → 176 345 → 159 911, footprint back to 24.0 G exactly).
- **Not bounded by `LIMINA_GPU_MEM_BUDGET_MIB`** — that ledger counts venus blob allocations;
  this reached 17.3 G against an 8 192 MiB default cap.

So the search is the vrend GL path: the EGLImage-backed vrend scanout (vrend rendering *into*
the display IOSurface) and the classic-gbm venus import work of 2026-08-05/06.
