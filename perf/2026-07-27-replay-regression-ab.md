# venus replay "regression" — the 2026-07-27 A/B campaign

Follow-up to `perf/2026-07-26-remeasure.md` §"gl-replay-venus −18%". Eight arms measured on
one F44-enhanced clone (kernel 7.1.4-limina16k), display pinned 1280x800@1.0, EFI enforcing,
worker at aaa362e. Raw rows in `perf/ledger.csv` (notes `venus-replay-regression A/B arm *`).

## TL;DR — the "regression" dissolves into three findings

1. **No component of the current stack reproduces the −18%.** Every suspect A/B'd flat at
   matched envelope (table below). The whole host stack from 2026-07-13 to today measures
   *identically* on the same guest; virglrenderer is exonerated wholesale by a dylib swap.
2. **gl-replay is substantially a guest-CPU/latency metric, not a GPU one: +14% going
   4→6 vCPU** (47.9 → 54.8, every stack era alike). The 2026-06-25 vehicle hardcoded
   4 vCPU/4 GiB; the current boot scripts default 6/8192 — an envelope trap that briefly
   manufactured a fake "era signal" today (arms F/G) until the PROVENANCE line exposed it.
3. **The 2026-06-25 reference (57.6 / 1601) is not reproducible against any reachable
   stack, and its guest no longer exists.** The June ledger's own CPU control moved +57%
   (llvmpipe 537 → 841) and glmark2-venus +53% (1306 → ~2000) between June and July on the
   same envelope, while the replays moved −18%/−10%. No uniform stack tax produces both
   signs at once: the guest userland changed under the metric (mesa respins rebuilt
   llvmpipe/zink/venus alike; apitrace itself is dnf-installed). Treat the June replay
   rows as **not comparable** — the honest baseline for the replay metrics starts at
   2026-07-26/27.

## The arms

| arm | config | gl-replay (fps) | glmark2-512 |
|---|---|---|---|
| A (baseline) | current stack, 4 vCPU | **47.87** | 1994 |
| B | virgl @ 83d9a3c (pre-relax 0041/0043/0044), dylib swap | 47.66 / 47.93 | 2274 / 2353 |
| C | current virgl, `VKR_JOURNAL=0` | 47.89 / 47.95 | 2175 / 2196 |
| D | guest mesa downgraded 26.1.4-3 → 26.1.3-4 | 47.53 / 47.71 | 2058 / 2072 |
| quartet | June's full `VN_PERF=no_semaphore,fence,event,query_feedback` | 48.33 / 48.56 | — |
| F | full July-13 host (repo 288f105 + virgl 4900245 + era libkrun) — **6 vCPU** | 54.48 / 54.73 | 1845 / 2123 |
| G | July-13 worker + **current** virgl dylib — 6 vCPU | 55.00 | 1924 |
| H | current stack — **6 vCPU** (envelope control) | 54.79 | 1886 |

Loaded-dylib identity for the swap arms was verified via `lsof`/`vmmap` against the file
size (current 4 409 608 vs pre-relax 4 409 480 vs jul13 4 372 504) — not assumed.

Previously exonerated (2026-07-26): KosmicKrisp (June-tip build), `no_fence_feedback`
alone, SELinux. Guest kernel exonerated across rows: F43 at 6.12.0-limina16k and F44 at
7.1.4-limina16k both read ~47–49 at 4 vCPU.

## The June-host reconstruction is impossible (attempted, twice)

A full June-25 host build (repo worktree @ c3f5138, virgl @ 7d9e406, June libkrun series)
**cannot run any current guest**: on the F44 image venus fails init outright
(`vkEnumeratePhysicalDevices → ERROR_INITIALIZATION_FAILED`; the 7.1 kernel's virtio-gpu
needs the 2026-07-03 blob fixes), and on the F43 image (6.12 kernel) today's mutter 49.6
crash-loops. The June pairing is extinct on both sides.

## What IS real and actionable (glmark2, interactive-shaped workloads)

- **The relax ladder (virgl 0041+0043+0044) costs glmark2 ~13–16%** (arm B: 1994 → 2313
  avg with relax reverted). This is the same residual 0043 measured on vkmark (~2360 vs
  ~2760 relax-off) and never fully recovered. The wakeup win is real but the price on
  submit-latency-bound work is real too — worth a finer-early-rungs iteration (e.g.
  10/10/20/40 one-per-rung before the plateau), re-measuring BOTH fps and wakeups/s.
- **The vkr journal (M9.3, recording ON by default) costs glmark2 ~9%** (arm C: 1994 →
  2185 avg with `VKR_JOURNAL=0`). A product decision is available: default recording off
  until a VM has snapshots enabled, or cheapen the create-path recording.
- The two taxes were only measured separately; the both-off combination is untested.
- Neither tax moves gl-replay at 4 vCPU — its bottleneck sits elsewhere in the guest
  round-trip path (it also barely notices the VN_PERF quartet, +0.7).

## Follow-ups

1. Re-baseline: accept 2026-07-26/27 numbers as the replay trend origin; stop chasing
   57.6. (The one unfalsifiable remnant: guest mesa limina patches 0014/0016 are in every
   surviving image, so a patchless rebuild in the build guest could still test them — only
   worth it if some *other* evidence points at the guest venus driver again.)
2. vk-replay was not re-measured today (gfxrecon-replay absent on this clone) — same
   campaign applies if it's ever worth it; expectation: same story.
3. The relax-rung iteration and the journal default (above) — real wins available on
   interactive workloads.
4. Tooling: perf-ledger PROVENANCE now records vcpus/ram — always read it before
   comparing rows; the 4-vs-6 vCPU trap ate half of this campaign.
