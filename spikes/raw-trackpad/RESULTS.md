# Raw trackpad mode: telling trackpad pointer events and physical clicks apart

`clickprobe.swift` is a listen-only session `CGEventTap` logging pointer, button, scroll and
gesture events with their mouse subtype and pressure fields and the local touch count of each
gesture event. Measured 2026-09-30, M1 Max built-in trackpad, macOS 26.6 (tap-to-click on):

| event | `kCGMouseEventSubtype` | time from the nearest touch-carrying gesture event |
|---|---|---|
| trackpad pointer motion, drags | 3 (touch) | — |
| one-finger physical click | 3 | 0–13 ms |
| two-finger physical click (arrives as right button) | 3 | 10–14 ms |
| one-finger tap-to-click | 3 | 235–246 ms after the lift |
| two-finger tap-to-click | 3 | 310–320 ms after the lift |
| Universal Control mouse: motion, clicks | 0 | — |

- The recordings in `crates/limina/testdata/trackpad/` agree: of 178 clicks, every tap click
  came 222–341 ms after the last touch and every physical click 0–28 ms after one (one
  outlier at 54 ms, held 7 ms, cause unknown). A 100 ms cut separates them.
- No `NSEventTypePressure` (34) event reaches a session tap; pressure is 1.0 on every press,
  tap or physical, so neither tells them apart.
- Tap clicks arrive as a down and an up within ~0–20 ms; physical ones are held.
- During a one-finger click-drag the touch-carrying gesture events keep coming, at most
  ~47 ms apart.
- **macOS's tap-to-drag press comes with the finger down.** After a tap, landing again
  within ~300 ms makes macOS press 7–18 ms after that landing (raw-mode poke, 7 drags;
  29–32 ms for 3 in the recordings) and hold until the lift; the touch before it was a tap of
  35–83 ms. Physical presses came 60 ms or more after a landing, except one two-finger click
  at 6.5 ms with no tap in the 7 s before it; rapid physical clicks land 66–267 ms after the
  previous lift, but the touch before them pressed. `--fields` dumps every set CG field of
  each button event: none marks a synthesized press (fields 89/90 appear on tap-drag presses
  and on two-finger physical clicks alike).
