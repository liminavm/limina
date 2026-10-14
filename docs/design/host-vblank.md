# Host-driven vblank: the guest's frame clock on the Mac's grid

**Status: agreed; being built.**

Enhanced tier only (the `virtio-gpu-limina` module with the limina extension). A stock guest keeps
the in-tree driver's free-running vblank timer, which `refresh_millihertz` (the exact-rate EDID)
already runs at the host panel's rate; only its phase stays arbitrary.

## The problem

A guest compositor schedules from vblank: it aims to commit before the next one and treats the
flip-done timestamp as when the frame reached the screen. The guest's vblank comes from a timer
of its own, so even at the host panel's exact rate its phase against the Mac's vsync is whatever
the CRTC came up with. Measured on F44 Venus (90 s timing capture, 2026-10-14): with the rates
59.995 vs 59.950 Hz the phase slid a full refresh every ~22 s, and frame pacing swung between
bursts and dips on that period. Rate matching removes the slide; it cannot place the phase.

What the compositor actually needs from a vblank is a **deadline**: the last moment a commit
still makes a given Mac vsync. That is the Mac's vsync minus the time a frame takes to get from
the guest's flush to the window server's composite — the **lead**.

## Decisions (agreed)

1. **Timestamps are deadline times.** A vblank, and every flip-done event, is stamped at
   `mac_vsync - lead`, not at the moment of glass. Compositors then schedule correctly with no
   knowledge of limina; presentation feedback (`wp_presentation`, A/V sync) reads `lead` early,
   a few ms, which is the accepted cost.
2. **A shared timing page, not a forwarded interrupt per vsync.** The host publishes the grid;
   the guest programs its own timer from it. No VM exit per frame, no delivery jitter.
3. **The lead is measured, not a constant.** Copy-on-present (next step) will add its blit to the
   path; a measured lead stays right across that and across host load.

## Why the guest can be told an exact instant

On HVF the guest's virtual counter is `mach_absolute_time() - vtimer_offset`
(`third_party/libkrun/src/hvf/src/lib.rs:1797`), at the same 24 MHz on both sides. The offset is
the worker's own: 0 at boot, advanced by the same paused ticks on every vCPU across a pause
(`vstate.rs:1011`), and written from the snapshot on restore (`hvf/src/lib.rs:1377`). So a Mac
vsync time from CoreVideo, which is in `mach_absolute_time` ticks, minus the offset, *is* a guest
counter value: no clock estimation, no PLL, on either side.

The offset lives in per-vCPU HVF state, which only that vCPU's thread can read, so the vCPU
threads publish it (an atomic beside the pause/restore code) for the GPU device to use.

## The pieces

### Host: vsync source (supervisor)

The supervisor is the guest's display engine — it puts every frame on glass — and knows which
screen each guest display's window is on, so it owns the vsync source: that screen's display link
(`NSScreen.displayLink`, macOS 14+; `CVDisplayLink` is deprecated). Its timestamps are
`CACurrentMediaTime` seconds, i.e. `mach_absolute_time` scaled, and convert back to ticks without
loss that matters here. From them it keeps, per guest display, an **anchor** (one vsync's time, in
`mach_absolute_time` ticks) and the **actual period** (measured over the link's callbacks, not the
nominal rate). VRR / ProMotion's adaptive cadence is out of scope: the grid is the screen's fixed
nominal one. It sends them, with the lead, to the worker on the
display-control socket, `vsync id=<display> anchor=<mach> period=<ticks·2^-32> lead=<ns>`, only when
they change: a new screen, a rate change, the predicted grid drifting more than 50 µs from the
observed one, or a new lead estimate (below). Between updates nothing flows — except that it
also sends its current grid whenever it attaches to a worker (a new, relaunched or restored one),
which otherwise would have none until something changed.

When the link stops (window occluded, screen asleep) the last grid stands and the guest free-runs
on it, which is exactly what its timer would have done anyway.

### Host: lead estimator (supervisor)

The lead must not be estimated from how early frames arrive before the vsync they make. Once the
guest is locked it commits at `published_deadline - render_estimate`, so every frame's
`vsync - flush` is the published lead plus the compositor's own margin: a statistic of it feeds
back into itself, ratchets the lead up until frames land a vsync early, and then oscillates by a
refresh. The lead is built from the two parts the guest cannot influence instead:

- **host processing** `h`: from the worker receiving the guest's flush (`mach_absolute_time`,
  stamped on the frame message it already sends) to the supervisor committing that frame to its
  layer. Both stamps are the host's; their p95 over the last ~2 s. Copy-on-present's blit lands
  here, where it belongs.
- **composite deadline** `w`: how long before a vsync a layer commit must land to be shown at it,
  a property of WindowServer, not of the guest. Measured once per macOS version by a spike that
  commits surfaces at swept phases against the display link and records which vsync each goes
  live at (the replaced surface leaving window-server use); kept as a constant, with a runtime
  guard: a frame whose commit was earlier than `vsync - w` yet missed that vsync widens `w` by
  0.5 ms.

**lead = h + w + 1 ms margin.** A new value is published only when it moves by more than 0.5 ms
(hysteresis). Before the first `h` a default of 4 ms stands.

### Worker: the timing page

A new shared-memory region of the extension, **shm id 0x82**, one 4 KiB page, read-only to the
guest. One 64-byte entry per scanout, each under a seqlock:

| field | |
|---|---|
| `seq` (u32) | odd while the host writes the entry |
| `flags` (u32) | `VALID`; clear = no grid, use the stock timer |
| `anchor` (u64) | guest counter value of one **deadline** vblank (`mac_anchor - vtimer_offset - lead`) |
| `period` (u64) | counter ticks per frame, 32.32 fixed point |
| `lead_ns` (u32) | the lead folded into `anchor`, for diagnostics |
| `generation` (u32) | bumped on every write, for tracing |

The worker translates each `vsync` line (minus its current `vtimer_offset`) and rewrites the
entry, and rewrites every entry after a pause or a restore moves the offset. A snapshot carries
nothing: the page is derived, and the restored worker rewrites it from the next `vsync` line.

### Guest: the module's vblank

When the host offers the region and a scanout's entry is `VALID`, that CRTC's vblank comes from
the module instead of `DRM_CRTC_VBLANK_TIMER_FUNCS`:

- an hrtimer per CRTC; each expiry reads the entry, computes the next tick as
  `anchor + k·period` in counter time, converts it to `ktime` against a fresh
  `(arch_timer_read_counter(), ktime_get())` pair, rearms, and calls `drm_crtc_handle_vblank`.
  Every tick is recomputed from the anchor, so there is no accumulated drift and a new anchor
  takes effect at the next tick;
- ticks stay monotonic and one per period: a new anchor never arms a tick earlier than
  `last_tick + period/2`, and a phase correction larger than that is slewed over several ticks
  rather than jumped (mutter and wlroots take a backwards timestamp as given; KWin replaces it
  with "now");
- `get_vblank_timestamp` returns the tick's own time, so `drm_crtc_vblank_count_and_time` — and
  therefore every flip-done event — carries the deadline time;
- an entry that goes invalid hands the CRTC back to the stock timer at the next tick.

The hook patch gains one hunk: the CRTC funcs route `enable_vblank` / `disable_vblank` /
`get_vblank_timestamp` through `virtgpu_limina.c`, which falls back to the stock helpers. Anchored
on code that is the same in 6.19, 7.1 and 7.2.

## What changes when copy-on-present lands

With the frame copied at latch the host releases the guest's buffer within ~1 ms of the flush,
before the deadline vblank the frame was aimed at; the flip-done event then fires *at* that
vblank, as on hardware, with no "complete at release" needed. That switch remains useful only
for the zero-copy fenced path, where the release can come after the tick.

## Validation (frame pacing, not fps)

- The timing capture records gain the worker's flush time `f` (in guest counter time as well as
  host). Phase lock means `f mod period`, against the Mac grid, stays in a narrow band for minutes;
  a free-running timer's sits wherever the CRTC came up and drifts with any rate error left.
- The module exposes its last tick (counter value, generation) in debugfs; the L2 check compares
  ticks against the published grid (within 50 µs) and against the Mac vsync (`lead` apart).
- A guest without the module, or a host that does not offer 0x82, must keep running on the stock
  timer: covered by the existing stock-tier suite.
