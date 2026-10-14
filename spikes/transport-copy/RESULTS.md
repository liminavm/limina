# transport-copy — what a full-frame copy costs a guest that waits for it

Copy-on-present for the Metal transport releases a fenced primary flush when the host's copy of
the frame is done, so the guest waits for one blit. `blitlat.swift` blits a 2560×1440 BGRA
IOSurface into another (a ring of three) at ~60 Hz, one command buffer each, and reads Metal's
GPU start/end times and the completion handler against the commit.

Measured 2026-10-14 on the dev Mac (M1 Max, macOS 26.6.2), 600 blits, while F44 Venus ran the
WebGL aquarium on the same GPU (37 guest flushes/s; `data/blitlat-aquarium.txt`), µs:

| | p10 | p50 | p90 | p95 | p99 | max |
|---|---|---|---|---|---|---|
| commit → GPU start | 181 | 445 | 707 | 776 | 1276 | 1942 |
| on the GPU | 115 | 158 | 381 | 424 | 616 | 706 |
| commit → completion handler | 466 | 805 | 1232 | 1324 | 1676 | 2402 |

The same copy on the CPU (`blitlat … cpu`: lock both surfaces, `memcpy`, unlock, as virglrs's
`copy_on_cpu` does), same load (`data/cpucopy-aquarium.txt`): p50 1645, p95 3521, p99 6699,
max 13710 µs, and a core busy for all of it.

## Conclusion

The blit is cheap even on a busy GPU: a waiter sees it done in under 1.4 ms at p95. A CPU copy is
worse at every percentile and costs a core, so the copy is a GPU blit. Releasing the guest's
fence at the blit's completion is enough; holding it until the copy is on glass is what
cost a refresh. The supervisor's copy path measured ~4 ms mean from copy to show with the same
load: the blit plus a main-thread wake and an apply, which the guest does not need to wait for.
