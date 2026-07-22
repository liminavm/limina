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

## EVENT_IDX + fence-IRQ coalescing A/B (2026-07-21, libkrun 0091 — task #30)

Same specimen and method (host-sleep-eyeball.raw, 6 vCPU, blobs fullscreen, overview
dismissed, user-eyeballed rendering, settle ~45s, procwake 5s×4 + guest /proc/interrupts
10s delta). Change under test: GPU queues offer `VIRTIO_RING_F_EVENT_IDX` (negotiation
verified in-guest: `/sys/bus/virtio/devices/virtio5/features` bit 29 = 1), worker drains
control/cursor inside a disable/enable-notification bracket, fence handler signals once
per completion callback (was once per retired descriptor) gated on `needs_notification`.

| metric | before (0090) | after (0091) | Δ |
|---|---|---|---|
| host wakeups/s (ri_interrupt_wkups) | ~18,600 | ~15,700 | **−2,900 (−16%)** |
| guest virtio5 IRQ/s (fence/ctrl injections) | ~912 | ~539 | **−41%** |
| guest IPI1/s | ~4,560 | ~4,360 | flat |
| guest arch_timer/s | ~3,330 | ~3,080 | flat |
| worker CPU | 68.6% | 65.5% | −3pp |
| firefox render CPU | 14.7% | ~13-14% | flat (render unchanged) |

Reading: EVENT_IDX + coalescing trimmed ~2.5-3k/s off the wake chain (suppressed doorbell
kicks while the worker drains + fewer, batched fence injections). IPI/timer terms untouched,
as expected — they're guest-scheduler terms (#35's territory). The remaining chain share is
~8k/s: doorbells still hop vcpu → kevent main loop → gpu worker, and fence completions still
bounce through the main loop for IRQ injection — that's the doorbell-path-shortening lever
(inventory lever 5), not EVENT_IDX's.

The EFI GOP driver doesn't ack EVENT_IDX → stock semantics preserved (this boot went
GOP → GRUB → 16k kernel and rendered fine); a stock guest that does ack it gets the same
suppression (the feature is transport-level, kernel-side since forever).

## Direct attribution + the ring-relax fix (2026-07-21, task #38 — virgl 0041)

Lever 5's premises ("doorbell hops through the kevent main loop", "fence→IRQ bounces
through the main loop") were checked against source before coding and BOTH were false:
the queue doorbell is vCPU MMIO 0x50 → queue eventfd → the gpu worker's own epoll
(libkrun mmio.rs:702, gpu/worker.rs), and IRQ injection is `hv_gic_set_spi` straight from
the fence thread (hvfgicv3.rs:130, in-kernel GIC). The chain description in earlier notes
was sample-inferred, never observed. Lesson re-banked: enumerate and verify premises.

So we instrumented instead of guessing (LIMINA_WAKE_TRACE=1; libkrun 0092 event-manager /
gpu-worker / fence counters + virgl 0042 ring counters, all env-gated, ~5s cadence to the
worker log). Attribution under blobs (post-0091 EVENT_IDX build):

| source | rate |
|---|---|
| **vkr_ring poll sleeps (relax ladder)** | **~15,500/s** |
| gpu worker epoll (672 doorbells + 60 present) | ~720/s |
| fence callbacks → IRQ signals | ~240/s → ~235/s |
| main event loop (all other devices) | **6-11/s** |

The ladder slept once per ITERATION, duration doubling only per power-of-two block (16
sleeps @10µs, 32 @20µs, ...) ≈ ~55 timed wakeups per 1ms idle window, restarted on every
decoded command. Fix (virgl 0041): one sleep per rung, doubling per call, 640µs cap —
worst-case in-window pickup latency unchanged, ~7 sleeps per full window.

Same-protocol A/B (fresh boot each leg, measure after blobs confirmed rendering):

| metric | old ladder | new ladder |
|---|---|---|
| host wakeups/s | ~14,300 | **~6,400** |
| vkr_ring poll sleeps/s | ~15,500* | ~3,300 |
| poll resumes / parks per s | 680 / 315 | ~620 / ~240 |
| fences / present per s | 240 / 60 | 240 / 60 |
| render (user eyeball) | smooth | smooth |

*pre-fix sleeps measured on a leg whose total read ~18k; totals move ±2k between boots.

The poll window is USEFUL — ~600/s resumes are caught mid-window vs ~245/s doorbell parks
— so don't replace it with immediate parking (that would trade guest-notify vmexits and
add latency). Remaining knob: coarser first rung (20-40µs base) ≈ another ~1-2k/s if ever
needed, at the cost of early-pickup latency.

CPU-comparison caveat (methodology): worker %CPU read 79-108% and firefox parent 12-22%
ACROSS BOOTS in BOTH configs at matched workloads — the blobs specimen is not stationary
enough to resolve CPU deltas of that size across sessions; an apparent "40pp regression"
and a "firefox creep" during this work were both boot-to-boot / top-slicing artifacts
(a direct /proc utime 10s delta on the "creeping" firefox showed a healthy 13.6% with the
0017-fixed thread mix). Wakeup rates are the reliable cross-boot oracle here.

Post-fix budget ≈ 6k/s: ring poll ~3.3k + libkrun sources ~1k + in-kernel vCPU wakes
(IPI/timer, #35's territory) + KK/Metal internals (uninstrumented, ~1k).

## Final rung shape: quadruple per rung (2026-07-21, folded into virgl 0041)

Third leg, same protocol: ladder 10→40→160→640µs (one sleep per rung, quadrupling per
call). Quadrupling keeps the 10µs first rung — early pickup latency unchanged — versus
raising the base, and a full window walk is ~4 sleeps.

| metric | per-iteration (upstream) | doubling rungs | quad rungs (shipped) |
|---|---|---|---|
| host wakeups/s | ~14,300-18,000 | ~6,400 | **~4,180** |
| ring poll sleeps/s | ~15,500 | ~3,300 | **~1,900** |
| poll resumes / parks | 680 / 315 | 620 / 240 | 440 / 200 |
| render (user eyeball) | smooth | smooth | smooth |

Day summary (blobs specimen, 6 vCPU): 18.6k → 4.2k/s (−77%) via libkrun 0091
(EVENT_IDX + fence coalescing) + virgl 0041 (ring relax ladder, quad rungs).
Remaining ≈ 4.2k: ring poll 1.9k + in-kernel vCPU IPI/timer wakes (#35) + KK/Metal
internals + libkrun ~1k.

## Dynamic vCPU offlining de-risk (2026-07-21, task #35 — vcpu-offline-probe.sh)

Before investing in agent-driven vCPU offlining, the probe asked the two load-bearing
questions empirically (6-vCPU 7.1.0 injected-16k guest, headless+NAT, idle, offline cpus
2..5 via `echo 0 > /sys/devices/system/cpu/cpuN/online`):

| state | worker %CPU | outcome |
|---|---|---|
| 6 vCPUs online (baseline) | ~2.9% | idle, all parked |
| after offlining cpus 2..5 | **~546%** | **guest WEDGED (ssh Connection reset)** |

**Offlining a running vCPU SPINS, it does not park — and it wedges the guest.** Root cause:
libkrun's PSCI (`hvf/src/lib.rs handle_psci_request`) models CPU_ON (0xc400_0003) but NOT
CPU_OFF (0x8400_0002) or AFFINITY_INFO (0xc400_0004) — both return `NOT_SUPPORTED` (both
warns seen in the log). So arm64 Linux commits to the offline at `cpu_psci_cpu_disable`
(function-id is registered), then `cpu_die`'s CPU_OFF HVC fails → the dying vCPU threads
can't stop and busy-spin (~+5 cores), while the reaper (`cpu_psci_cpu_kill`) polls
AFFINITY_INFO forever. Re-online is moot (guest already dead) AND structurally broken: the
secondary boot channel (`macos/vstate.rs` `boot_receiver.recv()`) is **one-shot**, consumed
at boot — a runtime CPU_ON sends an entry addr to a channel nobody reads.

**Conclusion: dynamic offlining is NOT a guest-only feature on current libkrun — it is
destructive.** It requires a libkrun CPU-hotplug mechanism: model CPU_OFF so the vCPU thread
parks cleanly (reuse the M9 `handle_pause` park machinery — a new `VcpuEvent::Online(entry)`
+ per-vCPU park), model AFFINITY_INFO for a parked vCPU, make CPU_ON re-deliverable at
runtime (durable per-vCPU control channel, PC reset on the owning thread since HVF regs are
thread-bound), and handle IRQ/vtimer re-affinity + snapshot-while-offlined + CPU_ON↔CPU_OFF
races. **Cost/benefit (advisor-reviewed, opus):** the original ~3k/s was measured vs the OLD
18.6k baseline; #30/#38 attacked the GPU/IO chain that *drives* the guest ttwu IPIs, so the
in-kernel IPI/timer slice of today's 4.2k is likely <1k/s — a poor trade for a full
hotplug state machine on a GPU-bound workload that never needed the vCPUs. **Recommendation:
DEFER the dynamic feature; remeasure the IPI/timer slice against today's 4.2k baseline before
any build (if <1k/s → DROP).** Independently worthwhile regardless: a minimal "model CPU_OFF
so it parks instead of wedging the VMM" patch is a real robustness fix — a stock guest that
offlines a CPU today hangs the whole VM, violating the two-tier stock-guest guarantee — and
is upstreamable + de-risks a future #35 without committing to the policy.

## Throughput cost of the relax ladder + the adaptive-plateau fix (2026-07-21, virgl 0043)

The wakeup work traded away throughput and we hadn't priced it. A/B on the ring-relax
(virgl 0041) + EVENT_IDX (libkrun 0091) changes, scored on **vkmark** (venus, seated,
`vkmark -s 1280x720`, host-sleep-eyeball.raw, 6 vCPU) — chosen over the blobs eyeball
specimen because it is a *stationary, scored* oracle (leg B ran 1193/1193, 0% variance)
and it is **submit-latency-bound** (uncapped ~2400 fps, a tight submit->wait->submit
ping-pong), so it is the most sensitive probe for the ring-poll pickup latency.

| leg | EVENT_IDX | ring-relax | vkmark Score | wakeups/s (load) | wakeups/s (idle) |
|---|---|---|---|---|---|
| A  | off | off (per-iter)      | 2399 / 2801 | ~30–55k | (busy) |
| C1 | **on** | off (per-iter)   | 2745 / 2782 | ~29k    | — |
| B  | on | on, cap=640 (shipped) | 1193 / 1193 | ~11k    | ~270 |
| —  | on | on, cap=160           | 1562 / 1561 | ~12.5k  | — |
| —  | on | on, cap=40 (flat)     | 2332 / 2353 | ~18k    | **~25k (idle regression)** |
| **✔** | on | **adaptive plateau** | **2333 / 2390** | **~18.4k** | **~270** |

FINDINGS:
1. **The 2× throughput drop is ENTIRELY the ring-relax ladder, not EVENT_IDX.** Leg C1
   (EVENT_IDX on, relax off) scores the same ~2760 as leg A (both off) → EVENT_IDX is
   throughput-neutral (keep it). The relax backoff added up to 640 µs of pickup latency
   per inter-submit gap; on a ping-pong workload that halves fps (1193 vs ~2760).
2. **A flat low cap is NOT the answer** — it fixes load throughput (cap=40 → 2342) but a
   permanently-idle ring then sleeps at the cap rate (~25k/s at 40 µs), destroying the
   idle-wakeup win the whole path exists for.
3. **Adaptive plateau (shipped, virgl 0043) is the optimal trade** — keyed on how long the
   ring has been idle (`relax_iter` resets on every processed command): an actively-fed ring
   stays on a responsive 40 µs plateau (~2360 Score, full throughput), a quiet desktop crosses
   it once and falls back to 640 µs / parks (~270/s idle). ~2× the throughput of shipped-640
   with idle wakeups unchanged.

CAVEAT (unmeasured): the ~18.4k/s load figure is vkmark's worst case (continuous uncapped
submit at ~2400 fps). A vsync-capped 60 fps workload is mostly idle gaps → its rate sits far
closer to the ~270/s idle floor. The blobs 60 fps A/B numbers earlier in this file were NOT
re-measured against 0043; the 60 fps wakeup delta from the plateau is expected small but is a
TODO. Oracle: `spikes/wakeup-probe/ab-vkmark.sh <label>` (fresh clone → venus boot → idle
sample → vkmark ×N with per-run wakeup sampling → teardown) built each leg here.

**UPDATE (2026-07-22): that "expected small" guess was WRONG — the 60 fps re-baseline below
measures the plateau's common-case cost at ~4k/s, not small.** See the next section.

## 60 fps common-case re-baseline vs 0043 (2026-07-22) — the plateau is NOT free at 60 fps

Closing the TODO above. Fresh COW clone of `Fedora-Workstation-44.enhanced.raw` (kernel
7.1.4-limina16k, **mesa 26.1.4-3.limina with the 0017 free-list fix**), 6 vCPU, windowed
venus+net, current shipped worker (virgl 0043). procwake 5s×5 + guest `/proc/interrupts` 10s
deltas + `LIMINA_WAKE_TRACE` attribution. **Method note (load-bearing):** the windowed guest
MUST be in **clean fullscreen** (unredirected scanout) before measuring — a firefox launched
into an undismissed GNOME **Activities overview** makes mutter composite every frame and adds
~2.7k/s of pure artifact. Get there with `firefox --kiosk <url>` (robust) — NOT host osascript
function keys (F11 = macOS *Show Desktop*, hits the host; see the key-injection roadmap backlog).

| workload (0043 + mesa 0017, 6 vCPU) | host wakeups/s |
|---|---|
| idle (seated desktop, no GPU) | **~130/s** (better than the ~270 claimed; deep-idle backoff) |
| vkcube (light 60 fps venus, one cube) | ~3,200/s |
| **blobs, clean fullscreen 60 fps** | **~8,100/s** |
| blobs, overview-composited 60 fps (artifact) | ~10,800/s |

Guest half under clean-fullscreen blobs (unchanged from the historical 60 fps specimen, i.e.
the workload is the same): IPI1 ~4,390/s, arch_timer ~2,910/s, virtio5 (GPU fence IRQ) ~491/s.

Wake-trace decomposition of the clean 8.1k, vs the prior-session 0041 (flat-640, quad rungs):

| source | 0043 adaptive (now) | 0041 flat-640 (prior) |
|---|---|---|
| **vkr_ring poll_sleeps** | **~5,900/s** | ~1,900/s |
| gpu_worker doorbells (control) | ~485/s | ~672/s |
| present | ~56/s | ~60/s |
| fence callbacks → IRQ | ~235/s | ~240/s |
| event_manager | 6/s | 6/s |

FINDING: the entire 4.2k → 8.1k rise is **vkr_ring poll-sleeps tripling** (1.9k → 5.9k). blobs
feeds the ring in many small submits per frame (WebGL + compositor), so `relax_iter` keeps
resetting and the ring sits almost continuously on the responsive 40 µs warm plateau → ~5,900
cheap nanosleep expiries/s. The flat-640 cap slept 640 µs regardless → far fewer. This is 0043
doing exactly what it was designed to (stay responsive for actively-fed rings) — it's the
latency-vs-wakeups trade, now quantified on the 60 fps case. Both are big wins over the 18.6k
original (−56% and −77%); these are cheap timer wakeups, not real work.

DECISION (2026-07-22, user): **leave 0043 as shipped** — the responsive plateau is worth it for
game/interactive latency, 8.1k is a fine common-case number, and instead of a static retune the
plateau depth becomes a **`(visible, power)`-keyed knob under M13** (focused+AC → responsive;
60 fps-capped / battery / occluded → deep backoff). Folded into `docs/roadmap.md` M13 task 4.

Lever 4 (venus notify / doorbell) also answered here: the ACTIVE venus ring is **poll-driven,
not doorbell-driven** (venus notify throttle working); the ~485/s gpu_worker "doorbells" are
structural virtio-gpu submit-queue kicks (~8/frame), not venus-ring notifies — no easy win.

## #35 dynamic vCPU offlining — GO/NO-GO GATE (2026-07-22): DROPPED

The roadmap (§Dynamic vCPU hotplug) set a gate: the old ~2,950/s 6→2 win was measured against the
18.6k baseline; remeasure on the current stack before building the policy, drop it if <1k/s. Done —
same 0043+mesa-0017 stack, clean-fullscreen blobs, same procwake+interrupts method; only `--cpus`
differs (the 6-vCPU leg is the 8.1k re-baseline above):

| metric | 6 vCPU | 2 vCPU | Δ |
|---|---|---|---|
| **host wakeups/s** | ~8,100 | ~7,300 | **−800 (below the 1k drop bar)** |
| guest IPI1/s (ttwu wakelist) | ~4,390 | ~1,115 | −75% |
| guest arch_timer/s | ~2,910 | ~1,900 | −35% |
| render (user eyeball) | smooth | smooth | flat |

VERDICT: **DROP the dynamic policy (#35).** The mechanism plainly works (guest IPI/timer fell hard),
but the HOST budget is now dominated by the ~5,900/s vkr_ring poll-sleeps (0043 plateau), which are
INVARIANT to vCPU count — so the vCPU host-wakeup slice is ~800/s at the 6→2 EXTREME (a dynamic
policy offlining fewer, on average, would save less). Not worth the signal→policy→hysteresis
machinery + oscillation risk. The mechanism half (#40, libkrun 0094) already shipped as a standalone
two-tier robustness fix. RE-GATE only if the post-M13 vkr doorbell-handshake cuts the poll-sleep
floor (which would make the vCPU slice a larger relative fraction).
