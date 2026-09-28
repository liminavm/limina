# Basemark's scored run stalls: the harness's Esc cancels the page's navigation

**Question.** In about one perf-pass boot in five (17 of 93 recorded Basemark runs), the scored
suite never reaches its result page. Every one of those stalls is in run 2, and in 16 of 17 the
page read at the start of the run was `shader_pipeline_test`. Is the host renderer, or the guest
driver, stopping Firefox?

**Answer: no. The harness does it.** `client-basemark.sh` (virglrs `harness/vm`, frozen at
`d0416c9` for the perf passes) taps Esc in the guest about 12-15 s after launching each suite, to
leave the GNOME overview. Esc in Firefox is Stop. When the tap lands during the page's own
navigation from one test to the next, Firefox cancels that navigation. Basemark's engine has no
error path and no retry, so the page waits forever:

- `BasemarkWebEngine.nextPage` GETs `/api/tests/next`, then navigates with a bare
  `window.location = ...` inside a 300 ms timer.
- Its `ajax` helper calls back only on `readyState 4 && status 200`.

## Why it only ever hits run 2 at Shader Pipeline

Run 1 is cold, so when Esc lands 12-15 s in, the page is still inside the WebGL tests. From run 2
on, the WebGL tests are warm and finish in about that time. So Esc lands near the Shader Pipeline →
Draw-call hand-off:

- **Healthy later runs:** the page has already reached draw-call when Esc arrives.
- **Stalled runs:** Esc arrives during the hand-off, and the tab stays on the finished Shader
  Pipeline page.

## Evidence (caught live, measured 2026-09-28, `runs/20260928-182427-boot1/`)

Run 3 of one boot stalled on `shader_pipeline_test`.

- **The host owed the guest nothing.** The guest's `virtio-gpu-irq-fence` read `67673 67673` at
  the stall and `67675 67675` a minute later: signalled equals emitted, and the desktop kept
  rendering. No Firefox thread was in a GPU wait; the content process sat in `poll`.
- **The browser was healthy.** In the stalled tab, `requestAnimationFrame` fired 205 times in 3 s,
  timers fired, and the page was visible and focused. No WebGL context was lost.
- **The test had finished.** Its page listed all seven of its sub-tests (CelShading through
  Fresnel). Both of the engine's end-of-test calls returned 200 about 3 s into the page:
  `/api/sessions/test` and `/api/tests/next`. Asking `next` again returned
  `{"next": "/run/tests/30/graphics_suite/draw-call_stress_test/"}`.
- **The timing matches.** The `next` request's cache-buster dates it to 1790631199.6. With the
  reply ~0.23 s later and the 300 ms timer, the navigation fired around 1790631200.1. The client
  logged `tapped esc` at 1790631201.36, and the tap is sent just before that line prints (the
  helper takes ~1-3 s to create its uinput device).
- **Direct proof** (`esc-proof.py`, in the same Firefox): a navigation to a URL that takes 8 s to
  answer.
  - Without a tap, the tab landed on the new URL.
  - With `tap-keys.py esc` 0.5 s after the navigation started, the tab was still on the old URL
    12 s later, and nothing was logged.

## The fix

virglrs `c82e6c2` fixes the harness. It leaves the overview once, while the only page is the static
probe, and only if GNOME Shell reports `OverviewActive`; the property is read again afterwards to
confirm. Verified on the stock guest (measured 2026-09-28): the overview was left, both suites
scored, and run 2 passed through Shader Pipeline. A perf pass on an older harness can still hit the
stall, and there it is this race, not a renderer regression.

**Draw-call Stress may have been under-scored too.** That validation run scored it 171, against
79-81 in every scored run of the 09-26..09-28 passes. In those passes' healthy run 2s the old tap
landed during Draw-call Stress itself ("now at: draw-call_stress_test"). This is one sample, so
Draw-call rows taken with the old harness should be read as suspect until a pass on `c82e6c2`
confirms or refutes it.

## The vehicle

- `loop.sh`: boots the stock guest as the perf passes do and runs `client-stall.sh` (RUNS suites in
  one Firefox session). When the client announces a stall, it samples our worker twice and keeps
  the window capture and the worker log.
- `client-stall.sh`: the perf passes' client with two changes. It runs several suites, and it
  polls the page with `marionette.py url`, which the parent process answers. When one test page
  holds for `STALL_S` it runs `dump-guest.sh`.
- `dump-guest.sh`: Firefox's threads and kernel stacks, the virtio-gpu fence ledger and clients,
  dma-buf fences, top, sockets, dmesg, and the page.
