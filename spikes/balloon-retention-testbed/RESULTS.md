# Results

## Run log

| run | date | mix (linux/mesa/synoik) | pool @ plateau | scrub: ic_bal | scrub: pool | pf @ plateau | pf post-scrub |
|---|---|---|---|---|---|---|---|
| first-repro | 08-11 | 225/60/198 s | 10.47G | 11.20 → 4.26G | 10.47 → 3.55G | ~14.2G | 6.40G |
| memset-ctl | 08-11 | 225/61/198 s | 10.49G | 11.21 → 3.95G | 10.49 → 3.23G | 14.53G | 6.90G |
| memset-on | 08-11 | 226/61/198 s | 11.04G | 11.74 → 1.35G | 11.04 → 0.65G | **18.03G** | 8.99G (still falling at cutoff) |
| oracle2 (policy scrub, enhanced guest) | 08-11 | touch mix | ~0G (16k FPR kernel retains nothing) | n/a (policy-driven) | n/a | 1.45G | 1.49G (flat — no pool to recover, by design) |
| clampgrade (clamp+decay+bounded scrub, enhanced guest, NEW 0.4.0 agent) | 08-11 | cache mix (5G kept file + 3G anon) | 4.20G | n/a (bounded scrub correctly NOT DUE) | n/a | 6.23G | 5.79G (warn-tier policy inflation, no scrub) |

All runs: fresh APFS clone of ab-run-b.raw, `MIX=full`, 2G..12G, 8 cpus, ~12.5G
host ballast. Reproduction is tight: two independent control runs landed within
0.02G of each other at the plateau.

**The ab-* images have NO limina-agent** (no unit, no RPM — leaner than the
enhanced tier). Manual socket scrubs (`SCRUB=1`) work on them, but the
`POLICY_SCRUB=1` oracle needs guest pressure reports to tick the policy, so it
must run on an `enhanced.test` clone (4 KiB balloon-visible pool will be smaller
there: the 16k host-page-aware kernel reports frees FPR can actually take).

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

## Live policy-scrub oracle (`POLICY_SCRUB=1`, 08-11, out-oracle2-1786477496)

First end-to-end execution of the shipped pressure-triggered scrub (limina 8fdac4f),
on an `enhanced.test` clone (the ab images carry no limina-agent — no reports, no
policy ticks). Boot with the `@file` host-pressure seam pinned `normal` +
`--reclaim light` (policy quiescent, `converged` at target 0 through pool build and
plateau), then at 30.7 min uptime the file flipped to `warn`:

- **Trigger: ≤5 s** from the flip to `"scrub":"start"` (gen 1, resume 0) — the armed
  30-min construction cooldown held until then, through a plateau of injected-Normal
  ticks and the real sysctls reading Warn underneath.
- **Inflate: ~7 s to 96%** of the 10 GiB room (guest mostly free pages), the ≥90%
  fast-path advance to Holding, `reached_pct:96` in the trace.
- **Hold: 15.1 s** (SCRUB_HOLD honored), actual topped out at 10.74G = 100% of room.
- **Deflate → done: 6 s**, actual back to 0 = the resume target, converged via stats
  (not timeout). Total cycle 28 s. No abort, no watchdog, guest healthy, clean
  poweroff after.
- pf 1.45 → 1.49G, flat as designed: the 16k host-page-aware kernel's FPR retains
  ~nothing (plateau pool ≈ 0), so this run validates the **policy cycle**; the
  recovery magnitude stands on the manual-scrub rows above (4k ab guest, 66%).

Caveats this run does NOT cover: the abort path (acute pressure mid-scrub), the
watchdog path (reports dying mid-scrub), and a scrub against a populated retention
pool with a busy guest — those remain unit-tested only (policy) or manual-scrub
proven (recovery). Take 1–3 traps for the next runner: ab images lack the agent;
`out-<label>-<ts>/balloon.sock` must stay under ~100 chars (SUN_LEN — run.sh now
fails fast); detach runs >30 min with `nohup … & disown` (the background-task
reaper killed takes 1 and 3... take 1 was the suite).

## Clamp grade (08-11, out-clampgrade-1786492387): the MemFree clamp does NOT preserve cache — FALSIFIED premise

The tuning-arc grade run: `MIX=cache` (new — 5G file written, read twice, KEPT, + 3G anon
churn: the cache-warm profile) on an enhanced.test clone with the 0.4.0 MemFree-reporting
agent, `--reclaim moderate`, injected-Normal through build + plateau, warn flip at 30.5 min.

- **The headline: kswapd defeats the clamp on a cache-warm guest.** The clamp's premise —
  "low MemFree means only cache is left" — is false: as the balloon takes free pages, kswapd
  refills the free list *from cache* faster than the balloon drains it. Guest MemFree sat at
  its watermark equilibrium (~450–550M on this 12G guest) for the entire dig, never crossing
  the 256M Moderate margin, so `cap_step` stayed ≥ a full 256M step and inflation sprinted at
  the full 128 MiB/s — at **host-Normal** — converting the warm cache into balloon fill with
  the free list as a pass-through buffer: cached 5503M → 1960M in ~60 s, converging at
  avail ≈ 1536M = exactly the pre-clamp 12.5%-of-max allowance. Zero `free-exhausted` rows in
  1963 trace ticks. The clamp shaved only the last couple of steps (235M/207M) cosmetically.
  **What the clamp still is: a death-spiral protector** — it binds when free is genuinely
  pinned below the margin (anon-heavy, no swap, nothing reclaimable), the unit-covered regime.
  It is not a cache preserver, which is what the arc claimed. No static margin fixes this:
  the free-list floor is watermark_boost- and guest-size-dependent (450–550M here vs Light's
  512M margin — a coin flip), so margins above it can't be chosen reliably.
- **Bounded scrub: correctly NOT DUE — the pre-registered legitimate outcome.** At the warn
  flip, free headroom above margin was ~275M < SCRUB_MIN_INFLATE, so the due-gate refused
  (the driver's "POLICY-SCRUB FAILED" line is the expected verdict in this profile, not a
  failure). No cache-crushing scrub fired — that half of the design held.
- **Gap decay: NOT exercised** (no gap ever stuck ≥10 s; fills kept up at these rates). Not
  validated, not falsified.
- **Pool: MIX=cache reproduces 4.20G on the 16k enhanced kernel** — updates oracle2's "16k
  FPR retains ~nothing": that was touch-mix-specific (free-page content); cache-heavy content
  under host ballast bills 5.6G ic_bal against ~1.4G live even on the host-page-aware kernel.
- **Warn-tier behavior**: policy inflated 9076→9637M (full steps — free stayed above margin,
  so no trickle rows either), digging cache further to the 6.25% allowance; pf 6.37→5.75G.
- **Candidate fix directions** (design decision, not yet taken): (b) a **free-elasticity
  detector** — if MemFree does not drop by ~the step size after a send, reclaim backfilled it
  from cache → hold at Normal (pure host-side policy, uses the per-tick free_kib we already
  have, no proto/agent change); or (c) report `Cached` in MemPressure and gate on
  cache-shrinkage directly (direct measurement, but a 0.5.0 agent + redeploy right after
  0.4.0). A static-margin bump (a) is ruled out above. Note the intent split: at
  Warn/Critical digging cache is *intended* (memory is owed) — the elasticity hold is a
  Normal-tier rule; Warn might get a trickle governor instead.

**Protocol change (08-11 tuning arc, limina 59e40ee):** the scrub is now mode-keyed
(Light = critical trigger / 60 min cooldown / BOUNDED depth; Moderate = warn / 30 min /
bounded; Aggressive = warn / 15 min / full), so this oracle run's warn-flip-at-30-min
protocol no longer matches `--reclaim light`. run.sh now derives the flip level and
wait from `SCRUB_MODE` (default light → critical at 3630 s). Bounded scrubs need an
agent that reports MemFree; against an older agent they degrade to eager-full, so the
cycle shape above still holds. NEXT run here: grade the clamp + bounded scrub against
a POPULATED pool (MIX=full on an enhanced clone with the NEW agent, moderate mode) —
that's the profile where the 08-11 clamp actually binds.
