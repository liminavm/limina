# present-pacing — when is the replaced scanout surface actually off glass? (#24)

2026-07-30. Context: the zero-copy reuse tear (`limina-present-miss` memo, #24). The
supervisor acks `shown X` from the CATransaction completion block; the worker completes
the guest's held flush fence on that ack (virtio_gpu.rs "Acked = latched: complete
immediately"); the guest then treats the flip as done and repaints the buffer X replaced.
Question: at completion-block time, has WindowServer actually stopped reading that
replaced buffer?

## Probes

- `useprobe.swift` — alternate two IOSurfaces as plain `CALayer.contents` at 60 Hz; at
  each transaction's completion block, check `IOSurfaceIsInUse(prev)` and poll until it
  clears. CAVEAT: with only two surfaces the "prev" goes back onto the layer 16 ms later,
  so the "never cleared" tail is an artifact — kept as a record of that trap.
- `useprobe1.swift` — single-shot variant: swap once, no further commits, poll the
  replaced surface until clear. This is the honest instrument.

## Results (dev-mac, M1 Max, macOS 26.5, 2026-07-30)

- **WindowServer DOES hold an IOSurface use count on plain layer-contents surfaces.**
  (Not just CAMetalLayer drawables.)
- **The replaced surface is still in use at completion-block time in 19/20 single-shot
  rounds** (296/300 in the 60 Hz variant).
- Single-shot clear latency past the completion block: **p50 17.1 ms, p90 24.3 ms,
  max 32.9 ms** — about one refresh, sometimes two. It clears without any further
  commit (the hold is per-composite, released once the new frame composites).

## Conclusion

The latch-ack is a lie by ~one refresh: the guest gets flip-completion while
WindowServer is still sampling the buffer it is now free to repaint. That window is
exactly the observed tear (copy-mode kills it; fence oracle exonerated the fence chain).

Fix shipped in the supervisor (crates/limina/src/window/): the shown-ack message now
carries the surface each frame replaced as layer contents, and the dedicated ack-sender
thread holds the ack until `IOSurfaceIsInUse(replaced)` clears (500 µs poll, 50 ms cap;
the worker's 150 ms fallback stands behind it). Kill switches: `LIMINA_ACK_ONGLASS=0`
(env, whole run) and `touch /tmp/limina-ack-latch` (live, re-stat'ed at 500 ms — never
per ack). A same-surface re-flush (single-buffered guest) carries no replaced surface
and is acked at latch as before — pacing can't protect single buffering.

Expected side effect: guest flip-completion moves to the true off-glass boundary, so
miss scores that counted the early ack as punctual will read higher-but-honest;
async-vs-sync score convergence is the validation instrument.
