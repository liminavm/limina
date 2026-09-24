# Flush latency while video plays: synchronous against asynchronous decode

**Question.** Hardware decode used to run inside END_FRAME on the thread that serves the whole
virtio-gpu control queue. Does a desktop's flush still queue behind a decode, and do other
contexts' fences?

## Vehicle

- `point.sh`: boot the enhanced image on a prebuilt worker (4 vCPU / 4 GiB, 1280x800), settle, play
  a host-encoded 1280x720 30 fps VP9 clip in Firefox kiosk (VA-API → virglrs → VideoToolbox), and
  trace the guest's `virtio_gpu_cmd_queue` / `virtio_gpu_cmd_response` tracepoints for 30 s.
- `parse.py`: pairs the two by `seqno`. The gap is a command's whole trip through the host --
  queued behind earlier commands, then processed. For `RESOURCE_FLUSH` that is the part of
  flush-to-present a host stall can stretch; the present after the answer is the supervisor's.
- `ctx.py`: `SUBMIT_3D` split by context and fence flag. **A fenced command is answered only when
  its fence retires**, so its latency includes every wait the fence makes.
- `legs.sh`: `s` = virglrs `1dd26d3` (decode inside END_FRAME, with the delay knob) + libkrun
  `f393db7f`; `a` = virglrs `f93bebb` (decode on its own thread) + libkrun `32cc3776`. Each at real
  decode speed and with `VIRGLRS_DECODE_DELAY_MS=15` (inside the clip's 33 ms frame budget).
  Order s0 a0 s15 a15, twice, one boot per point. Hardware decode was live at every point (27
  decode windows each) and no trace overran.

Measured 2026-09-24 on the dev Mac (M1 Max, macOS 26.6.2).

## Flushes: the desktop no longer queues behind a decode

`RESOURCE_FLUSH`, ms, rounds 1 / 2:

| arm | p50 | p95 | p99 | max | > 16 ms |
|---|---|---|---|---|---|
| sync, real speed | 0.33 / 0.32 | 2.61 / 2.36 | 7.48 / 5.01 | 14.8 / 7.3 | 0 / 0 |
| async, real speed | 0.33 / 0.31 | 1.01 / 1.96 | 2.76 / 4.69 | 9.5 / 10.3 | 0 / 0 |
| sync, 15 ms decode | 12.98 / 12.82 | 44.3 / 46.4 | 58.8 / 62.9 | 99.6 / 83.2 | 264 / 279 of ~655 |
| async, 15 ms decode | 0.32 / 0.33 | 2.16 / 1.68 | 5.05 / 4.50 | 9.5 / 8.9 | 0 / 0 |

With decodes made 15 ms late, synchronous decode put 40% of flushes past a 60 Hz frame and the
median at 13 ms; asynchronous decode leaves flush latency where it is at real speed. At real speed
the decode is short enough that the difference is in the tail only (p95/p99), and within the
between-boot spread.

END_FRAME's own cost, from `VIRGLRS_SUBMIT_STATS`: 22-23 ms a frame synchronous at 15 ms, 0.03 ms
asynchronous, with no read, replacement or queue wait counted.

## Fences: other contexts wait behind the decoder's, through the waiter

Fenced `SUBMIT_3D` p50 / p95, ms, round 1 (round 2 agrees). Context 2 is gnome-shell, 9 Firefox's
renderer, 11 its media process, which owns the codec.

| arm | ctx 2 gnome-shell | ctx 9 renderer | ctx 11 decoder | ctx 11 unfenced |
|---|---|---|---|---|
| sync, real speed | 2.49 / 7.21 | 2.49 / 8.43 | 2.53 / 9.11 | 2.34 / 7.03 |
| async, real speed | 2.25 / 5.54 | 2.20 / 7.24 | 2.91 / 9.56 | 0.69 / 6.68 |
| sync, 15 ms decode | 18.99 / 36.99 | 23.18 / 50.69 | 25.85 / 53.85 | 20.16 / 33.56 |
| async, 15 ms decode | 4.34 / 27.21 | 19.08 / 36.76 | 25.75 / 45.61 | 0.54 / 4.09 |

The decoder's fences waiting ~26 ms is the design: a fence covers the pictures before it. The
other two contexts decode nothing, yet with the delay their fences still wait -- gnome-shell's p95
goes from 5.5 to 27 ms. **Every classic fence retires through one waiter thread in FIFO order**, so
a fence parked on a landing holds back every fence queued after it, whoever's it is. Asynchronous
decode still beats synchronous for them (gnome-shell p50 4.3 against 19.0), but it does not
isolate them. At real decode speed the effect is below this instrument's spread.
