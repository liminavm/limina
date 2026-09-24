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
asynchronous. The stats' decoder counters were not connected at these points (virglrs before
`b900066` dropped them after the first 2 s window), so this pass says nothing about how often a read
waited for a picture.

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

The FIFO is not the waiter's choice. Every classic fence in these traces carries flags `0x1` and
never `0x2` (`INFO_RING_IDX`), and fence ids form one sequence interleaved across contexts: stock
Mesa's virgl winsys puts every GL context on the guest's device-wide fence timeline, where
delivering a fence signals every older one. Retiring another context's fence first would signal the
decoder's early.

## Across frame sizes, at real decode speed

`legs-res.sh`: the same vehicle with 720p, 1080p and 4K VP9 clips (30 fps), no delay. Asynchronous
decode at virglrs `b900066`, whose stats report the decode thread's phases and the render thread's
waits for the decoder; synchronous decode (`1dd26d3`) at 4K for contrast. Two rounds each, one
boot per point. Every point decoded in hardware at 30 fps.

Per decode, frame-weighted mean (worst), ms:

| clip | queued | VideoToolbox | plane write | reads that waited, per 30 s run |
|---|---|---|---|---|
| 720p | 0.28-0.44 (58) | 2.10-2.19 (10.9) | 0.10-0.11 (1.9) | 1 and 1 (12, 38 ms) |
| 1080p | 0.24-0.34 (53) | 2.77-2.85 (10.6) | 0.22-0.23 (2.9) | 0 and 1 (14 ms) |
| 4K | 0.38-0.56 (59) | 5.60-5.63 (22.8) | 0.63-0.70 (8.8) | 0 and 0 |

No decode waited for room in the queue or to replace an unread picture.

The desktop, same points (ms); the synchronous row is from the same vehicle's first pass
(`evidence/res-s2160-r*`):

| clip | flush p50 | flush p99 | gnome-shell fence p50 | gnome-shell fence p95 |
|---|---|---|---|---|
| 720p async | 0.29-0.30 | 3.03-3.07 | 2.35-2.39 | 4.67-5.07 |
| 1080p async | 0.30 | 2.75-2.85 | 2.41-2.43 | 3.60-3.61 |
| 4K async | 0.31-0.33 | 2.36-5.91 | 2.53-2.70 | 7.04-8.97 |
| 4K sync | 0.34-0.35 | 11.84-12.83 | 2.51-2.55 | 10.57-10.62 |

**What bounds another context's fence:** one decode, which is VideoToolbox time plus the plane
write. At 4K that is about 6.3 ms a frame, and gnome-shell's fence p95 rises from ~4 ms to 7-9 ms;
its median does not move. Of that decode, the plane write is ~11% and the time queued ~8%; the rest
is the hardware decode. Removing the plane write entirely would take at most ~0.7 ms off a 4K
decode, and raising the decode thread's priority can only act on the queued share, whose mean is
under 0.6 ms. Neither changes the picture at any size measured here.

**Reads that outran the decoder are rare but not free:** at most one per 30 s run, each blocking
the render thread 12-38 ms -- a read that reached a target while its picture was still decoding.
Nothing in the design rules them out; they are timing.

## The stock tier

`legs-stock.sh`: `Fedora-Workstation-44.stock.test.raw` (stock kernel and mesa, plus RPM Fusion's
freeworld VA driver), the same clips, played by **Showtime**, GNOME's player. Stock Firefox is no
vehicle here: it decodes in software on this image (one host decode in a 30 s run,
`evidence/stock-pilot-a1080`). A stock guest decodes into per-plane targets, whose planes upload
with `glTexSubImage2D` on the render thread when they are read; virglrs times those uploads
("plane uploads on the render thread") and, in the confirming point, VideoToolbox session
creation. Two rounds, one boot per point, 2026-09-24.

Per plane, mean (worst), ms; two planes a frame at 30 fps:

| clip | render-thread upload | VideoToolbox decode | END_FRAME per frame |
|---|---|---|---|
| 720p | 0.15-0.17 (5.6) | 2.3-2.4 | 0.37-0.44 |
| 1080p | 0.25-0.26 (6.0) | 3.0-3.1 | 0.59-0.63 |
| 4K | 0.45-0.46 (5.3) | 5.9 | 1.02-1.05 |

**The uploads are not where the stock tier's time goes.** At 4K they are ~0.9 ms a frame, 2.7% of
the render thread's wall. Moving them off it -- unpack buffers, or a decode-thread GL context -- can
save at most that.

The desktop (ms):

| clip | flush p50 | flush p95 | flush p99 | gnome-shell fence p50 / p95 | Showtime fence p50 / p95 |
|---|---|---|---|---|---|
| 720p | 0.30-0.38 | 3.04-3.35 | 4.78-5.51 | 2.34 / 4.6-5.2 | 2.8-3.3 / 6.7-6.9 |
| 1080p | 0.29-0.31 | 3.29-4.19 | 6.37-6.78 | 2.3-2.5 / 4.2-5.5 | 3.7 / 7.2-7.7 |
| 4K | 0.28-0.30 | 2.92-3.37 | 6.76-7.31 | 2.4-2.5 / 8.2-8.8 | 10.1-10.3 / 13.8-14.1 |

The idle points have no floor to offer: the stock desktop sends almost nothing when nothing moves
(one flush in 30 s). What the uploads do not explain is Showtime's own fences at 4K, a 10 ms
median.

**Every playback starts with the control thread blocked for ~65 ms.** In every run, and only in
the first stats window: 4-7 decodes waited for room in the codec's queue, 66-78 ms in all, the
longest 62-66 ms, which is END_FRAME holding every context's commands. The confirming point
(`evidence/stock-create-1080`) names the cause: the first frame builds the VideoToolbox session on
the decode thread, **66.6 ms**, while gst-va submits frames ahead into a queue four deep. Firefox
on the enhanced image pays the same creation (the 53-59 ms worst queued times above) but submits
fewer frames ahead, so its queue did not fill.
