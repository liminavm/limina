# What a banded VM charges the host

Measured 2026-08-28, `host-impact.sh` — the `wakeprobe` oracle running **on the host** while a
guest runs under each policy, idle and saturated, two reps each. The quantity is what an ordinary
host thread pays for a 16.667 ms deadline: the same instrument, and the same units, as the host
baseline that started this investigation.

This is the question every other measurement here dodges. The band is a *reservation*: it takes
cores from everything not banded, which on a laptop is the user's editor, their browser, and any
second VM. A policy that fixes a guest by making its host stutter is not a fix.

## An ordinary host thread's lateness, in microseconds

| guest policy | guest | p50 | p99 | max |
|---|---|---|---|---|
| unbanded | idle | 2006, 2019 | 2261, 4623 | **2384, 6968** |
| unbanded | saturated | 2018, 2019 | 5517, 3212 | **5988, 6142** |
| static `rt` | idle | 2019, 2019 | 7775, 8423 | **14941, 8611** |
| static `rt` | saturated | 2018, 1018 | 3757, 13491 | **10259, 24975** |
| `rt+dyn` | idle | 1019, 2017 | 2520, 5992 | **2565, 5999** |
| `rt+dyn` | saturated | 2018, 2018 | 5799, 2766 | **5992, 7575** |

**Banding every vCPU costs the host its tail.** Worst-case lateness runs 8.6-25.0 ms under the
static band against 2.4-7.0 ms unbanded — a host app on a 60 Hz screen loses a frame, sometimes
more than one. **Dynamic arming does not**: 2.6-7.6 ms, inside the unbanded range in every cell.
That is the same ordering the guest-side numbers gave, measured from the other side.

The median is *not* the signal here, and reading it as one would invert the conclusion. It is
bimodal at 1 ms or 2 ms across reps of every arm, including unbanded — the host's own power state,
not the policy. The one rep where it halved to 1018 µs is also the rep with the worst tail
(24975 µs): a busy machine keeps cores awake and wakes threads *sooner* on average, the same effect
that makes a loaded guest render better, while the tail goes the other way. A mean hides both.

## A reservation still buys an escape, under every arm

A *banded* host thread measured 18-22 µs at p50 in all twelve cells, tails 70-780 µs. Eight vCPU
reservations do not close the real-time class to anyone else — a host thread that needs punctuality
can still ask for it and get it, which is what makes the band a safe thing for the app itself to
use later.

## Method notes

Repeat every arm: the guest-side collapse is stochastic and so is this — the static band's worst
cell (24.9 ms) and its best (8.6 ms) differ threefold. Two reps is the minimum that shows a range;
it is not enough to compare two arms that land close together.
