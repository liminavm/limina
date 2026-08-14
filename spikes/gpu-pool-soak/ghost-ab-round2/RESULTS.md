# Window open/close A/B — round 2: there is no per-cycle ratchet

**Verdict: closing a window returns everything it took. Seven windows through the system left
the pool +2 regions from where it started.** The 2026-08-13 observation that prompted this —
closing an app appeared to leave ~9,000 host regions standing — was teardown lag and concurrent
activity, not retention.

## Method

Driven by the Compositor session on a seated gsrs session (seat0/tty5, Wayland, synoik pid
32874), app `gnome-font-viewer` (GTK4, single static window, no animation, exits on close).
Sampler: `../window-ab-sample.sh`, 15 s cadence, both ends of the pool on one clock. Protocol,
timings and exclusions in `RUN-NOTES.md`. Every close verified twice — window count 0 by IPC
**and** the client pid gone — and all seven client pids were distinct, so these are seven
genuinely separate processes rather than one primary instance being reused.

Scored by **segment mean at each zero-window hold**, never a single sample. That is the
comparison round 1 lacked, and lacking it is why round 1's churn arm was void.

## Zero-window holds — the ratchet test

| hold | regions | vs baseline |
|---|---|---|
| baseline | 3673 | — |
| post-arm1 (5 min window) | 3675 | +2 |
| post-cycle 1 | 3685 | +12 |
| post-cycle 2 | 3677 | +4 |
| post-cycle 3 | 3661 | −12 |
| post-cycle 4 | 3661 | −12 |
| post-cycle 5 | 3669 | −4 |
| post-cycle 6 | 3685 | +12 |
| tail | 3675 | **+2** |

Guest blobs read exactly **50 at every single zero point** and 65 with a window open — 15 blobs
per window, perfectly repeatable. The drift spans ±12 regions, which is one sample quantum and
equals the within-segment standard deviation. It has no sign and no trend: cycle 3 and 4 sit
*below* baseline. Seven windows, +2 regions.

## Per-window cost — the independent read

If the ratchet hid below the idle noise it would still show as later windows costing more.

| window | open regions | following zero | cost |
|---|---|---|---|
| arm 1 (open 5 min) | 4068 | 3675 | +394 |
| cycle 1 (30 s) | 4035 | 3685 | +350 |
| cycle 2 | 4083 | 3677 | +406 |
| cycle 3 | 4070 | 3661 | +409 |
| cycle 4 | 4059 | 3661 | +398 |
| cycle 5 | 4059 | 3669 | +390 |
| cycle 6 (30 s) | 4035 | 3685 | +350 |

Flat. Cycle 1 and cycle 6 cost the same 350, and the mean is ~385 with no trend across the six.
Teardown latency was 1 s on all seven closes, cycle 1 through 6 alike — the "ratchet as slower
retirement" shape is absent too.

**A window's cost does not depend on how long it lives**: arm 1 held its window for five minutes
and cost +394; the 30-second windows cost the same. Combined with arm 1's open segment being flat
at 4068 ± 12 over those five minutes, an idle window neither grows the pool nor accumulates with
age.

## What this leaves

- **Amplification is the whole story.** 15 guest blobs per window stand behind ~385 host regions
  — about 26 host regions per guest blob for this app. That multiplier, not orphaning, is where
  the memory goes, and it is host-side (Metal heaps and command-allocator retention, see
  `limina-vrend-gfx-region-leak`).
- **Noise floor, for future runs**: ±12 regions on a quiet seat, against 649 in round 1 on a
  session with ~80 background blobs. Anything above ~25 regions is now resolvable. Getting the
  guest quiet is worth more than any amount of extra sampling.

## Method notes worth carrying

- **Verify the close by the CLIENT pid, not the spawn pid.** The driver's dry run found the
  spawn returns the `sudo` wrapper (38370) not the client (38374); checking the wrapper would
  report a clean close even if the client survived holding its GPU context — precisely the
  failure being tested for. Take the pid from the window's own IPC `pid` field.
- **Confirm the seat is active.** An inactive VT renders at ~1 fps here, which changes the
  allocation pattern with nothing visible in the counters.
- A human watched the screen during the baseline and arm 1 and confirmed the window appeared,
  with no interaction and no VT switch. Stated rather than hidden; the run is uncontaminated.
