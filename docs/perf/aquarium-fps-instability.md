# OPEN: WebGL aquarium FPS instability (host-dependent severity)

Status: **open, not scheduled** — parked here for the eventual investigation, with the
exoneration work already done so it doesn't get redone. Found 2026-07-27 during
fence-present validation (`spikes/present-miss/`), but **not caused by anything from that
session** (see the A/B record below).

## Symptom, per host

The same workload — WebGL aquarium in the guest, venus/enhanced tier, windowed — behaves
very differently on the two Macs:

| host | GPU | 500 fish | scaling |
|---|---|---|---|
| dev-mac (dev Mac) | M1 Max | **ping-pongs 40–60 fps**, seconds-long dips, sometimes to the 30s for ~a second | — |
| dogfood-mac (dogfood) | M4 Pro | unstable but only **59–60 fps** | holds **≥ ~54 fps up to 15 000 fish** (2026-07-27, dogfood-guest, fence-present live) |

The user remembers historical locked-60 runs at 500 fish on the dev Mac, so the dev-mac
behavior reads as a change *somewhere* — but the dogfood-mac data shows the stack as deployed
has huge headroom on an M4 Pro, so whatever it is either scales with thin GPU headroom
(M1 Max) or is local to the dev Mac's environment.

## What has been exonerated (A/B'd 2026-07-27 on dev-mac)

Unstable in ALL of, same window / same image clone / same host session:

1. New build + gpu-module trace logging (~2k sync log writes/s — a real perturbation in
   its own right, since fixed by the INFO engagement oracle + the non-blocking worker
   logger, but **not the cause**).
2. New build, info-only logging, fence-present **on AND off** (runtime marker toggle).
3. **The pre-session baseline worker** (virtio_gpu.rs @ 8d2e68f, `COPY=1`, the exact
   pre-session config).

So: not fence-present, not the 2026-07-27 session's changes, not the logging.

## Leads, in order

1. **The display-pinning trap** (`limina-perf-display-pinning` memory): the historical
   locked-60 runs' guest mode/scale vs these boots' — match-host → fractional scale →
   multiplied workload. Check FIRST; it has voided runs before.
2. **Host state**: the dev-mac runs followed a full day of suites/benchmarks (thermals,
   WindowServer state). A cold-boot repro on dev-mac is the cheap discriminator.
3. **The mesa-cs devenv KK ICD** build on dev-mac vs whatever the historical runs used
   (dogfood-mac runs the bundled KK from the .app — a build-provenance difference between the
   two hosts today).
4. **Guest image drift** — the enhanced.raw lineage vs the remembered runs.
5. **Raw headroom** (suggested by the dogfood-mac table): if per-frame overhead grew anywhere
   in the stack, the M4 Pro absorbs it and the M1 Max periodically doesn't. The venus
   replay regression (`limina-venus-replay-regression`, −18% gl-replay, open+accepted)
   is a known candidate for "overhead grew".

## Repro notes

- Aquarium at 500 fish in the seated guest browser; watch the FPS counter for ≥60 s —
  the dips are seconds-long, a short glance can miss them.
- Pin the guest display mode+scale before measuring (lead 1 / the pinning trap).
- The pre-session baseline worker binary was stashed at `/tmp/limina-vmm-baseline`
  (transient — likely gone; rebuild from virtio_gpu.rs @ 8d2e68f if needed).
