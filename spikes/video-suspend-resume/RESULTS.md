# Hardware-decoded video does not resume after suspend/restore

Measured 2026-08-30 on the dev Mac (M1 Max, macOS 26.5), Debian testing guest (stock tier,
`Debian-testing.luks.raw`), Chrome playing YouTube, snapshot suspend + restore through the
managed-VM path. Reproduced on the first attempt with tracing on.

Vehicle: `limina start Debian` with
`RUST_LOG=warn,limina=info,krun_vmm=info,krun_devices=info`, `LIMINA_GPU_TRACE=1`,
`LIMINA_WINDOW_CAPTURE` (+ a 3 s snapshot ring, since the capture file overwrites itself).
Log: `trace-excerpt.log` in this directory (the worker log, filtered to the GPU trace, restore and capture lines).

## The finding

**The guest keeps submitting at full rate, the host accepts everything, and the picture stops
advancing.** Those three facts together are the result; each alone is unremarkable.

Restore itself reports success:

```
gpu restore: classic content restored (7 contexts, 0 failed), 0 scanout flips
gpu restore: replay complete (session state re-created)
virtio_gpu: released DRIVER_OK after 1.267008334s (replay complete)
```

Every post-resume GPU trace tick is clean — this is the whole point:

```
[GPUTRACE] tick=3  submits=+389 unknown_ctx=+0 unknown_res=+0 errs=+0 fences_req=+359 fences_ret=+359 outstanding=0
[GPUTRACE] tick=8  submits=+290 unknown_ctx=+0 unknown_res=+0 errs=+0 fences_req=+246 fences_ret=+246 outstanding=0
[GPUTRACE] tick=10 submits=+288 unknown_ctx=+0 unknown_res=+0 errs=+0 fences_req=+240 fences_ret=+240 outstanding=0
```

~145 submits/second, zero rejections, every fence retired, nothing outstanding.

Meanwhile the video region of the captured scanout (mean |Δ| per channel between captures 3 s
apart, Chrome's player area):

| | before suspend | after resume |
| --- | --- | --- |
| consecutive frames | 92.0, 19.4, 24.0, 20.0 | 2.9, 3.7, 4.0, 4.0, 4.6 |

An order of magnitude down, and the residual 3–5 is the size of the progress bar and cursor
moving, not of a frame of video. (The 0.000 at the end of the pre-suspend series is the user
pausing before the suspend — a useful negative control: a genuinely still picture reads as
exactly zero, so the post-resume 3–5 really is *something* small changing.)

Whole-frame analysis over the same window shows the screen alternating between a small set of
near-identical images — captures 9 s apart matching each other to 0.6 while adjacent ones
differ by 1.9 — which is the reported "same few frames back and forth".

## What this rules out

**The journal gap is not the mechanism.** `vrend_journal.h:33` classifies video codec objects
as `VREND_JOURNAL_UNKNOWN` ("counted, not kept"), so the host's codec object is genuinely not
recreated at restore, and that made an attractive theory: guest keeps its handle, submits into
a codec that no longer exists. **The counters kill it.** Rejected commands are counted
unconditionally and each one warns (`gpu/trace.rs:14-18`), and the first error of any class
requests a renderer state dump. Post-resume there are zero `unknown_ctx`, zero `unknown_res`,
zero `errs`. Nothing is being rejected.

The failure is therefore in the *silent* class this stack keeps producing: commands accepted,
work done, and the picture never updated — a stale surface rather than a failed command.
Compare the AV1 note (a late picture is a silent stale surface, never a hang) and the
scanout/IOSurface staleness already on record.

## The guest's own screencast: the picture ping-pongs over a bounded pool

`guest-screencast.mp4` was recorded **inside the guest** (GNOME screencast, so it captures
mutter's own composition) while the symptom was live: 147 frames, 6.08 s, Chrome playing
`planet_earth.4k.vp9.webm`. Classifying the video region of every frame into distinct pictures
(64×32 luma signature, threshold 1.0):

- **35 distinct pictures in 6.08 s.** At the clip's frame rate there should be ~150–180. The
  player is cycling a small pool, not advancing through the stream.
- **The traversal is bidirectional: 17 ascending runs and 15 descending runs**, e.g.
  `0 1 2 … 13` then `1 0`, then `14 15 16 17 18`, then `7 6 5 4 3`, then `0 1`, then
  `13 12 11 10 9 8`. That is the reported "bouncing" precisely, and it is not a metaphor —
  the same decoded pictures are re-presented in reverse.
- **New pictures do keep arriving, just far too slowly.** First appearances cluster at frames
  0–22 (the initial fill), then only 38, 40, 42, 67, 72, 114–116 — and then a clean 11-frame
  ascending run at 138–145 with a new picture nearly every frame, which is what recovery looks
  like.

**This exonerates our host-side presentation.** The recording is made by the guest compositor,
upstream of our scanout, IOSurface import and window present — and the bouncing is already
present there. Whatever reorders these pictures does so at or before mutter's composition, so
the fault is in the guest: the decoder's output ordering, or the surfaces the player hands to
the compositor. Combined with the host counters (zero rejections, full submit rate), the host
is doing what it is asked; it is being asked for the wrong pictures.

## Where to look next

The question is now narrow: **why does a bounded set of already-decoded pictures get
re-presented in alternating order after a restore?** A ping-pong over a fixed pool is the
signature of an index or ordering key that has stopped advancing monotonically — a
presentation timestamp, a picture-order count, or a surface ring index — rather than of a lost
object or a failed allocation.

Ordered by cost:

1. **Guest-side first, because the guest is where the fault is.** Chrome's
   `chrome://media-internals` across a restore gives the decoder's own view: frames decoded,
   frames dropped, and the timestamps it is presenting. If Chrome believes it is presenting
   monotonically increasing timestamps while the picture bounces, the fault is below it (VA
   surface reuse); if its own timestamps bounce, it is above.
2. ~~The guest clock~~ — **measured and excluded, see below.**
3. The VA surface pool: whether the decoder is writing new content into surfaces the player is
   not picking up, which the trickle of new pictures (frames 38–116) hints at.

### The guest clock is NOT the cause (measured)

A 50 Hz sampler (`clocks.log`, `time.clock_gettime` on all four clocks) ran inside the guest
across a real managed suspend/restore — 181.07 s of wall time, snapshot 1.99 GB:

| clock | delta across the boundary |
| --- | --- |
| `CLOCK_REALTIME` | +181.072927 s |
| `CLOCK_BOOTTIME` | +181.073044 s |
| `CLOCK_MONOTONIC` | +0.939309 s |
| `CLOCK_MONOTONIC_RAW` | +0.939337 s |

**Zero backward steps in any clock over the whole 1203-sample trace.** This is exactly correct
behaviour — monotonic excludes suspended time by definition, realtime and boottime include it —
so the timekeeping the player would key off is sound, and the attractive
"jumped/reversed clock ⇒ frames chosen out of order" story is dead. It was worth an hour: it is
the cheapest theory that would have explained both the bouncing and the heal-on-pause.

That is two theories killed by measurement (the journal gap, then the clock), which is the
point of this file — neither would have announced itself as wrong during a fix.

### The A/B that should come next

**Is the bounce even specific to hardware decode?** Nothing measured so far proves it is. Run
the identical suspend/restore cycle with Chrome's VA-API decoder off
(`--disable-features=VaapiVideoDecoder`, or the equivalent `chrome://flags` toggle) and the same
clip:

- **Still bounces** ⇒ this is not the decode path at all, and the whole video framing is
  misleading; look at the player's frame scheduling or the compositor across restore.
- **Does not bounce** ⇒ the fault really is in the hardware path, and the VA surface pool is
  the next thing to instrument.

Do this before instrumenting anything. It is one boot and one clip, and it partitions the
search space in half — which is more than either of the two theories above achieved.

Note what would have been the wrong first move: instrumenting our decode backend. The host
counters and the guest's own recording both say the host is serving what it is asked for.
