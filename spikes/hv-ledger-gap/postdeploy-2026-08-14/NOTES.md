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

---

# The 10:33–11:24 churn, characterized (2026-08-14, afternoon)

Handed over as an uncharacterized observation: 60 balloon reversals >1 G in a 50.5-min window,
range 0.25–18.51 G, `inelastic` the modal decision at 788 of 2894, alongside a `phys_footprint`
ratchet to 23 G. Nobody had correlated it against what the guest was doing. Analysed from the
full boot trace (`balloon-trace.jsonl`, 09:00 deploy → 11:31), plus read-only guest journal.

## Verdicts on the two premises handed over

**(1) `inelastic` DOES mean what it says here, and it is not the problem.** Read the emission
site: `Hold::Inelastic` is reachable only on the *inflation* path — the policy wanted more
balloon, the last judged step was reclaim-fed, host is Normal (`balloon_policy.rs`, the
`i.inelastic && i.host == Normal && i.mode != Aggressive` gate). Of all 788 rows in the window,
**zero** are at the stranded shape: balloon actual min 12.49 G, median 16.24 G, free median
614 MiB. Every one is the *designed terminal state* the 08-13 write-up describes — a nearly-full
balloon declining to dig into page cache. The label's known conflation (benign vs stranded) is
real as a documentation defect but did not mislead here; the discriminator is one field,
`actual_bytes`, on the same row.

Corroboration: 09:09–09:20, balloon pinned flat at 16.12 G, `inelastic` 59×/min for eleven
straight minutes, guest completely quiet. That is what a *healthy* `inelastic` looks like.

**(2) Not the 08-13 give-back class, not the 07-03 PSI class — a third, distinct one.**
Of 177 sent target *decreases* in the window, **177 are `set` and 0 are `giveback`** (only 5
give-backs fired at all). The `GIVEBACK_AVAIL_CEILING_PCT` guard deployed at 09:00 is doing its
job. Memory PSI was ~0.00% and io-full ~0.1% through the churn, so the 07-03 PSI limit cycle is
out too. The churn runs entirely through the **allowance-shortfall deflate**.

## The mechanism: a ~19 s limit cycle on the shortfall-deflate path

The guest's demand is real (see workload below); the policy's *response* to it is what
oscillates. One full cycle, tick by tick (10:52:35–10:53:14, and it repeats near-identically
for the whole window):

```
10:52:35 set  17.90->17.93  act 17.90  free 1.29G  avail 3.02G   <- converged: avail ~= allowance
10:52:36 set  17.93->16.99  act 17.93  free 0.46G  avail 2.06G   <- guest takes ~0.85G; shortfall
10:52:37 set  16.99->16.52  act 16.99  free 0.91G  avail 2.53G   <- SECOND deflate on a STALE avail
10:52:39 dwell             act 16.52  free 2.52G  avail 4.20G   <- overshot: 1.41G given for 1.05G
10:52:40..10:53:13   set +0.25G / dwell, x6                      <- 14 s re-inflating at 256M/2s
10:53:14 set  17.94->17.23  ...                                  <- and again
```

**Convergence parks the system exactly on the trigger boundary.** `desired = current + (avail −
allowance)`, so a converged Moderate balloon sits at guest `avail ≈ allowance ≈ 3.11 G` (max/8 on
this 24 G VM) — measured convergence 3.01 G. From there *every* transient the guest takes crosses
the boundary by construction. On a busy guest that is not an edge case, it is the steady state.

**The overshoot is a staleness defect, and it has TWO sources — an actual-based clamp alone
would only catch one.** At 10:52:37 `actual` had already reached the target sent at :36 (the
driver deflates at ~2.5 GiB/s, well inside one 1 Hz report), so nothing was in flight: what was
stale is **meminfo**, an `avail` of 2.53 G that had credited only 0.47 G of the 0.94 G already
returned. But at 11:23:55–56 the other shape appears — targets 13.49 → 12.16 → 11.01 G while
`actual` was still 14.39/14.07 G, i.e. **the driver lagging by >1 G across consecutive
deflates**. Result either way: 1.41 G handed back for a 1.05 G shortfall — 0.36 G of pure
overshoot per event, then 14 s of re-inflation to undo it. A RED-first fix should reproduce
*both* shapes; settling one report after a sent shortfall deflate covers both, an actual-based
clamp passes the 11:23 case and fails the 10:52 one.

**The transient is the guest's, not the balloon's.** On the dip ticks the balloon moved ±0.02 G,
so it is not the balloon consuming the free list. Discriminating test — 1-second MemFree drops
>400 MiB:

| window | balloon | drops >400M/min |
|---|---|---|
| 09:09–09:20 (flat, `inelastic`) | pinned 16.12 G | **0.0** |
| 10:50–11:00 (churn) | 11.83–18.11 G | **4.7** |

## The 10:41 release-to-0 was real, and correct

The largest single event (18.51 G → 0) was not part of the cycle. Trigger row:

```
10:41:24  set 16.03G -> 0   free 395M  avail 1423M  some_avg10 10.78%  io_full 13.17%  host=warn
```

`some_avg10` crossed `PRESSURE_HIGH` (10%) on a genuinely squeezed guest, with the host itself at
`warn`. Acute release fired as designed, preceded by one `giveback` at 10:41:22. Only **two**
"Out of puff" lines appear in the guest kernel log for the whole window (10:41:13, 11:04:06) —
no held-unreachable-target loop. (A `journalctl | grep -i oom` over this guest returns ~300 hits
that are all `synoik` frame-log lines containing "head**room**". Grep for `Out of puff` or
`oom-kill`, not `oom`.)

## The footprint "ratchet" is the balloon being empty, not a leak

Reconciled against the trace: the ~23 G readings sit inside 10:42–10:45, when the balloon was at
**0** after the acute release — the worker then bills essentially the whole 24 G guest, which is
honest. Across the full boot `pf` oscillates with the balloon rather than ratcheting (8.06 G at
10:00 with an 18.4 G balloon; 25.7 G at 10:42 with a 0 G balloon), and it is **10.3 G live at
11:36**. Sweeps kept up: 41 this guest-boot epoch (the counter resets on a guest reboot, as at
09:25 and 09:35 — it is not a per-worker-lifetime total). Treat the two halves as one picture, as handed
over — but the picture is "the balloon emptied", not "the ledger leaked".

## What it cost

- `released_bytes` **97 G → 203 G in 52 minutes** — 106 GB of release traffic (unmap + REUSABLE
  + queue-zeroing) to hand back and re-take the same ~1.4 G, ~60 times.
- `pf` sawing 10.9 ↔ 13.3 G once per cycle.
- Demand sweeps pinned at their `DEMAND_SWEEP_MIN_INTERVAL` 60 s floor for the whole window
  (19 in 52 min) — each cycle re-opens the ≥4 GiB gap the demand sensor watches.
- Guest-visible harm: none measurable. PSI ~0, io-full ~0.1% between the acute events.

So this is a **cost defect, not a correctness defect** — which is also why it went unnoticed:
every guard reads healthy while it runs.

## The workload (the correlation nobody had done)

The churn window brackets a compositor development loop on the dogfood guest:
`gnome-session` restarted **10:37:47–51**, four minutes into the window; the session runs
`synoik` (the user's gnome-shell/mutter replacement) with a `journalctl -f --grep=synoik`
attached; a Claude Code session in the guest is working in `~/Projects/gnome-shell-rs` (an abrt
record at 10:49 names its scratchpad). Repeated `sudo` invocations 10:39–10:41. Load average had
decayed to 0.23/1.84/2.70 by 11:36, matching the churn stopping ~11:24.

A cargo build plus repeated compositor/session restarts is exactly a repeated
allocate-~1-GiB-then-free transient on a few-second cadence. **The demand is legitimate.**

## Proposed fix (not implemented — policy trade is the user's call)

Two changes, both scoped strictly to the **allowance-shortfall deflate**:

1. **Settle one report after a sent shortfall deflate.** Skip (or discount by the amount just
   released) the shortfall term on the next report, so a release cannot be counted twice against
   a meminfo sample that has not yet credited it. Kills the ~0.45 G/event overshoot.
2. **Require the shortfall to persist** — two consecutive reports, or a small hysteresis band
   below the allowance — before deflating on it. A sub-3-second transient inside the allowance
   is precisely what the allowance exists to absorb; reacting to a single 1 Hz sample turns a
   0.8 G blip into 2.8 G of balloon movement.

**Guardrail, load-bearing:** this must touch *only* the allowance-shortfall path. The acute
release-to-0 (`some_avg10 >= PRESSURE_HIGH`), `guest_starved`, and the pressure give-back stay
immediate and unconditional. Gating deflation is what produced both the 07-03 limit cycle and
the 07-09 sticky-Warn wedge; the whole point of "deflation is immediate at any pressure" is that
those paths never wait. RED-first when it is implemented.

**Open, for the user:** the deeper question is whether converging to `avail == allowance` is the
right resting point at all. It guarantees that every guest transient crosses the deflate
boundary. A resting point slightly *above* the allowance (converge to allowance + one step, say)
would give the cycle somewhere to absorb a blip without moving the balloon — but it costs that
much standing balloon on every VM. That is a tuning trade, not a bug fix.

## Monitoring

`spikes/hv-ledger-gap/balloon-watch.py` (new) replaces `decision-tail.py` for forensic watching:
it filters **nothing**, renders every record, and emits a per-minute CENSUS line counting every
label seen plus the balloon's range and reversal count. `decision-tail.py`'s QUIET set drops
`dwell`/`dead-band`/`cooldown`/`not-calm` — two of this cycle's three phases — so the cycle's
inflation half was invisible in the alert stream. Keep `decision-tail.py` as the alert stream;
use `balloon-watch.py` when the question is "what is actually happening".
