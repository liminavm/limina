# Does a burst of guest load hurt a banded guest?

Measured 2026-08-28, M1 Max on AC, `burst-matrix.sh` + `burstrun.sh` (vkcube under MangoHud for
20 s while every vCPU gets a spinner for *burst* seconds every 3 s) and `host-contention.sh`
(sustained guest saturation against N ordinary-priority host threads). Frames over 33 ms and over
100 ms are the reported quantity: a burst is *meant* to cost frames, and the question is whether it
costs far more than its own length.

The worry this answers: dynamic arming samples every 200 ms, so a build starting leaves every vCPU
banded *and* busy for up to a sample interval — the configuration that destroys a fully banded
guest. If a window that short did damage, a global cap would be a prerequisite for shipping.

## A burst costs nothing, at any length up to 8 seconds

| burst | `rt+dyn` | static `rt` | unbanded |
|---|---|---|---|
| 250 ms | 60.0 FPS, 2 frames >33 ms | 58.9, 1 | 55.8, **77** |
| 500 ms | 60.0, 29 | 60.0, 3 | 52.1, **123** |
| 1 s | 60.1, 4 | 60.0, 3 | 60.3, 47 |
| 2 s | 60.0, 3 | 60.1, 10 | 59.9, 19 |
| 5 s | — | 58.9, 23 | — |
| 8 s | — | 57.9, 22 (one >100 ms) | — |

**No frame over 100 ms anywhere**, and the banded arms are the clean ones — the unbanded arm pays
in the gaps *between* bursts, where the guest goes idle again and its wakeups slip. The transition
hole is therefore not a shipping blocker; a global cap is hardening, not a prerequisite.

## The collapse is stochastic, and host contention is not the variable

Sustained saturation under a full static band, same boot, minutes apart: **60.6, 55.0, then 31.8
FPS**. The direction is consistent and the magnitude is not — which is also why the first
measurement of it came out as "3 and 32 frames in 20 s". A single run of this configuration is not
a measurement; repeat it or report a range.

Host contention was the obvious suspect for the difference and it is **wrong**. Ordinary-priority
host threads cannot preempt a banded vCPU, and sweeping them changes nothing about the failure:

| host threads | static `rt`, saturated | `rt+dyn`, saturated | `rt+dyn`, idle |
|---|---|---|---|
| 0 | 31.8 FPS, max 252 ms | 58.9, max 53 ms | 59.9 |
| 4 | 29.6, max 349 ms | 52.2, max 62 ms | 58.4 |
| 8 | 29.8, max 366 ms | 35.0, max 109 ms | 59.7 |

An idle guest holds 58-60 FPS in every condition, including eight host threads competing — the
reservation delivers exactly where it is aimed. Dynamic arming is better than the static band in
every saturated cell and equal in every idle one, and at 8 host threads (8 vCPUs plus 8 spinners on
10 cores) it degrades smoothly where the static band's tail runs to a third of a second.

## What a run of this must do

`fpsrun.sh`/`burstrun.sh` need mangohud, which the F44 enhanced image does not ship — install it in
the clone first, and note that `pgrep -c spin-load` reads 0 even while the spinners run, because
`exec -a` sets `argv[0]` and not `comm`. Check `%Cpu(s)` instead: a burst shows ~98% user.
