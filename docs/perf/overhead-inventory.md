# Runtime overhead inventory — wakeups, exits, and CPU tax under load

**Date:** 2026-07-21. **Status: measured + decomposed (read-only pass); trimming is future
milestone work.** Goal per the user: *figure out exactly what the sources of overhead are and
plan to trim them down to the absolute minimum.*

Specimen: 6-vCPU enhanced F44 guest (kernel 7.1.4-limina16k, coexist venus/KK), windowed
1512x982, firefox running the blobs WebGL demo at ~60 fps steady. Dev M1 Max. Probes:
`spikes/wakeup-probe/procwake.c` (host, `proc_pid_rusage`), guest `/proc/interrupts` deltas,
guest `perf -e ipi:ipi_raise`, host `sample`. NOTE: numbers were taken while the guest mesa
still had the vn_ring free-list bug (patches/mesa/0017); re-baseline after it ships — the
malloc/scan churn inflates guest CPU but the *wakeup* budget below is structural.

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

1. **Per-frame GPU/IO wake CHAIN (~8k/s still at stake — the dominant remaining term).**
   The vCPU A/B showed the chain invariant to vCPU count; EVENT_IDX trimmed it ~2.5k. What
   remains: doorbell eventfds that still hop through the kevent main loop, present
   handshakes, and the cross-thread condvar hops (vcpu → kevent main loop → gpu worker →
   vkr-ring → KK → vkr-queue → fence → IRQ inject). Attack with doorbell-path shortening
   (lever 5). Helps STOCK guests too.

2. **vCPU right-sizing (~3k/s at stake — MEASURED, smaller than expected).** A/B 2026-07-21:
   6→2 vCPU cut total host wakeups −16% (~2,950/s: IPI −67%, timer −56%) with throughput
   UNAFFECTED on blobs (GPU + single-threaded-JS bound). IMPORTANT CORRECTION: the guest
   topology is ALREADY shared-LLC-correct (L2 shared 0-5, one MC sched domain) — the ttwu IPIs
   are the idle-target branch, so a topology patch would do nothing; the win comes purely from
   fewer idle CPUs to wake. Ship as DYNAMIC vCPU offlining via limina-agent (a static low
   default would hurt genuinely parallel guest workloads — compiles, etc.); pairs with
   ballooning. NOT a topology change.
2. **Kernel tick — DROPPED 2026-07-21 (user decision: no Fedora-config divergence unless
   absolutely required).** HZ audit found Fedora aarch64 ships 1000 too, so lowering HZ
   would be a deliberate divergence, enhanced-tier-only, and NO_HZ already makes idle free —
   the HZ cost is confined to the busy/oscillating regime, which lever 1 (shared-LLC
   topology → wake_affine packing → fewer WFI-oscillating vCPUs) attacks without diverging.
   Revisit ONLY if topology + EVENT_IDX leave us far from the kernel_task-sum bar.
3. **virtio EVENT_IDX on the remaining devices (vsock, input, snd) — small.** The GPU got it
   in lever 0; vsock is the only other chatty one (control plane + timesync). Same recipe.
4. **venus notify throttle.** Mesa already rate-limits `vkNotifyRingMESA` to 1/ms
   (`VN_RING_IDLE_TIMEOUT_NS`); the host ring parks on cnd_wait (virgl 0003). Check the
   remaining kick rate under load; the ring being ACTIVE should need zero doorbells — verify.
5. **Shorten the host wake chain per submission.** Today: vcpu → eventfd → kevent main loop →
   gpu-worker channel → vkr-ring condvar (→ KK → vkr-queue → fence eventfd → main loop → IRQ
   inject). Candidates: route the GPU queue doorbell eventfd directly to the gpu worker
   (skip the main-loop hop); fence completion → IRQ without the main-loop bounce.
6. **Re-baseline after mesa 0017** (free-list fix) — guest venus encode pp should drop and
   stay flat over hours; keep a long-soak procwake trace as the regression oracle.

## Reproduction

```
spikes/wakeup-probe/procwake <worker-pid> 5 5              # host wakeup budget
ssh guest 'cat /proc/interrupts' twice, 10 s apart          # guest-visible decomposition
ssh guest sudo perf record -a -g -e ipi:ipi_raise sleep 5   # IPI attribution
sample <worker-pid> 10 + spikes/venus-draw-probe/threadacct.py  # thread attribution
#   (threadacct wait-leaf set must include iokit_user_client_trap, plain read, mach_msg)
```
