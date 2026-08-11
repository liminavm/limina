# balloon-retention-testbed — reproduce the retention pool, grade the fixes

## Why

The hv-ledger-gap campaign settled (2026-08-11): with the ReleasedRam unmap fix in
place, the remaining host-billing excess is a **retention lag pool**, not a leak.
Pages the guest dirtied and then freed — which free-page reporting cannot re-report
(low-order / fragmented frees) — plus cold page cache get compressed under *host*
memory pressure and stay billed to the worker until the guest happens to re-touch
them. Field signature: worker `internal_compressed` of many GB against a much
smaller guest-visible live set, `reusable` ~0, and the pool *drains* when the guest
gets active. No slots strand (system unattributed compressed stayed at noise).

This testbed builds that pool on demand and measures how much of it each candidate
fix recovers. It needs no agent in the guest: the balloon is driven manually over
the control socket, and FRQ runs kernel-autonomously.

## Recipe

```
./run.sh <disk.raw> [label]
```

Phases (each logged, one CSV per run):

1. **Boot** the disk headless (EFI, `--memory $MEM` default 2G..12G, FRQ on,
   `--balloon-control-socket`), start the 10 s ledger/guest sampler.
2. **Pool build**: run the compile mix in the guest (`MIX=full`, needs a primed
   ab-run image) or a synthetic dirty-then-free pass (`MIX=touch`, any image:
   fills page cache + anon and frees it fragmented), then idle.
3. **Pressure**: hold pool-sized incompressible ballast on the host
   (free+inactive+speculative+purgeable − 2G, capped) until worker `ic_bal`
   plateaus. **Reproduction achieved** when
   `pool = ic_bal − guest_live` (guest_live = MemTotal − MemAvailable) is ≥ a few GB.
4. **Scrub grade** (`SCRUB=1`, default): `target <max>` on the balloon socket
   (full inflate — the guest hands over its free pages, whose stale host copies
   release() then settles instantly), hold, `target 0` (deflate). The drop in
   `ic_bal` across the cycle = what a scrub policy would recover.

## Metric and predictions

`pool_before`, `pool_after_scrub`, and the drop, all from the same ledger sampler.

- **Scrub cycle** (pure policy): predicted to settle the dead-dirty share —
  if the drop is most of the pool, a periodic/pressure-triggered scrub is the fix.
- **MemFree clamp** (balloon-bench mandatory lever): grade by re-running the
  oscillating profile with the clamp and comparing steady-state pool.
- **Host-page-aware / lower-order FPR** (guest kernel, roadmap): grade by
  steady-state pool vs stock kernel on the same phases.
- Residue that a scrub cannot settle is the only remaining reason to suspect real
  stranding (bounded ≤1.66G system-wide in the field) — only then build a
  mid-laundry release probe.

## Notes

- Reuses `../hv-ledger-gap/ledger-dump.c` and `../hv-ledger-gap/ballast.c`
  (built on first run).
- Never run while an HVF test suite is running.
- Guest creds: the ab-run images use user `claude` (passwordless key + sudo).
- The balloon control socket speaks `target <bytes>\n` and `stats\n` (one-line
  reply); `target` is the balloon size goal, so max = MEM_MAX − MEM_MIN.
