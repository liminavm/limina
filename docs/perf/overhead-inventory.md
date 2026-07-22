# Runtime overhead inventory — wakeups, exits, and CPU tax under load

**Date:** 2026-07-21. **Status: measured + decomposed (read-only pass); trimming is future
milestone work.** Goal per the user: *figure out exactly what the sources of overhead are and
plan to trim them down to the absolute minimum.*

Specimen: 6-vCPU enhanced F44 guest (kernel 7.1.4-limina16k, coexist venus/KK), windowed
1512x982, firefox running the blobs WebGL demo at ~60 fps steady. Dev M1 Max. Probes:
`spikes/wakeup-probe/procwake.c` (host, `proc_pid_rusage`), guest `/proc/interrupts` deltas,
guest `perf -e ipi:ipi_raise`, host `sample`. NOTE: the ORIGINAL headline numbers below were
taken while the guest mesa still had the vn_ring free-list bug (patches/mesa/0017). Re-baselined
2026-07-22 on the shipped image (mesa 26.1.4-3.limina, virgl 0043) — guest CPU is now flat (no
creep, lever 6) and the current 60 fps common-case wakeup rate is ~8.1k/s (lever 0b′); the
malloc/scan churn is gone but the *wakeup* budget below is structural.

## The headline numbers (worker process, under load)

| metric | value |
|---|---|
| worker CPU | ~75-85% of one core |
| interrupt wakeups (`ri_interrupt_wkups`) | **~20,300/s** (~340/frame) |
| pkg-idle wakeups (`ri_pkg_idle_wkups`) | ~8-47/s |
| host context switches | ~7k/s |
| guest context switches | ~11.5k/s |
| supervisor process | ~7% CPU, ~71 wakeups/s (negligible) |
| GPU device utilization | ~17% |

Activity Monitor's "Idle Wake Ups" column = the interrupt-wakeup **rate** (windowed), NOT
cumulative and NOT top's IDLEW — see `spikes/wakeup-probe/RESULTS.md`.

## Decomposition (guest-visible half, /proc/interrupts over 10 s)

| source | rate | what it is |
|---|---|---|
| IPI1 function-call IPIs | **~4,970/s** | **93% = `ttwu_queue_wakelist` remote task wakeups** (perf ipi:ipi_raise: 22.6k of 24.2k raises in 5 s); 7% = `kick_ilb` nohz balance kicks. The guest scheduler spreads the frame pipeline (Renderer → firefox:zfq0 → vn_wsi → Compositor → WaylandProxy → gnome-shell) across 6 vCPUs and pays an IPI per cross-CPU wake. Each IPI = a sender vmexit (GIC SGI trap) + a host wakeup of the target vCPU thread. |
| arch_timer | ~3,590/s | tick + hrtimers. Our 16k kernel is **CONFIG_HZ=1000** — CORRECTION (2026-07-21, verified at src.fedoraproject.org kernel f44 kernel-aarch64-fedora.config): Fedora aarch64 stock is ALSO 1000 (the earlier "stock is 100" claim was wrong — that's RHEL/server); our config inherits it faithfully per build-kernel-rpm.sh's Fedora-fidelity goal. Lowering HZ is therefore a DELIBERATE enhanced-tier divergence (HZ=250 upstream-defconfig-like ≈ 4x cut, HZ=100 RHEL-like ≈ 10x), not a restore — and it cannot help stock guests. NO_HZ_FULL=y is already in both configs; `nohz_full=` is not on the cmdline. |
| virtio5 = virtio_gpu | ~912/s (~15/frame) | host→guest fence/ctrl completion injections. **539/s after libkrun 0091** (EVENT_IDX + one-signal-per-fence-callback, lever 0). |
| virtio9/11/10/1 (blk/net/vsock/i2c) | ~40/s total | noise. |

Guest-invisible other half (host-internal, from `sample` + architecture): guest→host doorbell
kicks (GPU submit + venus notify; each = kevent wake of the worker main loop → gpu-worker hop →
vkr-ring cnd_signal), KK submit → MTLSharedEvent → vkr-queue fence retirement, present
handshake (gpu shown-ack pipe + gpu latch), and the main-thread kevent loop itself (~10pp of a
core, always caught parked in `kevent` because each dispatch is µs-short).

## Where the worker's ~80% CPU goes

~60pp in-guest execution (demo JS ~10-15pp; guest venus/zink encode ~10pp — inflated by the
0017 free-list bug; gnome-shell ~5pp; guest kernel/IRQ/exit overhead the rest — the guest's own
accounting shows only ~48pp because tick sampling undercounts µs bursts and the host bills
exit/entry to the vCPU threads) + ~10pp worker kevent event loop + ~7pp venus decode
(vkr-ring) + ~3pp gpu worker/present. vkr-queue threads *look* busy in ps but are blocked in
`IOSurfaceSharedEvent waitUntilSignaledValue` (kernel wait, not spin).

**P/E-core placement is NOT a factor in accounting gaps**: the guest's timebase (CNTVCT) ticks
at constant rate regardless of core, so E-core placement would *inflate* guest-reported CPU,
never deflate it. (Whether vCPU threads get E-core residency IS an open question for frame
pacing/battery — measure with `powermetrics --samplers tasks`, needs sudo.)

## Trim levers, ranked by expected yield (the future-milestone backlog)

0. **virtio-gpu EVENT_IDX + fence-IRQ coalescing — SHIPPED 2026-07-21 (libkrun 0091, task
   #30): −2.9k/s.** The GPU queues now offer `VIRTIO_RING_F_EVENT_IDX` (both feature sets;
   a driver that doesn't ack it keeps stock semantics — the EFI GOP phase and 6.12 test
   kernel verified unaffected), the worker drains control/cursor inside a
   disable/enable-notification bracket, and the fence handler signals once per completion
   callback (was once per retired descriptor — over-notified stock guests too), all gated
   on `needs_notification`. Measured, same specimen/method as the A/B below: host wakeups
   18.6k → **15.7k/s**, guest virtio5 fence-IRQ injections 912 → **539/s (−41%)**, worker
   CPU 68.6 → 65.5%, render throughput unchanged (user-eyeballed smooth, firefox CPU
   signature flat).

0b. **vkr ring relax ladder — SHIPPED 2026-07-21 (virglrenderer 0041, task #38): the
   dominant term, ~−10k/s.** Direct instrumentation (LIMINA_WAKE_TRACE, libkrun 0092 +
   virgl 0042) attributed the wake budget and found the "chain" guess wrong: the main
   event loop wakes **6/s**, the gpu worker ~720/s (672 doorbells + 60 present), fences
   ~240/s → the missing term was **~15.5k/s of nanosleep expiries in `vkr_ring_relax`**,
   which slept once per iteration with the duration doubling only per power-of-two block
   (~55 sleeps per 1 ms ring idle window, relax_iter reset on every decoded command).
   Fix: one sleep per rung, quadrupling per call (10, 40, 160, 640 µs — the 10 µs first
   rung keeps early pickup latency unchanged; worst-case in-window pickup stays at the
   640 µs cap), ~4 sleeps per full window. Same-protocol legs: total wakeups
   **14.3k → 6.4k/s (doubling rungs) → 4.2k/s (quad rungs, shipped)**, ring poll sleeps
   15.5k → 1.9k/s, fences/present rates identical, smoothness eyeball-confirmed every
   leg; worker CPU showed no resolvable delta (79-108% across boots in every config —
   the blobs workload is not stationary enough for finer CPU comparisons; wakeup rates
   are the reliable cross-boot oracle). The poll window is *useful* (catches ~440
   resumes/s vs ~200 doorbell parks/s), so early-park variants were rejected.
   **Day total: 18.6k → 4.2k/s (−77%).**

0b′. **vkr relax throughput cost + adaptive-plateau fix — SHIPPED 2026-07-21 (virgl 0043,
   task #38/#39).** The 640 µs quad-rung cap above bought its wakeup savings with pickup
   latency: on **vkmark** (venus, uncapped ~2400 fps, a submit-latency-bound ping-pong)
   the Score fell from ~2760 (relax off) to **1193** at cap=640 — roughly halved. A/B
   attributed the whole drop to the relax ladder (EVENT_IDX/0091 is throughput-neutral).
   A flat low cap recovers throughput (cap=40 → 2342) but sleeps at the cap rate when the
   ring is *idle* (~25k/s at 40 µs), destroying the idle win. Fix (0043): a two-phase
   backoff keyed on idle duration — a responsive 40 µs plateau (held for `warm_rungs`,
   ~640 µs) that an actively-fed ring never leaves (`relax_iter` resets per command), then
   a 640 µs deep-idle fallback / park. Result: **vkmark ~2360 (≈2× cap=640) with idle
   wakeups unchanged (~270/s)**, load wakeups ~18.4k/s under vkmark's worst-case continuous
   submit. See spikes/wakeup-probe/RESULTS.md for the full table; ~18.4k is the uncapped
   ceiling.
   **60 fps common-case re-baseline — MEASURED 2026-07-22 (closed the TODO; virgl 0043 +
   mesa 0017, 6 vCPU): idle ~130/s, vkcube ~3.2k/s, clean-fullscreen blobs ~8.1k/s**
   (overview-compositing adds ~2.7k — measure fullscreen only). Wake-trace: the plateau is
   NOT free at a *visible* 60 fps — vkr_ring poll-sleeps ~5.9k/s (vs ~1.9k for old flat-640),
   the whole 4.2k→8.1k rise, because blobs feeds the ring in small per-frame submits so
   relax_iter keeps resetting onto the 40 µs plateau. Still −56% vs 18.6k; cheap timer
   wakeups. DECISION (user 2026-07-22): keep 0043 shipped, make plateau depth a `(visible,power)`
   knob under M13 (roadmap M13 task 4) rather than a static retune. The user is **not fully happy
   with the plateau tuning** and believes we can do better — deliberately **parked for another pass
   POST-M13**, revisited together with the vkr doorbell-handshake idea (lever 4) since they're the
   same lever. Not a static retune now.

1. **Doorbell-path shortening — PREMISE KILLED 2026-07-21 (source-verified during #38).**
   The GPU doorbell is already direct (vCPU MMIO write at 0x50 → queue eventfd → gpu
   worker's own epoll, mmio.rs:702-708 / gpu/worker.rs inner_run) and fence IRQ injection
   is already `hv_gic_set_spi` from the fence thread (hvfgicv3.rs:130 — in-kernel GIC, no
   main-loop bounce). There is nothing to shorten; the "kevent main loop hop" existed only
   in the sample-inferred chain description. Remaining ~4.2k/s ≈ ring poll ~1.9k +
   in-kernel vCPU wakes (IPI/timer — #35 territory) + KK/Metal internal threads
   (uninstrumented) + libkrun ~1k.

2. **vCPU right-sizing — DROPPED as a dynamic feature 2026-07-22 (task #35 gate failed).** The
   2026-07-21 A/B against the OLD 18.6k baseline showed 6→2 vCPU cutting ~2,950/s. RE-GATED
   2026-07-22 on the current stack (virgl 0043 + mesa 0017, clean-fullscreen blobs, same method):
   6-vCPU ~8.1k → 2-vCPU ~7.3k = **only −800/s host, below the 1k/s drop bar**. Guest half still
   moved (IPI1 −75%, arch_timer −35%) but the host budget is now dominated by the ~5.9k/s vkr_ring
   poll-sleeps, INVARIANT to vCPU count — so the earlier ~2,950 shrank to ~800 once the round-2
   fixes collapsed everything else. Throughput was already fine; idle vCPUs near-free under NO_HZ.
   Building the full agent-driven policy + hysteresis (oscillation risk) for <800/s is not worth it.
   The mechanism half (#40, libkrun 0094: clean CPU_OFF/AFFINITY_INFO/re-deliverable CPU_ON) SHIPPED
   on its own as a two-tier robustness fix (stock guest offlining a vCPU no longer wedges the VMM).
   Topology stays MOOT (already shared-LLC-correct: L2 shared 0-5, one MC sched domain; ttwu IPIs are
   the idle-target branch, not the cache branch). RE-GATE only if the post-M13 vkr doorbell-handshake
   cuts the poll-sleep floor.
2. **Kernel tick — DROPPED 2026-07-21 (user decision: no Fedora-config divergence unless
   absolutely required).** HZ audit found Fedora aarch64 ships 1000 too, so lowering HZ
   would be a deliberate divergence, enhanced-tier-only, and NO_HZ already makes idle free —
   the HZ cost is confined to the busy/oscillating regime, which lever 1 (shared-LLC
   topology → wake_affine packing → fewer WFI-oscillating vCPUs) attacks without diverging.
   Revisit ONLY if topology + EVENT_IDX leave us far from the kernel_task-sum bar.
3. **virtio EVENT_IDX on the remaining devices (vsock, input, snd) — small.** The GPU got it
   in lever 0; vsock is the only other chatty one (control plane + timesync). Same recipe.
4. **venus notify throttle — VERIFIED 2026-07-22, no throttle win.** Mesa already rate-limits
   `vkNotifyRingMESA` to 1/ms (`VN_RING_IDLE_TIMEOUT_NS`); the host ring parks on cnd_wait
   (virgl 0003). Wake-trace under clean-fullscreen blobs confirms the ACTIVE venus ring is
   **poll-driven, not doorbell-driven** — the ~485/s gpu_worker "doorbells" are structural
   virtio-gpu submit-queue kicks (~8/frame), NOT venus-ring notifies. Nothing to *throttle* here.
   **BUT — a down-the-line lever (post-M13): make the active ring doorbell-driven via a
   wakeup-suppression HANDSHAKE, not a naive block-on-notify.** The ~5.9k vkr_ring poll-sleeps
   (lever 0b′) are the dominant remaining term. A plain "host blocks, guest kicks every submit"
   is a NET LOSS — it trades cheap host nanosleep wakeups for guest vmexits (each notify is an
   MMIO trap host-ward), and mesa deliberately does NOT notify while actively submitting because
   it assumes the host polls; the poll window catches ~440 resumes/s that would each need a
   doorbell. The winning form is the **EVENT_IDX pattern (shipped for the virtio-gpu queue in
   0091) applied to the vkr ring** (shared-memory, not a virtio queue, so it doesn't get it for
   free): host publishes "parked at ring position X" before blocking on a futex/cnd; guest kicks
   ONLY when it advances past a parked host, silent (no vmexit) whenever the host keeps up by
   polling. Eliminates the poll-sleeps without a notify-per-submit. Composes with the M13
   `(visible,power)` knob (occluded/battery → park-and-doorbell sooner; focused/AC → poll for
   latency) — SAME lever as the plateau retune below, so revisit both together post-M13.
5. ~~Shorten the host wake chain per submission~~ — KILLED, see lever 1: both hops it
   proposed to remove were already absent (doorbell eventfd goes straight to the gpu
   worker's epoll; IRQ inject is in-kernel hv_gic_set_spi from the calling thread).
6. **Re-baseline after mesa 0017** (free-list fix) — **VERIFIED FLAT 2026-07-22.** ~14 min
   clean-fullscreen blobs soak on the shipped image (mesa 26.1.4-3.limina): firefox lifetime
   average 114s CPU / 835s elapsed = 13.65%, instantaneous %CPU 13.7% — they MATCH, so
   per-frame cost is flat over the whole run (a quadratic free-list creep would push
   instantaneous well above the lifetime average). Matches the documented GREEN post-0017
   signature (flat ~14%) vs the RED bug (19.6→29.3% creep). No creep; 0017 holding.

## Reproduction

```
spikes/wakeup-probe/procwake <worker-pid> 5 5              # host wakeup budget
ssh guest 'cat /proc/interrupts' twice, 10 s apart          # guest-visible decomposition
ssh guest sudo perf record -a -g -e ipi:ipi_raise sleep 5   # IPI attribution
sample <worker-pid> 10 + spikes/venus-draw-probe/threadacct.py  # thread attribution
#   (threadacct wait-leaf set must include iokit_user_client_trap, plain read, mach_msg)
```
