# Host-driven vblank: the guest's frame clock on the Mac's grid

**Status: built, except the composite deadline's runtime guard**: `w` is a measured constant
(`crates/limina/src/window/vsync.rs`).

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
3. **The lead is measured, not a constant.** It stays right across host load and across what the
   present path costs, the copy's blit included.

## Why the guest can be told an exact instant

On HVF the guest's virtual counter is `mach_absolute_time() - vtimer_offset`
(`third_party/libkrun/src/hvf/src/lib.rs:1797`), at the same 24 MHz on both sides. The offset is
the worker's own: 0 at boot, advanced by the same paused ticks on every vCPU across a pause
(`vstate.rs:1011`), and written from the snapshot on restore (`hvf/src/lib.rs:1377`). So a Mac
vsync time from CoreVideo, which is in `mach_absolute_time` ticks, minus the offset, *is* a guest
counter value: no clock estimation, no PLL, on either side.

The offset lives in per-vCPU HVF state, which only that vCPU's thread can read, so the `Vmm` keeps
its own record of it: the one place a pause advances it (`Vmm::resume`) and the restore that sets
it both go through `Vmm::note_vtimer_offset`, which pushes it to the timing page — on resume
*before* the vCPUs run, so the guest never sees an anchor stated in the old counter time.

## The pieces

### Host: vsync source (supervisor)

The supervisor is the guest's display engine — it puts every frame on glass — and knows which
screen each guest display's window is on, so it owns the vsync source: each guest window's display
link (`NSWindow.displayLink`, macOS 14+, which follows the window from screen to screen;
`CVDisplayLink` is deprecated). Its `targetTimestamp`s are `CACurrentMediaTime` seconds, i.e.
`mach_absolute_time` scaled, and convert back to ticks without loss that matters here.

The link runs in **bursts** (up to 60 vsyncs or 2 s), at 0, 3 and 12 s after the window lands on a
screen and every 30 s after, not every frame: a fixed-rate screen's grid is a phase and a period,
and a long baseline between bursts pins the period down far better than continuous watching.
Each burst gives a least-squares period and the vsync nearest its centroid; later bursts refine the
period over the span from the first, counting the frames between with the period known so far.
That count is only safe while the baseline grows by at most 16× a step, which the schedule keeps;
a count that leaves more than a quarter period unexplained starts over (another screen, a rate
change). VRR / ProMotion's adaptive cadence is out of scope: the grid is the screen's fixed one.

It sends the grid, with the lead, on the display-control socket — `vsync id=<slot> at=<mach>
period=<ticks·2^-32> lead=<ticks> leadns=<ns>` — only when the worker needs it: a new slot or lead,
or the worker's grid being 50 µs off the measured one now or within a minute. A fresh worker (a
resume, a relaunch) is told again. When the link stops (window occluded, screen asleep) a burst
ends with what it has, and the last grid stands. `LIMINA_VSYNC_SOURCE=0` keeps the source quiet,
for runs that state a grid of their own. A window never sends `off`: a slot it leaves may already have
another window's grid on it, and an `off` racing that would clear it. A stale grid stands until
the next window's first burst, about a second.

### Host: lead estimator (supervisor)

The lead must not be estimated from how early frames arrive before the vsync they make. Once the
guest is locked it commits at `published_deadline - render_estimate`, so every frame's
`vsync - flush` is the published lead plus the compositor's own margin: a statistic of it feeds
back into itself, ratchets the lead up until frames land a vsync early, and then oscillates by a
refresh. The lead is built from the two parts the guest cannot influence instead:

- **host processing** `h`: from the worker receiving the guest's flush (`mach_absolute_time`,
  stamped as `t=<mach>` on the `frame` line when the present reaches its display backend) to the
  supervisor starting that frame's layer commit — the commit call itself belongs to `w`, which
  the spike measured from the same point. Both stamps are the host's. The present copy's blit
  (below) lands here, where it belongs. Measured with the aquarium (F44, 30 presented fps): the worker →
  reader leg is p50 0.07 ms; arrival → commit p50 1.6, p95 3.4, p99 6.9, max 18 ms.
- **composite deadline** `w`: how long before a vsync a layer commit must land to be shown at it,
  a property of WindowServer, not of the guest. Measured once per macOS version by
  `spikes/composite-deadline`, which commits surfaces at swept phases against the display link
  and reads which vsync each goes live at from when the replaced surface leaves window-server use.
  On macOS 26.6 a window's commits make the vsync up to ~0.3 ms *after* the link's target, and a
  window covering its screen (natively fullscreen; the borderless cover is assumed the same) needs
  ~0.7 ms before it: **`w` = 1 ms, 2 ms covering**, each ~1 ms clear of the edge. Owed: the
  runtime guard — a frame whose commit was earlier than `vsync - w` yet missed that vsync widens
  `w` by 0.5 ms — for the stray misses a constant cannot absorb.

**lead = h + w + 1 ms margin**, `h` the p95 of a **10 s tumbling window**, taken once as each
window ends. A p95 re-read continuously over a couple of seconds is the third-largest of sixty
frames and moves with every stall; that restated the lead ~8 times a second, and every restatement
moves the guest's vblank. A longer lead is published once it exceeds the last by 0.5 ms, a shorter
one only once it is 1.5 ms shorter (a missed vsync costs a refresh, too long a lead only latency).
The 4 ms default stands until a window holds 50 frames and then gives way either way; an idle
guest keeps the last value; a lead past 33 ms is capped — that is a stalled host, not a deadline.

### Worker: the timing page

A new shared-memory region of the extension, **shm id 0x82**: one 16 KiB host page (what
`hv_vm_map` maps), at the arena window's first slot; arenas are placed after it. It is mapped
read-write like the arenas; the host owns it and rewrites entries whole, so a guest writing to it
only disturbs its own vblank. One 64-byte entry per scanout, each under a seqlock:

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

When the host offers the region and a scanout's entry is `VALID`, that CRTC's vblank ticks on the
grid. The stock vblank timer (`DRM_CRTC_VBLANK_TIMER_FUNCS`) still runs it: the hook patch adds
one line, `handle_vblank_timeout` in the CRTC helper funcs, the same on 6.19, 7.1 and 7.2, and
`virtgpu_limina.c` re-aims every expiry before it hands the vblank to the core:

- the next tick is the first grid point (`anchor + k·period`, in counter time) past half a period
  from now. Ticks therefore stay between half a period and one and a half apart: a new phase
  neither doubles nor drops a vblank, the vblank core's timestamp-based count never sees a
  redundant interrupt, and a stamp is never more than half a period ahead. Every tick is
  recomputed from the anchor, so nothing drifts, and a stale anchor (hours old) costs nothing —
  `k` comes from 128-bit arithmetic;
- the timer's expiry is set to that tick and its interval to the period, so the core's stamp
  (`expiry - interval`) is the grid point just reached: the deadline time, carried by every
  flip-done event;
- counter ticks become `CLOCK_MONOTONIC` nanoseconds at the rate the two clocks moved since the
  previous expiry, not the counter's nominal rate. hrtimers run on `CLOCK_MONOTONIC`, which NTP
  slews against the counter: chrony slews at up to 1/12 while it pulls a freshly booted guest's
  clock in, and a nominal conversion then fires every tick ~1.4 ms off the grid. Measured rates
  more than an eighth from nominal are not trusted;
- an entry that is not `VALID` (or a page that is not, or not yet, one the module understands)
  leaves the expiry the stock handler set: the stock cadence, at the mode's rate.

## The flip completes when the host releases the buffer

A fenced flush's flip-done event cannot fire before the host releases the guest's buffer, and the
release necessarily follows the tick the frame was aimed at: the frame was flushed a lead ahead
of the vsync, and the host takes it after. Arming the event at the end of the commit therefore
cost a refresh on every frame. The module's rule instead: **if a vblank tick passed between the
commit's `atomic_begin` and `atomic_flush`, send the event now** (the core stamps it with that
tick); otherwise arm it for the next one. Debugfs counts both (`flips N at_release= at_tick=`).

What the release waits for decides which branch wins. The Metal transport's scanouts are shown as
copies (`try_park_present` in libkrun's `virtio_gpu.rs`, the blit in
`crates/limina-vmm/src/scanout_copier.rs`): a copied frame's hold confirms when the blit retires,
~1 ms after the flush (`spikes/transport-copy`), so most flips complete at the tick. Showing the
guest's own buffer instead (`LIMINA_TRANSPORT_COPY=0`) holds the guest until the replaced
surface leaves WindowServer, a refresh after latch, and caps a 60 Hz guest near 39 fps.

## Validation (frame pacing, not fps)

- The timing capture records gain the worker's flush time `f` (in guest counter time as well as
  host). Phase lock means `f mod period`, against the Mac grid, stays in a narrow band for minutes;
  a free-running timer's sits wherever the CRTC came up and drifts with any rate error left.
- `dri/<n>/limina_timing` (debugfs) shows the page, each CRTC's timer, its last ticks and a
  (counter, `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`) triple. `l2_host_vblank` pushes a grid with
  an odd period and checks the vblank stamps userspace gets from `DRM_IOCTL_WAIT_VBLANK`, moved
  onto the counter through `CLOCK_MONOTONIC_RAW` (which NTP does not slew), against it: within
  50 µs (measured: within 5 µs), across a phase change, and back on the stock timer after `off`.
- A guest without the module, or a host that does not offer 0x82, must keep running on the stock
  timer: covered by the existing stock-tier suite.
- Measured 2026-10-14, F44 Venus, WebGL aquarium, BenQ at 59.95 Hz, copies on: 58.7 presented
  fps over 30 s (1741 of 1779 intervals one refresh, 37 two, none three), the guest flushing
  59.4/s, flips completing at the tick ~3.5x as often as at release.
- A window on a variable-refresh screen (ProMotion) states no grid: its slot's entry goes off and
  the guest keeps the stock timer. There, the display link runs at ~60 Hz against a 120 Hz
  `duration`, and its phase wanders by milliseconds (`spikes/promotion-link`). VRR support is owed
  (`docs/hardening-backlog.md`).
