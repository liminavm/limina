# `l2_desktop_restore_landmarks` blanks on the monitor-identity push

`seated_gpu_workload_survives_restore_unchanged` fails its pre-suspend content floor with
`1 distinct colours`. That failure has twice been read as a desktop that never seated, and it is
not one: **the desktop seats and paints, the monitor-identity push blanks it, and the host is
never asked to paint it again.**

## The finding

Measured in one boot, with the render-target census taken on both sides of the push and every
route a scanout can be written through traced:

    BEFORE   ctx 2   resources 5, 40, 102    2560x1440, bind 0x14000a   (SCANOUT)
             ctx 9   1880x1200               ctx 13  1280x1376          ctx 4  48x48
    AFTER    ctx 9   940x600, bind 0x10000a
             ctx 13  640x688, bind 0x10000a
             scanout writes: 0

Context 2 is the compositor: the only one attaching a full-size render target, and the only one
whose bind carries `SCANOUT` on top of the clients' `0x10000a`. Before the push it draws straight
into the scanout resources. After it, **context 2 does not appear at all**, and no write of any
kind reaches a scanout — not a draw, a blit, a resource copy, a surface clear, a stream transfer,
or a transfer the VMM names. The clients (nautilus, firefox) keep rendering throughout.

The guest is meanwhile flipping:

    framebuffer[49]:  allocated by = gnome-shell, XR24, 2560x1440
    plane[38]: crtc=crtc-0  fb=48  allocated by = gnome-shell, 2560x1440
    mutter GetCurrentState: 2560x1440@59.994 is-current, logical monitor scale 1.0

So the mode set did exactly what the fabricated EDID intends — the guest settles on scale 1.0 —
and gnome-shell allocates fresh 2560x1440 framebuffers and page-flips them. It just never paints
them. The host mints an IOSurface per flip, is asked to write none of them, and reads back zeros
for as long as anyone watches.

**The blank is the guest's.** `clutter_actor_has_allocation` assertions and `Can't update stage
views ... needs an allocation`, which run for the whole black period, are not noise around the
failure: a stage with no allocation composites nothing while the frame clock keeps flipping.

One route is not covered by this census, and is the only way the conclusion could still be wrong:
the traces are vrend's, so a write arriving through venus would be invisible. Against it, the
compositor is on vrend ctx 2 before the push and a session does not migrate renderer mid-run —
but it is untested, and it is cheap to test by logging the surface shares venus takes.

## What the frames say

Measured 2026-09-07, virglrs `0a0a75f` and `cdd82c6`, `Fedora-Workstation-44.enhanced.test.raw`
(r26), 2560x1440 coexist display:

    immediately before update_display(pushed_identity())   536 distinct colours
    +0.3 s                                                 536   (the pre-push frame, still)
    +0.8 s through +60.2 s                                   1   solid black, 2560x1440

Through that whole minute the capture file is rewritten at ~2 Hz at a constant 76437 bytes.
Frames keep being presented; every one of them is black. One run watched for 60 s, so "it does
not recover within a minute" is what is measured -- not "never".

The black is RGB `(0,0,0)`. Its alpha reads `255` and means nothing: the scanout is `BGRX`, and
`swizzle_to_rgba` forces opaque alpha for the `X` channel by construction
(`limina-display/src/lib.rs:326`). This format cannot distinguish a never-written buffer from a
deliberately cleared one, so do not spend a run trying.

The push mints three more IOSurface scanouts from the **classic/vrend** site (`virglrs
src/vrend/resource.rs:1837`), not the venus one: 3 before, 5-6 after. IOSurface scanouts were
therefore already in use while the desktop was painting, and were being read back correctly. The
push does not switch the scanout onto the IOSurface path; it mints more on a path that worked.

## What is ruled out, and how

| suspect | result |
|---|---|
| r26 guest image | 3/3 fail on a pre-r26 clone of the same test golden |
| the virglrs budget commits | 549 -> 1 colours identically at `0292db7` and at `0a0a75f` |
| KosmicKrisp | fails identically on the Sep 6 10:49 and the Sep 7 17:27 dylib |
| the sampler-view GL objects (`0292db7`) | never implicated: the desktop paints until the push |
| the coexist display path as such | `vrend_session_restore` passes on the same EFI-seated vehicle with `with_coexist_display` |
| a slow relayout the test does not wait out | disproved: it is black continuously to +60 s, not late |
| `CaptureBackend` reallocating its staging buffer per `configure_scanout` | disproved **on mechanism**: a same-geometry early return takes the `configure scanout` log from 2168 lines to 0 -- so the guard is proven reached -- and the capture is still 1 colour, 3/3 |
| the capture backend not being on the blob/zero-copy scanout path | refuted from the code: `CaptureBackend` has no `present_surface`, so the vtable default returns `MethodNotSupported` (`rust_to_c.rs:72-80`) and the device falls through to readback on every route |
| the guest picking a scale the test did not intend | measured: mutter settles on 2560x1440 at scale 1.0, which is what `pushed_identity()` is built to provoke |
| the zero-copy sync fallback | instrumented as state (`sync_failures`, `on_readback_fallback`, carried across the per-flip re-declaration, one line per transition each way). Neither line appears. It is also not a latch: the next `SET_SCANOUT` re-arms `iosurface_id`, and one arrives per page-flip |

Readback therefore runs, every frame, and fills the buffer with black. That the file is rewritten
at ~2 Hz is the independent confirmation that `present_frame` keeps being called.

## Still unexplained

The readback path differs between boots: one run produced no blank-readback lines at all while
the capture was black, others produce them continuously. Nothing measures what that boot did
instead.

`0292db7` records 4/4 passes at ~13:56; every failure here -- 16 of them -- is from 16:08 on, and
the host was restarted at ~15:43 between the two. Nothing distinguishes the two sets that has
been controlled for. Do not chase it: whatever it explains, it does not change who is failing to
paint.

## The rules this earned

A gate that panics on a measurement must keep the artifact the measurement was taken from,
*before* the assertion. Every failing run of this test discarded its own frame, because the PNG
was saved after the assert -- so seven runs produced a colour count and no picture, and the one
probe that looked at the desktop before the push settled in a single run what three rounds of
counting could not.

A diagnostic filter is a claim about what the code logs, and it fails silently. Match log
needles case-folded: the one line that would have named this mechanism was excluded for three
rounds by an `IOSurface` that the emitting code spells `iosurface`.

An identifier is not evidence about who owns the thing it names. A trace printing handles and
IOSurface ids supported "the compositor renders offscreen and something copies" for a whole
round; adding the context and the geometry showed those surfaces were 640x688 and 940x600 client
windows. Log what distinguishes the readings, not what is convenient to reach.

"Never a render target" is not "never written", and a census that covers one door is worth
nothing. Draws reach `attach_surface`; blits, copies and clears bind their own framebuffers, and
a transfer the VMM names does not touch the context stream at all. Silence only means silence
once every door is traced.

A negative-space probe needs its positive control in the same run. The after-set became readable
only against a before-set measured the same way -- that is what turned "no full-size render
target" into "context 2 stopped".

A diagnostic for a latching condition must be readable after the fact, not only at the instant it
latches. A one-shot warning about a state that then persists is unfindable by anyone who starts
looking afterwards, which is everyone. Log the state on each use, or expose a counter.

A satisfying mechanism is not a passing test. The staging-buffer theory explained every symptom,
had a unit test, and was wrong; it was announced before it was run. Verify first, then say it.
