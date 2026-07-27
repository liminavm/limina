# present-miss — the compositor's "presentation misses" are guest-emulated-vblank artifacts

**2026-07-27.** Response to `dogfood-guest:Projects/gnome-shell-rs/docs/fork/present-misses.md`
(the gnome-shell-rs session's writeup: ~12% of frames land one refresh late despite being
queued early, 30% at idle; §6 asks whether `DRM_EVENT_FLIP_COMPLETE`'s timestamp is real
scanout time or host bookkeeping).

## TL;DR — the answer to their §6

**Neither real scanout time nor host-side bookkeeping: the timestamp is the guest's own
emulated vblank hrtimer.** Since kernel ~7.1 (the deployed `7.1.4-limina16k` has it),
virtio-gpu no longer completes flips at commit time (`drm_atomic_helper_fake_vblank`): it
has `drm_vblank_helper` timer-emulated vblank (`virtgpu_display.c` uses
`DRM_CRTC_VBLANK_TIMER_FUNCS`; flip events are ARMED at commit-tail and DELIVERED at the
next expiry of a guest-side hrtimer running at the mode's `framedur_ns`). The host's real
present/latch time never enters the picture:

- **The fence-present chain (libkrun 0017–0021, `LIMINA_FENCE_PRESENT`) is DORMANT in the
  deployed app.** Neither the supervisor nor the .app sets the env, and dogfood-mac has no
  `/tmp/limina-fence-present` marker (verified live 2026-07-27): `try_park_present` bails,
  presents are fire-and-forget at flush time, and the guest's flush fence completes
  synchronously (`create_fence_inner` Global-ring path). The mechanism designed to gate
  the guest's flip on the true CA latch (`shown <id>` acks — the supervisor DOES set
  `LIMINA_SHOWN_ACK_FD` and sends acks from the `CATransaction` completion block) exists
  end-to-end but the arming knob never made it from the round-27–29 spike scripts into the
  product. The roadmap's "enhanced tier = `FENCE_PRESENT=1`" describes intent, not what
  ships.
- So today: guest commit-tail = in-fence wait (their `IN_FENCE_FD` render fence) + flush
  round-trip (sub-ms; host completes it at command processing) → vblank event armed →
  **delivered/timestamped at the next guest hrtimer tick**. "Missed vblank" measures *the
  commit chain crossing a guest-local 16.668 ms grid boundary*, not anything on glass.

## What we measured (probes in this dir, local F44 enhanced clone, M1 Max)

`vblgrid.c` — rides `drmWaitVBlank` (no DRM master needed) to sample the emulated grid.
`flipmiss.c` — DRM-master page-flip loop mimicking their frame clock (target = last event
timestamp rounded up on the refresh grid; queue at `target − headroom`; miss =
`round((actual−target)/refresh)`), dumb-buffer/software-2D path. Guest booted via the
standard EFI+venus path, gdm stopped for the master probes.

1. **The grid is exact and phase-stable while the timer is enabled.** 300 back-to-back
   waits: period 16 668.34 µs (59.9938 Hz — the EDID generator's integer clock truncation,
   not 60.000), phase residual **±1 µs** over 5 s. Flip events land *exactly* on grid
   points (hit deltas of 0–1 µs in flipmiss).
2. **Every vblank re-enable RE-ANCHORS the phase arbitrarily.** The timer starts
   `HRTIMER_MODE_REL` at "now + framedur" (`drm_crtc_vblank_start_timer`) and is cancelled
   when DRM disables vblank — `drm/parameters/vblankoffdelay` = 5000 ms after the last
   reference drops. Waits 6 s apart: phase jumps of −2.4 ms … +7.1 ms per sample. Waits
   1 s apart (within the off-delay): mod-residual 0–1 µs, same grid. Corollary: any
   isolated flip after ≥5 s of display idle gets its completion exactly
   `commit + ~1 framedur` on a fresh grid unrelated to the previous timestamps the
   compositor extrapolates from.
   (Also latent: the timer re-arms with `hrtimer_forward_now`, so a late hrtimer fire —
   vCPU wake latency — would shift the phase permanently. Not observed under load here;
   the ±1 µs says HVF timer delivery is tight on a quiet host.)
3. **Miss rate = P(commit chain latency > headroom), quantized to whole cycles.**
   flipmiss busy at 60 fps, 2560×1440 dumb-buffer flips (host does a full readback +
   swizzle per flip on this path, so the chain is ~5–10 ms — heavier than niri's zero-copy
   flush, lighter than niri's in-fence+render):

   | headroom | missed (of 300) |
   |---|---|
   | 2 ms | 52.7% (probe's own sleep jitter — 230/300 queued late) |
   | 5 ms | 14.3% |
   | 10 ms | 2.3% |
   | 14 ms | 2.3% |

   Misses are isolated, exactly-one-cycle, coin-toss-shaped — the same signature as
   their §3/§4 — and reproduce **without a compositor, without venus, without their
   fork**: it is the structure of arm-at-commit / deliver-at-tick. Hits land at
   |actual−target| p50 **0 µs** (events are grid-locked); the residual 2.3% at
   h≥10 ms were multi-cycle stalls (guest background load), not the one-cycle class.
4. **Idle regimes — the re-anchor becomes deterministic.**
   - 1 s gaps (within the 5 s off-delay), h=5 ms, 60 flips: **3.3%** missed (both were
     the probe's own late queues; the grid stayed put).
   - 6 s gaps (crossing the off-delay), h=5 ms, 30 flips: **100% missed, every single
     one exactly one cycle**. Mechanically forced: the re-enabled timer's first expiry
     is `enable + framedur`, so an isolated flip's event lands `≈ queued + 16.67 ms` —
     one cycle past any target extrapolated from the previous grid. An idle desktop
     whose flips come >5 s apart cannot NOT "miss".

## Reading their numbers with this lens

- **Sustained regime (their §3, 84% queued-early misses, headroom p50 4.83 ms):** the
  end-to-end chain their commit must complete before the tick is
  `in-fence signal latency (≈ their gpu time + venus fence delivery) + flush RTT +
  commit-worker scheduling`. Against gpu avg 2.5–7 ms, a 4.83 ms-headroom frame missing
  is *expected*; the >10 ms-headroom misses (146) are the tail of the same chain (venus
  fence-signal latency spikes are a prime suspect and tie into the ring relax/park work —
  measurable with `GPUWAKE`/`RINGWAKE`).
- **Idle regime (30% misses, headroom p5 negative):** flips separated by >5 s ride a
  freshly-anchored grid each time — and are then *guaranteed* one-cycle-late (our
  6 s-gap run: 100%). Flips ~1 s apart see a stable grid (our run: 3.3%). Their 30%
  likely = the fraction of idle flips arriving after >5 s of display quiet, plus loose
  headroom at idle; their per-window variance (headroom p50 0.08–13.15 ms) fits the mix.
  They can settle it by logging the inter-flip gap alongside each idle miss.
- Nothing here says the *pixels* are late on glass: with fence-present off, the host
  presents at flush + next CA latch regardless of what the guest's timer reports. The
  guest-side "miss" mostly poisons the compositor's frame clock / animation timing, which
  is its own real (if softer) user-visible cost.

## What we can do (assessment; item 1 IMPLEMENTED 2026-07-27)

1. **Arm the fence-present chain in the product** — **DONE** (libkrun 0110/0111 + repo
   92d3890): the worker defaults fence-present ON exactly when `LIMINA_SHOWN_ACK_FD`
   is present (windowed runs with a live ack channel); `LIMINA_FENCE_PRESENT=0`/`off`
   forces off, the `/tmp` marker stays a live force-on, and deferred presents got the
   immediate path's readback fallback so a forced-on run is correct on ack-less sinks.
   Guarded by the new L2 `venus_fence_present` (seated boot, chain forced on, the
   `[FENCEPRESENT]` oracle + no-wedge + no-dropped-frames asserts) and a libkrun unit
   test on the policy. Two side discoveries along the way: the deployed .app had been
   running with NEITHER flip pacing NOR the round-21 present-copy mitigation (both
   env-gated, neither set — the #31 stale-sampling race was live on dogfood), and the
   test harness's `with_supervisor_log` captured only stderr while worker log lines
   land on the supervisor's stdout in capture boots (both captured now).
   `boot-enhanced-efi-kk.sh` no longer forces `LIMINA_PRESENT_COPY=1` — the windowed
   default is now the June round-27 validated `FENCE=1 COPY=0` config.
   With this, the guest's flip event lands at the first guest tick ≥ the true CA
   latch — honest, at the cost of ~½ host frame of reported latency and a (real)
   increase in reported misses until the frame clock adapts.
   **Validation (2026-07-27):** full HVF suite green (36 test binaries, including the
   new guard twice); seated windowed A/B on the enhanced image — A (default, no env):
   chain engages on its own right at the venus gnome-shell handoff, 152/152 zero-copy
   flushes parked with a 1:1 injected ring-63 fence, 0 injection failures, 0 dropped
   frames, overview-animation storms clean, idle ~110 wakes/s, 2.5–3.5% CPU (trace
   logging on, post-animation); B (`LIMINA_FENCE_PRESENT=0`): override honored (0
   oracle lines), ~128 wakes/s, 1.2–1.8% CPU. No regression signal either direction;
   the June glmark/vkmark parity stands as the throughput reference. Deploying to the
   dogfood Mac (a fresh .app build) is the user's step.
2. **Stable-phase vblank timer (guest kernel patch, upstreamable to drm core).** Re-arm
   with `hrtimer_forward(timer, expiry, interval)` instead of `forward_now`, and on
   re-enable anchor to the previous grid (`last_expiry + k·interval`) instead of
   `now + framedur`. Kills the idle re-anchor chaos for every vblank-timer driver (vkms
   included). Small, self-contained, benefits stock guests once upstream.
3. **Host-refresh-locked vblank (the deep fix, design only).** Feed the host display's
   cadence (CVDisplayLink / CA latch times on the window's screen) into the guest as the
   vblank source — either periodic phase/period resync of the emulated timer or full
   host-driven vblank events via a limina virtio-gpu extension. Combined with (1), guest
   targets align with actual glass. Interacts with ProMotion/dynamic refresh and window
   moves across displays; needs a design doc before code.
4. **Measure the venus fence-signal tail** (their in-fence): extend the wake-chain probes
   to timestamp render-fence retire → guest ISR → sync_file signal, correlate with ring
   relax/park state. This is the piece our recent ring changes could have made worse.

## OPEN (separate issue): aquarium FPS instability — moved to its own memo

Found during this validation, exonerated from this session's changes, and now tracked
outside the spike: **`docs/perf/aquarium-fps-instability.md`** (symptom per host — the
M4 Pro barely wobbles where the M1 Max dives to the 30s — the full A/B exoneration
record, and the ordered leads, display-pinning trap first).

## §7 (bimodal vkCreateImage) — scoped, not yet chased

Their 0.10 ms p50 / 3.9–23 ms tail on image creation: the fast path is a plain KK
`vkCreateImage`; the slow candidates on our side are the **external/scanout-capable
creates** — `vkr_dispatch_vkCreateImage` (virglrenderer fork) strips the external-memory
structs and, for mappable formats, forces LINEAR + allocates an **IOSurface** with the
driver's rowPitch (`vkr_mtl_iosurface_alloc` → `IOSurfaceCreate`, a kernel object), plus
`vkGetImageSubresourceLayout` and later host-pointer import machinery. A one-line
env-gated timing breakdown in `vkr_dispatch_vkCreateImage` would split KK-create vs
IOSurface vs layout query; the compositor side can tell us whether the slow creates are
exactly the ones with `VkExternalMemoryImageCreateInfo` chained.

## Files

- `vblgrid.c`, `flipmiss.c` — the probes (build in-guest:
  `gcc -O2 -o X X.c -I/usr/include/libdrm -ldrm [-lm]`).
- `*.csv` — raw runs from the 2026-07-27 session (local F44 enhanced clone).
- Kernel source consulted: v7.1.4 `virtgpu_display.c`, `virtgpu_plane.c`,
  `drm_vblank.c`, `drm_vblank_helper.c` (fetched from kernel.org; not vendored).
