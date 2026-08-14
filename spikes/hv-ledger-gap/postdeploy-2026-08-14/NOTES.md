# Field notes: the availability guard's first hours (2026-08-14, post-deploy)

Running notes on the build carrying `GIVEBACK_AVAIL_CEILING_PCT` (commit 7994695), deployed to
the dogfood Mac at 09:00:26. Written as observations land; conclusions belong in a RESULTS.md
once there is enough to conclude. Paired host+guest samples in `samples.csv` (30 s cadence).

## CAVEAT on every graphics-pool number taken before ~09:25

The dogfood guest was still running the **retired zink-as-guest-GL** configuration
(`MESA_LOADER_DRIVER_OVERRIDE=zink`) through this morning *and* through the 2026-08-13 gpuscore
run, and was rebooted onto the supported one (`virtio_gpu` / `GALLIUM_DRIVER=virgl`, GL on
**vrend**) after that. GL therefore travelled a completely different allocation path in every
graphics-pool sample taken before the reboot.

**Consequence: the amplification distribution must be re-measured before it is optimised
against.** The 105 / 26 / 7.7 host-regions-per-guest-blob figures in
`spikes/gpu-pool-soak/gpuscore-2026-08-13/RESULTS.md` are zink-path numbers, and the plan of
record is to attack the expensive small-surface end of that distribution — on a configuration we
no longer ship.

What does carry over: the *method* (paired host+guest sampling on one clock, driver-stated phase
marks), and the "no host-side retention" conclusion, which rests on four independent routes
including the gdm drain and the window A/B — neither of which is zink-specific.

The balloon observations are unaffected: the policy does not see the GL driver.

## 09:02:36 — the combined guard permits a release, correctly

First field observation of the *combined* guard firing rather than declining.

```
bal 14.25 G   free 567 MB       (under the 1 GiB ceiling)
              avail 4.45 G      of 9.47 G total = 47.0%  (under the 50% ceiling)
              io-full 11.23%    memory PSI 0.00%
```

Both terms agree the guest is short, so the arm fires. Worth contrasting with restic that
morning, which sat at **68% available on a comparable free-list level** and was declined: the
availability term is discriminating, not merely echoing free.

Caveat on reading this one: the balloon was still ramping from boot (0 -> ~17.8 G) and had
overshot into the guest, so this is the ramp correcting itself rather than a settled guest under
load. Cleaner episodes are the ones to weigh.

## 09:03 — a compile is anon-shaped; memory PSI separates it from a streaming read

Guest-side, mid-build (rustc x5 + ld.mold, largest RSS 597 MB):

| | this build | restic, 08:01 |
|---|---|---|
| AnonPages | 2.93 G | 2.07 G |
| Cached | 4.83 G | 8.74 G |
| MemAvailable | 5.18 G (52.6%) | 10.6 G (68%) |
| memory PSI some avg10 | **0.97%** | **0.00%** |
| io PSI full avg10 | 1.07% | 11-18% |

The prediction going in was that a compile allocates anonymous memory and so would push
`MemAvailable` *down*, unlike a streaming read that parks everything in cache. It held.

**Memory PSI cleanly separates the two workloads** (0.97% vs a flat 0.00%). That does not
overturn the 2026-08-13 decision not to *gate* on memory PSI — the 07-09 wedge had it silent on
a genuinely starved guest, which is the whole reason the io arm exists — but it is a strong
corroborating signal and was underweighted when the free ceiling was chosen.

## 09:03 — the false-negative boundary, live

At that same sample the guest would have been declined by **both** terms:

- free 1.33 G — above the 1 GiB ceiling
- available 52.6% — above the 50% ceiling
- memory PSI some avg60 0.59% — below the 2% bar that fires the memory arm

So had io-full crossed 10% at that instant, nothing would have released. On this sample that is
the *right* answer: io-full was 1.07% and `some` 8.16%, a working build rather than a thrashing
one, and a guest with 5 GB reclaimable is not starving.

But it is three points of availability and ~300 MB of free away from refusing a guest that
genuinely needs memory. **And the denominator is the balloon** (see the backlog item): a
give-back raises availability, which makes the *next* decision more likely to decline. The
signature to watch for is a give-back ladder that stops mid-episode with the guest still hurting.

## 09:23 — a GUEST reboot skips the ramp entirely (the VMM survives, so the policy keeps state)

Two restarts an hour apart, and they behave completely differently:

| | first target after restart | time to rest |
|---|---|---|
| 09:00:26 VMM restart (deploy) | 256 MiB — ramps in ~70 steps | ~2 min |
| 09:23:23 guest reboot | **16.87 G immediately** | 8 s |

On a guest reboot the worker never dies, so the policy retains its converged target and re-takes
everything in one move:

```
09:23:23  bal  0.00 G   free 22.66 G
09:23:31  bal 16.99 G   free  1.13 G     17 G reclaimed in 8 seconds
                        io-full 9.44%    (the give-back bar is 10.00%)
```

The guest finished booting at 17.58 G / 976 MiB free without ever crossing the bar, so nothing
went wrong. But note where it sat while booting: free just above the 1 GiB ceiling and io-full
just under the 10% trigger — **both guard terms a hair on the "decline" side**, with no ramp
behind it and only a give-back available as a correction. Worth keeping in mind for a guest that
boots into something memory-hungry, and worth deciding whether the retained target should be
re-converged rather than re-applied after a guest reboot.

## 09:31 — `free-exhausted` is a misnomer: it means "the pacing clamp bound this step"

Second guest reboot (back to zink), same retained-target catch-up as 09:23. The whole ramp
printed `free-exhausted`, at every free level on the way down:

```
09:31:47  free-exhausted  bal  8.45 G   free 10257 MiB
09:31:48  free-exhausted  bal 10.69 G   free  7795 MiB
09:31:49  free-exhausted  bal 13.06 G   free  5352 MiB
09:31:50  free-exhausted  bal 15.47 G   free  2873 MiB
09:31:51  free-exhausted  bal 17.79 G   free   579 MiB
09:31:52  set             bal 18.40 G   free   539 MiB
```

Ten gigabytes free is not an exhausted free list. The verdict is really "the pacing clamp
limited this step so free would not be driven below `free_margin_pages`", which during a fast
catch-up is true on *every* tick irrespective of how much memory the guest has.

Benign as behaviour — the clamp is doing its job, and this is why the 24 G re-inflate does not
starve the guest. But the **label actively misleads**: reading a trace and seeing
`free-exhausted` at 10 GB free, the honest first reaction is either "the trace is wrong" or "the
guest is in trouble", and both are wrong. Same conflation family as `inelastic` (benign
declining-to-dig vs genuinely stranded). Cheap fix: distinguish "clamped by pacing" from "the
guest has nothing left", which are different states that presently share one name.

## 09:51 — measured: sweeps are PRODUCTIVE, ~871 MiB each

Repeated demand sweeps against a persistent ~4.3 G gap looked like busywork, so measure the
cumulative counters instead of inferring from the per-sweep records:

```
sweeps = 4    cumulative sweep_debited_bytes = 3484 MiB    avg = 871 MiB/sweep
sweep_faults = 0    sweep_ms = 169
footprint 9.55 G    compressed 0.91 G    balloon 16.39 G
```

Nearly 900 MiB reclaimed per sweep, no faults, sub-200 ms. **The "cadence sweeps run at
near-zero yield" backlog item is not supported by anything observed today** — and this is the
second time in one morning the zero-yield story failed under measurement (the first being the
retracted null-read below). It may have been true when written, on a different build or
workload, but it should be re-grounded rather than carried as received wisdom.

## 09:03:05 — RETRACTED: the "zero-yield sweep" was a misread start record

Originally written up here as a demand sweep debiting **0 MB against a 4,158 MB gap** — the
sharpest instance yet of the "cadence sweeps run at near-zero yield" backlog item. **It is not a
finding.** The raw record reads:

```
{'sweep': 'demand', 'gap_bytes': 4359994784, 'debited_bytes': None}
```

`debited_bytes` is **null**, not zero: this is the sweep's START record, emitted before the debit
is known. That sweep went on to debit **3,480 MiB** (visible as the cumulative
`sweep_debited_bytes` at 09:16). Nothing about it was low-yield.

The cause was a tool fix made an hour earlier, in this same investigation. `decision-tail.py`
had crashed on `debited_bytes` being null, so every numeric read was routed through a helper
coalescing null to 0. Correct for arithmetic that must not crash — **wrong in a display path,
where it invents data**, and it erased precisely the distinction the balloon policy is careful
to keep (`mem_free_kib == 0` means "not reported", never "no free memory").

Lesson worth more than the retracted finding: a null-safety fix applied uniformly can manufacture
observations. Display now renders unreported values as `?` (`show_mib`), so a start record can
never again be read as a zero.
