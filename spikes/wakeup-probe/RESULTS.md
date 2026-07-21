# wakeup-probe — what Activity Monitor's "Idle Wake Ups" actually shows

**Date:** 2026-07-21. **Question:** the user observed limina-vmm at "20k+ Idle Wake Ups"
in Activity Monitor while a windowed venus VM ran the blobs WebGL demo — an order of
magnitude over kernel_task, and the number fluctuated. A prior memory claimed AM's column
is cumulative-since-launch; the fluctuation disproves that (cumulative counters can't go
down). Which of macOS's per-process wakeup counters is AM showing, and is the rate real?

## Method

macOS exposes two wakeup counters per process via `proc_pid_rusage(RUSAGE_INFO_V4)`
(readable without root for same-uid targets):

- `ri_pkg_idle_wkups` — wakeups that brought the package out of idle. This is what
  `top`'s IDLEW column shows (cumulative on the first sample, deltas after).
- `ri_interrupt_wkups` — all interrupt wakeups.

`procwake.c` samples both and prints cumulatives + per-interval rates. Build:
`cc -O2 -o procwake procwake.c`; run: `./procwake <pid> [interval_s] [count]`.

## Result (limina-vmm, 6-vCPU enhanced venus guest, blobs demo fullscreen)

| counter | rate |
|---|---|
| pkg_idle_wkups | ~8/s |
| interrupt_wkups | **~21,500/s** |

The "20k+" Activity Monitor shows matches the **interrupt-wakeup rate** (AM windows it per
refresh interval — hence the fluctuation). Conclusions:

1. **AM "Idle Wake Ups" is a windowed rate of `ri_interrupt_wkups`, NOT cumulative and NOT
   `top`'s IDLEW.** Comparing AM's number against top's pkg-idle counter is apples/oranges.
2. The 2026-07-01 venus-ring fix remains valid for what it measured (pkg-idle at true idle:
   ~150/s → ~2-4/s); it says nothing about under-load interrupt churn.
3. **~21.5k wakeups/s under a 60 fps venus workload (~360/frame) is a real optimization
   target**: doorbell eventfds, the worker main-thread kevent loop, cross-thread condvar
   hops (vcpu → event loop → gpu worker → vkr-ring → KK → vkr-queue), fence/present
   handshakes. CSW ~7k/s on the same process. Candidate levers: virtio notification
   suppression (EVENT_IDX) on the hot queues, batching doorbells, shortening the per-submit
   wake chain.

## vCPU right-sizing A/B (2026-07-21, blobs WebGL, mesa 26.1.4-3, host-sleep-eyeball.raw)

Same image/workload, only `--cpus` differs. Settled ~45s after firefox confirmed rendering
(user-eyeballed animating), procwake 5s×4 + guest /proc/interrupts 10s deltas.

| metric | 6 vCPU | 2 vCPU | Δ |
|---|---|---|---|
| host wakeups/s (ri_interrupt_wkups) | ~18,600 | ~15,700 | −2,950 (−16%) |
| guest IPI1/s (ttwu_queue_wakelist) | ~4,560 | ~1,525 | −67% |
| guest arch_timer/s | ~3,330 | ~1,460 | −56% |
| worker CPU | 68.6% | 70.2% | flat (noise) |
| firefox render CPU | 14.7% | 14.3% | flat |

FINDINGS:
VERIFIED against kernel source (v7.1.4 kernel/sched/core.c ttwu_queue_cond, fetched from
git.kernel.org 2026-07-21) — NOT inferred from the wakeup numbers:
  if (!cpus_share_cache(this_cpu, cpu)) return true;   // (1) no-shared-cache → wakelist IPI
  if (cpu == this_cpu)                  return false;
  if (!cpu_rq(cpu)->nr_running)         return true;   // (2) target CPU idle → wakelist IPI
  return false;
Guest-side evidence that path (1) is NOT taken (cpus_share_cache == true): L2 shared_cpu_list
= 0-5 AND one MC sched domain spanning 0x3f (the MC/LLC domain is what sets sd_llc_id, which
cpus_share_cache compares). So our ttwu_queue_wakelist IPIs are path (2) — waking an IDLE
sibling (nr_running==0) — reachable ONLY because we already pass the cache check. A topology
patch is therefore genuinely moot (confirmed, not assumed): it can't remove path-(2) IPIs,
which is why the A/B cut them only by removing idle siblings (fewer vCPUs), not by topology.

Original guest-view observations:
1. cacheinfo:
   cpu0 L2 (index2) shared_cpu_list = 0-5; /proc/schedstat: one MC domain spanning 0x3f;
   cpus_share_cache() true for every pair. So the ttwu IPIs are the IDLE-TARGET branch
   (wake an available_idle_cpu even within the LLC → must IPI a WFI'd CPU), NOT the
   no-shared-cache wakelist branch. A topology patch would change nothing.
2. Fewer vCPUs = fewer idle targets → more same-CPU/local wakes → IPI −67%, and fewer busy
   CPUs ticking → timer −56%. But these are only ~5-6k of the ~18.6k, so total drops just
   −16%. Throughput UNAFFECTED (worker + firefox CPU flat) because blobs is GPU + single-
   threaded-JS bound, not vCPU-parallel.
3. THE DOMINANT term (~13k/s, ~70%) is the per-frame GPU/IO wake CHAIN (doorbells, fence
   injects, present handshakes, cross-thread condvar hops) — INVARIANT to vCPU count.
   That is the bigger lever: virtio-gpu EVENT_IDX + fence coalescing + doorbell-path
   shortening (task #30), not vCPU/topology.

RE-RANK: #30 (wake-chain, ~13k at stake) now outranks vCPU right-sizing (~3k). vCPU
right-sizing stays valuable but as DYNAMIC offlining via limina-agent (static low default
would hurt genuinely parallel guest workloads); the mechanism's yield is proven here.

## Leg C: 1 vCPU (2026-07-21) — FIRST ATTEMPT CONTAMINATED (overview stayed up), re-run below

| metric | 6 vCPU | 2 vCPU | 1 vCPU |
|---|---|---|---|
| host wakeups/s | ~18,600 | ~15,700 | ~19,700 |
| guest IPI/s | ~4,560 | ~1,525 | 0 (exactly — no other CPU to wake) |
| guest timer/s | ~3,330 | ~1,460 | ~960 |
| worker CPU | 68.6% | 70.2% | 60.2% |
| firefox CPU | 14.7% | 14.3% | 9.7% |

1 vCPU drove cross-CPU IPIs to ZERO (confirms the ttwu term is purely cross-CPU) and the
timer to its floor (one CPU ticking) — yet TOTAL host wakeups ROSE above both other legs.
Why: ri_interrupt_wkups counts host wakeups of the worker's THREADS, dominated by the
single vCPU thread being woken out of WFI for every device event (GPU fence IRQ inject,
doorbell response, timer). With one vCPU that is only ~60% busy, it parks between bursts and
must be re-woken for EVERY event — nothing lands on an already-running sibling. Spreading
across more vCPUs lets some events hit a running CPU (no host wakeup) but adds IPIs; 2 is the
balance. CONFOUND: firefox CPU fell 14.7→9.7%, i.e. the demo likely ran at LOWER fps at
1 vCPU (single CPU can't keep the pipeline fed), so per-frame the 1-vCPU wakeup cost is even
worse than the flat rate suggests.

⚠️ INVALIDATED (user caught it): the GNOME overview never dismissed on this leg — the
busctl OverviewActive=false didn't take (likely lost to single-CPU contention during settle).
So firefox was OCCLUDED behind the overview and throttled (→ the 9.7% CPU, well below the
~14% of legs A/B), while gnome-shell animated the overview. This measured overview
compositing at 1 vCPU, NOT blobs. The apparent U-shape is probably this artifact. Re-run
required; the >5%-CPU gate was too weak (it passed on the throttled state) — tighten to
require ~full render CPU (≈14%) AND OverviewActive=false before measuring.

### Leg C RE-RUN (clean: overview confirmed false, user-eyeballed SMOOTH render)

| metric | 6 vCPU | 2 vCPU | 1 vCPU (clean) |
|---|---|---|---|
| host wakeups/s | ~18,600 | ~15,700 | ~17,000 |
| guest IPI/s | ~4,560 | ~1,525 | 0 |
| guest timer/s | ~3,330 | ~1,460 | ~890 |
| worker CPU | 68.6% | 70.2% | 47.4% |

Corrected picture: the U-shape is REAL but SHALLOW — 2 (min, ~15.7k) < 1 (~17k) < 6 (~18.6k),
a <±20% wobble around a ~16-17k floor. The contaminated first 1-vCPU attempt read ~19.7k;
the overview alone inflated it ~2.7k/s (gnome-shell animating blur/thumbnails while firefox
was occluded/throttled) — the user caught the overview still up.

Two methodology lessons banked:
- **`top` %CPU is per-CPU-normalized** → NOT comparable across guests with different vCPU
  counts (firefox "9% at 1 vCPU" vs "14% at 6 vCPU" is 9%-of-one-core vs 14%-of-one-of-six,
  not less work). A CPU-threshold render gate is therefore invalid across legs; the reliable
  "is it rendering" signal is a human eyeball of smoothness (or a real fps counter), not %CPU.
- At 1 vCPU the demo still renders SMOOTH 60fps (user-confirmed) at 47% worker CPU — the
  heavy lifting is host-side venus→KK→Metal, the guest CPU merely submits, so 3D throughput
  is NOT vCPU-bound for this workload.

FINAL CONCLUSION: vCPU count is a shallow ±3k/s knob (min at 2, IPI-driven) around a ~16-17k
floor set by the per-frame GPU/IO wake chain. That floor is the target — task #30 (EVENT_IDX
+ fence coalescing + doorbell-path shortening). vCPU right-sizing stays a minor, dynamic-only
lever (#35). No shipping decision rides on the static vCPU count; these were bounding extremes.
