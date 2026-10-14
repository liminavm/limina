# promotion-link — what a window's display link delivers on a ProMotion panel

The host vblank (`docs/design/host-vblank.md`) states each screen's vsync grid from short bursts
of an `NSWindow` display link (`crates/limina/src/window/vsync.rs`). Moved onto the built-in
panel, the F44 Venus window's second guest output never got a grid: every burst logged
`vsync: burst of 60 vsyncs read nothing`. This measures why.

## Vehicle

`linkprobe.swift` opens a window on a chosen screen, takes its display link (or the screen's,
`screenlink`; or natively fullscreen, `fullscreen`), optionally asks for 120 Hz
(`preferredFrameRateRange`) and swaps the layer's IOSurface on every callback (`every`), and
prints per callback the gap between `targetTimestamp`s, the link's `duration` (what `vsync.rs`
takes as the nominal period) and `timestamp`.

Measured 2026-10-14 on the dev Mac: MacBookPro18,4 (M1 Max), macOS 26.6.2, built-in Liquid
Retina XDR set to ProMotion (`1512 x 982 @ 120.00Hz`, `minimumRefreshInterval` 8.33 ms,
`maximumRefreshInterval` 41.7 ms), BenQ 2560x1440 @ 60 Hz attached as the main display, on AC,
Low Power Mode off.

## Results

Gaps between `targetTimestamp`s, by count (`data/`):

| run | 16.7 ms | 8.3 ms | other (10–40 ms) | `duration` |
|---|---|---|---|---|
| window, idle (`window-idle.txt`) | 233/240 | 2 | 5 | 8.333 |
| window, 120 asked, drawing every callback (`window-120-drawing.txt`) | 224/240 | 3 | 13 | 8.333 |
| screen link, same (`screenlink-120-drawing.txt`) | 227/240 | 3 | 10 | 8.333 |
| native fullscreen, same (`fullscreen-120-drawing.txt`) | 350/360 | 3 | 7 | 8.333 |

A least-squares grid through each run's `timestamp`s (period at the median gap): the drawing
window sat on 16.6666 ms with |residual| p90 0.14 ms; the idle window and the fullscreen run
wandered, p50 1.5–1.8 ms, p90 3.9–4.6 ms, max 10–12 ms. A CVDisplayLink on the same screen
delivered no callbacks at all in 10 s.

## Conclusion

- **The link runs at 60 Hz while `duration` says 120.** Asking for 120, drawing every frame and
  going fullscreen change nothing, on the window's link or the screen's. `read_burst` drops any
  burst whose fitted period is more than 1% off `duration`, so on this panel it drops them all:
  that is the missing grid. The guest's own presents agree: on this panel they went up at 60.0
  fps, every interval 16.7 ms.
- **The 60 Hz is not a fixed grid.** The phase moved by milliseconds within a 4–6 s run in two
  runs of three: the panel refreshes when content arrives, inside its 8.3–41.7 ms window. A
  phase-locked guest vblank, which is what the grid exists for, has nothing stable to lock to.
- Why apps get 60 here is not established: an external display attached as main, or a policy
  for unbundled processes, are both untested. Re-measure without the BenQ, and from inside the
  signed app bundle, before building on 60 as the panel's rate.
