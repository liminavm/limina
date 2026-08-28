# What a scheduling policy on the vCPU threads does to a guest's frame times

Stock Fedora 44, 8 vCPUs, `vkcube` 900x600 under MangoHud, 20 s samples, two reps each, on one
`cp -c` clone booted fresh per policy (`arm-matrix.sh`). Frame times in ms.

Guest states: **idle** is vkcube alone; **loaded** is six CPU spinners against eight vCPUs, so
there is spare capacity; **saturated** is one spinner per vCPU, so every vCPU thread always has
guest code to run. Only the saturated shape reaches the failure, which is why "load" was the wrong
word for it.

| policy | state | avg FPS | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| none | idle | 43.6 / 39.2 | 19.12 / 25.47 | 34.59 / 35.92 | 42.20 / 49.88 | 66.65 / 64.93 |
| none | loaded | 54.5 / 55.7 | 16.88 / 16.82 | 23.94 / 20.10 | 33.98 / 38.74 | 50.87 / 50.71 |
| none | saturated | 60.4 / 60.4 | 16.64 / 16.66 | 17.20 / 17.09 | 31.62 / 22.52 | 159.62 / 63.56 |
| `rt` (all vCPUs) | idle | 59.5 / 59.7 | 16.66 / 16.67 | 18.02 / 17.85 | 19.56 / 22.99 | 50.07 / 31.49 |
| `rt` | loaded | 59.6 / 59.7 | 16.67 / 16.67 | 16.72 / 16.73 | 20.29 / 18.26 | 48.45 / 78.50 |
| `rt` | saturated | **3 and 32 frames in 20 s** | — | — | — | 7147 / 7623 |
| `rt+hb` (heartbeat) | idle | 60.4 / 59.7 | 16.60 / 16.68 | 17.88 / 17.87 | 19.44 / 19.86 | 34.58 / 48.59 |
| `rt+hb` | loaded | 59.6 / 59.6 | 16.67 / 16.67 | 16.76 / 16.78 | 20.63 / 20.71 | 48.39 / 37.37 |
| `rt+hb` | saturated | **28, 26, 60, 18 frames in 20 s** | — | — | — | up to 9952 |
| `rt` computation 15 ms | idle | 59.5 / 59.6 | 16.67 / 16.67 | 17.74 / 17.84 | 19.79 / 22.53 | 50.33 / 40.60 |
| `rt` computation 15 ms | saturated | **80 and 37 frames in 20 s** | — | — | — | 3164 / 2323 |
| `rt#1` (vCPU 0 only) | idle | 52.1 / 53.0 | 17.33 / 16.81 | 29.77 / 30.55 | 39.91 / 41.88 | 60.64 / 56.45 |
| `rt#1` | saturated | 59.9 / 57.8 | 16.67 / 16.66 | 16.99 / 17.10 | 22.64 / 33.46 | 49.54 / 46.13 |
| `qos` (USER_INTERACTIVE) | idle | 47.2 / 46.6 | 18.21 / 18.18 | 32.96 / 33.22 | 39.19 / 39.73 | 52.71 / 56.16 |
| `qos` | loaded | 55.0 / 58.6 | 16.80 / 16.75 | 19.64 / 17.43 | 39.64 / 33.16 | 60.14 / 47.00 |
| `qos` | saturated | 59.2 / 59.4 | 16.66 / 16.66 | 17.54 / 17.44 | 33.65 / 33.39 | 147.36 / 57.04 |
| **`rt+dyn`** (per-vCPU) | idle | **58.6 / 59.5** | 16.77 / 16.65 | 18.26 / 17.66 | 21.61 / 21.41 | 44.64 / 49.15 |
| **`rt+dyn`** | loaded | **59.7 / 59.7** | 16.67 / 16.67 | 16.74 / 16.73 | 19.42 / 18.55 | 38.10 / 31.88 |
| **`rt+dyn`** | saturated | **59.8 / 59.1** | 16.66 / 16.66 | 17.09 / 17.14 | 31.62 / 33.45 | 50.19 / 60.97 |

A collapsed arm is reported as frames rendered, not as FPS and percentiles: with 3 samples in 20 s
those statistics describe nothing.

## What the collapse is

Not xnu's real-time fail-safe. A demoted thread is an ordinary timeshare thread, which is the "no
policy" row — and that row is clean at 60.4 FPS, so demotion cannot produce an 8-second frame. The
heartbeat that exists to prevent demotion demonstrably fires (1200 forced parks logged during one
run) and changes nothing, and raising the declared computation to 15 ms of a 16.667 ms period does
not fix it either.

It is host-side starvation, observed directly (`starvation-probe.sh`): during the collapse all
eight vCPU threads sit at 100% CPU at priority 97 in the real-time band, and every other thread in
the worker — including the venus ring thread and the GPU worker that carry the present — is at
0.0%. `LIMINA_RING_WAKE_PROFILE=1` at the same moment shows `signal->resume` at **28.7 ms average,
434.9 ms worst** in the long-park bucket, against the 8-27 µs it measures with no policy, plus a
lost signal. Banding one vCPU instead of eight is clean under the same load, and a QoS class, which
carries priority but no reservation, never collapses.

The band is a *reservation*. Promising it on every vCPU thread promises away the machine.

## Why per-vCPU arming is the shape

The vCPU that needs a punctual timer wake is the one that is idle between frames — and an idle
thread is also the one whose reservation costs the host nothing. `rt+dyn` samples each thread's own
share of a core every 200 ms and arms below 35%, disarms above 60%. The gap is hysteresis: a policy
change is exactly the moment the present path can lose its core, so a thread hovering at the
threshold must not switch every sample.

`rt#1` is the reason a static choice will not do: banding vCPU 0 alone recovers only about half the
idle gap (52-53 FPS against 59.5 banded and 39-44 unbanded), because the deadline that matters
lives on whichever vCPU the guest scheduler put the client on, and that migrates.

## What the arming costs at idle

Same clone, no client running, worker sampled after a 60 s settle (`idle-cost.sh`, eight 10 s
samples of `top -stats pid,cpu,idlew`):

| policy | worker CPU | idle wakeups |
|---|---|---|
| none | 2.2-4.7% | 22-30 per 10 s |
| `rt+dyn` | 2.2-4.7% | 18-29 per 10 s |

No measurable difference either way. The sampler is one thread at 5 Hz for the whole VM, against
the per-vCPU kicker the heartbeat attempt needed (8 Hz × 8 threads), and it does not show up in the
idle-wakeup count at all — which still leaves the outer gate worth building, so it stops sampling
entirely when nothing is presenting.
