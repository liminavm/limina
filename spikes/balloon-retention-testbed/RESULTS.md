# Results

## Run log

| run | date | mix (linux/mesa/synoik) | pool @ plateau | scrub: ic_bal | scrub: pool | pf @ plateau | pf post-scrub |
|---|---|---|---|---|---|---|---|
| first-repro | 08-11 | 225/60/198 s | 10.47G | 11.20 → 4.26G | 10.47 → 3.55G | ~14.2G | 6.40G |
| memset-ctl | 08-11 | 225/61/198 s | 10.49G | 11.21 → 3.95G | 10.49 → 3.23G | 14.53G | 6.90G |
| memset-on | 08-11 | 226/61/198 s | 11.04G | 11.74 → 1.35G | 11.04 → 0.65G | **18.03G** | 8.99G (still falling at cutoff) |

All runs: fresh APFS clone of ab-run-b.raw, `MIX=full`, 2G..12G, 8 cpus, ~12.5G
host ballast. Reproduction is tight: two independent control runs landed within
0.02G of each other at the plateau.

## Testbed verdict (first-repro + memset-ctl)

- The retention pool reproduces on demand: ~10.5G billed against a ~0.7G guest
  live set after compile mix + host pressure.
- The scrub cycle (full inflate → 30 s hold → deflate, ~90 s, driven over the
  balloon control socket, no guest agent) recovers ~two thirds of the pool.
- Cost signal: the scrub crushes guest page cache (4.85G → 0.85G) — policy
  should be host-pressure-triggered, never periodic.

## memset-before-REUSABLE A/B (`release-memset.patch`, gate `LIMINA_BALLOON_RELEASE_MEMSET=1`)

Zero the released range between `hv_vm_unmap` and `MADV_FREE_REUSABLE`
(`released_ram.rs::release`).

- **Compile-mix timing: free.** 226/61/198 s vs 225/61/198 s — within the ±1 s
  band of four runs. Inflate wall time also unchanged (full 10G in ~15 s both
  arms) despite memsetting all 10G.
- **Scrub residue: memset settles what plain release leaves behind.** Post-scrub
  compressed residue 1.35G vs 3.95G (pool 0.65G vs 3.23G). This *falsifies part
  of the settled "unmap settles compressed slots instantly" conclusion at scale:*
  in the control, inflate dropped ic_bal 11.21 → 4.63 in one sample tick (instant
  settling worked for ~70% of the released content) but ~2.6G of already-compressed
  slots did not settle — and memset recovers exactly that share, presumably by
  faulting the page back (freeing the slot) before the zero page is discarded.
  Why the instant settle is partial is an OPEN question. It also corrects the
  earlier residue guess: most of the control residue was settleable guest-RAM
  compressor slots, not worker-own anon.
- **The cost is resident footprint, not time.** At the plateau the memset arm
  ran pf 18.03G vs 14.53G (+3.5G, int_bal 6.27 vs 3.30). Composition UNEXPLAINED:
  the obvious story (zeros awaiting the pageout scan) is contradicted by the
  ledger — reus_bal was only 0.69G at plateau against 20G cumulative credit, so
  the scan was keeping up and the standing +3G of int_bal is something else.
  Post-scrub the memset arm was still draining at run end (pf 10.06 → 8.99G over
  ~40 s), so the post-scrub pf comparison is truncated, not settled — a KEEP_VM=1
  run with a longer window would give the settled endpoint.

**Net:** memset moves the billing from compressor slots (which persist
indefinitely — the retention pool) to resident reusable zeros (which the OS
reclaims lazily, and cheaply, under pressure). As an always-on release-path
change it trades a permanent pool for a churn-rate-dependent resident overhead.
The attractive refinement: zero only on *deflate/scrub-path* releases (balloon
inflate queue) and leave FRQ releases plain — settles the pool when a scrub
runs without the steady-state overhead. The balloon device can tell the two
queues apart.

## No-scrub soak A/B (`SCRUB=0 SOAK_MIN=10`, 08-11): memset without a scrub buys nothing

Protocol: plateau, then (ballast held): 10 m idle → guest touch workload
(2G cache cycle + 3G anon touch, ~6-11 s) → 10 m idle.

| arm | plateau pool | idle slope | post-touch pool | pf idle | pf post-touch |
|---|---|---|---|---|---|
| soak-ctl | 9.02G | −0.05G/10m | 3.61G (settled ≤2 m, then flat) | 14.36G | **14.45G (flat)** |
| soak-on | 10.79G | −0.12G/10m | 3.69G (settled ≤2 m, then flat) | 18.90G | 15.96G |

- **Pool endpoint identical** (3.61 vs 3.69G) — without a scrub, memset does not
  change where guest activity lands the ic-based pool metric.
- **Footprint: memset strictly worse at every phase** of the no-scrub protocol
  (18.9 vs 14.4 idle; 16.0 vs 14.4 post-activity).
- **The metric caveat this run exposed:** in the control, the touch-driven
  ic_bal drop (9.68 → 4.32G) left pf FLAT at ~14.4G — int_bal rose 4.7 → 10.1G
  in the same tick. Guest re-touches *decompress* dead content into residency;
  the dead-dirty total billed to the worker is unchanged, it just changes form.
  `pool = ic_bal − guest_live` only counts the COMPRESSED share, so
  activity-driven "drain" of the metric is largely accounting migration, not
  memory returned to the host. Read pf alongside pool for any recovery claim.
  (This also reinterprets the dogfood 17.4 → 6.6G drain observation: evidence
  the content is guest-reachable, NOT evidence of recovery.)
- The only intervention so far that actually shrinks pf is the scrub
  (14.5 → 6.90G control, → 8.99G-falling memset). After the workload exited,
  control int_bal and reus_bal sat flat for 10 minutes — nothing further was
  reported/settled post-exit; the drained content reformed resident. How much
  FPR released *during* the touch window is underdetermined from these columns
  (the cache cycle writes fresh pages, so int-arithmetic can't isolate it, and
  reus_bal only shows releases whose pages were resident when marked — which is
  why the memset arm shows +3.1G there and the control +0.4G without implying
  asymmetric FPR behavior). The sampler now records the balloon's
  released/remapped/heals counters so the next soak answers this directly.
