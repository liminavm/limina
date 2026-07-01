# Venus command-ring idle wakeups — root cause & fix

**Status:** SHIPPED (host virglrenderer patch `patches/virglrenderer/0003`), validated on the
enhanced tier 2026-07-01. Supersedes the `#30` 2 ms-poll workaround (`vkr_ring.c` commit `1f9b328`).

## Problem

On the enhanced (venus) tier, the `limina-vmm` worker showed **~60k "Idle Wake Ups"** in macOS
Activity Monitor — an order of magnitude over a comparable idle Parallels VM — and held **~12% of
a CPU core continuously at idle**. Battery cost with a VM open and nothing happening on screen.

(Activity Monitor's *Idle Wake Ups* is cumulative-since-launch; the meaningful figures are the
steady-idle **rate** ~75–150/s and the ~12% CPU.)

## Investigation (measured, not assumed)

Booted an idle enhanced-tier F44 desktop (16k kernel, 6 vCPUs, zink→venus→KosmicKrisp). At true
idle every worker thread parks cleanly (6 vCPUs, all virtio device/vsock/block/net/input workers in
`epoll_wait(-1)`/`cvwait`/`poll`) **except** the venus command-ring threads (`vkr-ring-*`). `ps -M`
showed 3 threads at ~3% CPU each with a ~5:1 **system:user** time ratio — the signature of
`sched_yield` (`swtch_pri`) spinning. `sample` put them in `vkr_ring_thread` (on-CPU poll) and
`vkr_ring_thread → cthread_yield → swtch_pri`.

**Causal A/B:** killing the venus client (`systemctl isolate multi-user.target`, destroying the
venus contexts) collapsed `limina-vmm` idle wakeups **~75/s → ~0/s** and removed the ring threads.
So the venus ring threads are the source.

## Root cause: a store-buffer (SB) memory-ordering race — NOT #28 coherency

The ring idle/notify handshake (`vkr_ring.c` host ↔ mesa `vn_ring.c` guest) is the classic
store-buffer litmus:

| | Host ring thread (`vkr_ring_thread`) | Guest (`vn_ring_submit`) |
|---|---|---|
| store | set `IDLE` status bit — **seq_cst** (`vkr_ring_set_status_bits`) | advance tail — release (`vn_ring_store_tail`) |
| load | read tail — **`memory_order_acquire`** ⚠️ (`vkr_ring_load_tail`) | read status — **seq_cst** (`vn_ring_load_status`) |
| action | if tail empty → sleep in `cnd_wait` | if `IDLE` observed → emit `vkNotifyRingMESA` (doorbell, real vmexit) |

A lost wakeup (host sleeps **and** guest skips the notify) requires the host to miss the guest's
tail **and** the guest to miss the host's IDLE. `seq_cst` on **both** host ops forbids it — whichever
seq_cst op the SC total order puts second observes the first (the guest's tail store is `release`
but sequenced-before its seq_cst status load, so it inherits the ordering; the host's IDLE store is
seq_cst). But the host's tail load was **`acquire`**, which on weakly-ordered Apple Silicon compiles
to `LDAPR` and **can reorder ahead of the prior seq_cst IDLE store** — reopening the race.

That race — not blob coherency — is the real cause of the `#30` "missed notify" hang:

- On **x86 TSO** (where upstream venus runs) a seq_cst store fences the store buffer, so
  store→acquire-load can't reorder; upstream's blocking `cnd_wait` never hangs. The bug is
  **Apple-Silicon-specific**, which is exactly when the `#30` workaround was added.
- `#28` is a **GPU-write** SLC-beyond-PoC staleness (`docs/roadmap.md:381`); the IDLE bit is a
  **host-CPU** write to a Shared-MTLBuffer-backed blob, and host-CPU↔guest-CPU coherency for normal
  cacheable memory is an ARM hardware guarantee. The `#30` commit plausibly-but-wrongly attributed
  the missed notify to `#28` (a nearby scary premise) without testing the ordering angle — the exact
  "verify premises empirically" failure mode called out in `CLAUDE.md`.

The `#30` fix (2 ms `cnd_timedwait` + re-poll) masked the race at the cost of a permanent idle
busy-poll: after each 2 ms timeout it reset `last_submit`, forcing a full `idle_timeout` (1 ms;
`VN_RING_IDLE_TIMEOUT_NS`) window of `vkr_ring_relax` (`thrd_yield` spin → `nanosleep` backoff) —
~9–12% of a core, forever, per live venus context.

## The fix (`patches/virglrenderer/0003`)

Two lines of intent in `src/venus/vkr_ring.c`:

1. **`vkr_ring_load_tail_seqcst`** — load the tail with `memory_order_seq_cst` **in the idle check
   only** (the hot producer/consumer data path keeps the cheaper `acquire`). Now SC-ordered with the
   seq_cst IDLE store → the SB race is closed on ARM. No guest change needed.
2. **Revert the idle wait to blocking `cnd_wait`** (drop the 2 ms `cnd_timedwait` + `last_submit`
   reset). A quiescent ring parks with **0 host wakeups**, woken only by the guest's `vkNotifyRingMESA`
   doorbell — which `vkr_ring_notify` delivers under the ring mutex (condvar handshake already correct).

## Validation (enhanced tier, M1 Max, idle gnome-shell venus desktop)

| Metric | before (2 ms poll) | after (this fix) |
|---|---|---|
| `limina-vmm` idle wakeups | ~75–150/s | **~2–4/s** |
| idle CPU | ~12% of a core | **~2%** |
| ring threads at idle | spinning (`swtch_pri`) | **blocked in `cnd_wait`** (`__psynch_cvwait`) |

No regression of `#30`: gnome-shell composited through sustained idle **and** 8 idle↔burst cycles
(overview animations + `glxgears` bursts — the bursty ring-idle trigger) with **zero** `vn_relax` /
aborts / ring failures in guest dmesg or the host log.

## Why not the originally-scoped "coherent doorbell" (#3)

The bigger doorbell/coherency redesign is unnecessary: the missed notify was an ordering race, not a
coherency gap. The wake **channel** was already a real doorbell (`vkNotifyRingMESA` → vmexit →
`vkr_ring_notify`); only the guest's *decision* to send it was racing, and the decision is now
race-free. Kept the change minimal and upstreamable (mechanism in the dependency).

## Notes / residual

- **Defense-in-depth option (not taken):** a long `cnd_timedwait` (e.g. 500 ms–1 s) instead of pure
  `cnd_wait` would self-heal any *unforeseen* missed wakeup at ~1–2 wakeups/s aggregate (still
  negligible). Pure blocking validated clean, so we ship it (matches upstream); revisit only if a
  hang ever recurs.
- **Testing:** an SB race is timing-dependent and not deterministically reproducible in CI on ARM
  (the old code didn't hang 100% either), so there is no L2 regression test — validation is the
  empirical mechanism+outcome+stress above. The failure mode would resurface as a `#30` `vn_relax`
  abort under gnome-shell, which the stress exercises.
- Only affects the **enhanced/venus tier** (`vkr_ring_thread` doesn't exist on the stock virgl/GL
  path); the two-tier floor is unchanged.

See memory `limina-idle-wakeups-venus-ring`; probe `spikes/venus-draw-probe/guest-idle-probe.sh`.
