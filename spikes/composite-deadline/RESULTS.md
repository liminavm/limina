# composite-deadline — how early must a layer commit land to be shown at a vsync?

The host-driven vblank's lead (`docs/design/host-vblank.md`, §lead estimator) is `h + w + margin`,
where `w` is WindowServer's composite deadline: how long before a vsync a layer-contents commit must
land to go live at it. This measures `w`.

## Vehicle

`deadline.swift`: one opaque `CALayer` in an `NSWindow`, contents swapped between two IOSurfaces
exactly as the supervisor does (`present.rs::set_layer_surface`: `CATransaction`, actions
disabled, implicit flush by `commit`). Each round waits for a `NSWindow.displayLink` callback,
spins until `targetTimestamp - d`, commits, then tight-polls (50 µs) the *replaced* surface until
WindowServer stops using it. That release time `u` says which vsync the commit made: it lands
~5 ms after the target vsync when the commit made it, and one refresh later (~21–23 ms) when it
missed. The two are never confused; `fixed 12 60` validates that first (`data/fixed12.txt`: 59/60
between 3.7 and 8.0 ms, one at −1.1, none near +P).

`targetTimestamp` is the same reference the supervisor's grid is built from
(`crates/limina/src/window/vsync.rs`), so `d` is measured against the grid the guest is put on.
Phases are shuffled per pass; 250 ms settle between rounds. `analyse.py <file>` tabulates
made/total per phase.

Measured 2026-10-14 on the dev Mac: M1 Max, macOS 26.6.2 (25G83), main display BenQ 2560×1440 at
59.950 Hz, compositor otherwise quiet (the F44 Venus guest idle).

## Results

`d` > 0 is a commit before the target vsync, < 0 after it.

**Windowed** (`data/sweep-win.txt`, `sweep-win-neg.txt`, `fine-win.txt`):

| d (ms) | made |
|---|---|
| 0.5 … 15.5 (by 0.5) | 248/248 |
| 0.2 … 0.8 (by 0.1) | 67/70 |
| −0.3 … 0.1 | 42/50 |
| −0.4, −0.5 | 5/20 |
| −0.6 … −0.8 | 3/30 |
| −0.5 … −12 (by 0.5; the coarse run, `sweep-win-neg.txt`) | 0/192 |

**Native fullscreen** (`data/fs.txt`, `fs-wide.txt`):

| d (ms) | made |
|---|---|
| 1.0 … 15.0 (by 1.0) | 90/90 |
| 1.75 … 3.0 (by 0.25) | 35/36 (one stray miss at 3.0) |
| 0.75 … 1.5 | 20/24 |
| 0.5 | 1/6 |
| −3.0 … 0.25 | 0/84 |

## Conclusion

- **The deadline is at the display link's target, not ahead of it.** A windowed commit made up
  to ~0.3 ms *after* `targetTimestamp` still goes live at it; the edge is ~0.4 ms wide.
- **Native fullscreen wants ~0.8 ms more**: its edge sits ~0.7 ms *before* the target, and it is
  reliable from ~1.8 ms. Fullscreen takes a different path through WindowServer
  (likely direct scanout of the one opaque layer), with its own deadline.
- So, for a commit to make the vsync reliably: **w = 1 ms windowed, 2 ms fullscreen**, each
  about 1 ms clear of the made/missed edge. These are the constants `vsync.rs` uses.
- Stray misses exist well clear of the edge (one in ~150 fullscreen rounds at 3 ms). That is
  what the design's runtime guard is for; a constant cannot absorb it.

Re-measure on a new macOS release, a ProMotion panel, or a VRR display — none of those were in
this run.
