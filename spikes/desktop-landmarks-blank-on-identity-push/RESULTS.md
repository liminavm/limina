# `l2_desktop_restore_landmarks` blanks on the monitor-identity push

`seated_gpu_workload_survives_restore_unchanged` fails its pre-suspend content floor with
`1 distinct colours`. That failure has twice been read as a desktop that never seated, and it
is not one: **the desktop seats and paints, and the monitor-identity push blanks it.**

## What the frames say

Measured 2026-09-07, virglrs `0a0a75f`, `Fedora-Workstation-44.enhanced.test.raw` (r26),
2560x1440 coexist display:

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

Guest-side during the black period: no EGL, venus, zink or renderer errors. gnome-shell answers
D-Bus (`ShellVersion '50.0'`) and logs only `clutter_actor_has_allocation` assertions and
`Can't update stage views ... needs an allocation`, which is a compositor mid-relayout. The push
is what provokes the relayout -- it changes the EDID so the guest drops from 250% scale to 100%.

The push is immediately followed by three IOSurface scanout mints:

    display-control: DisplayControl { display_id: 0, size: Some((2560,1440)), edid: Some(..) }
    [virglrs] vrend: iosurface scanout: 2560x1440 B8G8R8X8_UNORM (IOSurface id 113); ...
    [virglrs] vrend: iosurface scanout: 2560x1440 B8G8R8X8_UNORM (IOSurface id 117); ...
    [virglrs] vrend: iosurface scanout: 2560x1440 B8G8R8X8_UNORM (IOSurface id 55);  ...

They come from the **classic/vrend** mint site (`virglrs src/vrend/resource.rs:1837`), not the
venus one. Measured on both sides of the push: **3 before, 6 after**. IOSurface scanouts were
therefore already in use while the desktop was painting at 537 colours, and were being read back
correctly through the headless sink. The push does not switch the scanout onto the IOSurface
path; it mints three more on a path that already worked.

That is the fact any remaining theory has to fit. A freshly minted IOSurface is zero-filled, so
reading one that renders never landed in yields black forever -- which points at *which* surface
the readback resolves to, not at whether the path is covered.

## What is ruled out, and how

| suspect | result |
|---|---|
| r26 guest image | 3/3 fail on a pre-r26 clone of the same test golden |
| the virglrs budget commits | 549 -> 1 colours identically at `0292db7` and at `0a0a75f` |
| KosmicKrisp | fails identically on the Sep 6 10:49 and the Sep 7 17:27 dylib |
| the sampler-view GL objects (`0292db7`) | never implicated: the desktop paints until the push |
| the coexist display path as such | `vrend_session_restore` passes on the same EFI-seated vehicle with `with_coexist_display` |
| a slow relayout the test does not wait out | disproved: it is black continuously to +60 s, not late |
| `CaptureBackend` reallocating its staging buffer per `configure_scanout` | disproved **on mechanism**, not on timing: a same-geometry early return takes the `configure scanout` log from 2168 lines to 0 -- so the guard is proven reached -- and the capture is still 1 colour, 3/3 |
| the capture backend not being on the blob/zero-copy scanout path at all | refuted from the code: `CaptureBackend` has no `present_surface`, so the vtable default returns `MethodNotSupported` (`rust_to_c.rs:72-80`), and the device falls through to readback on every route -- `virtio_gpu.rs:2470-2478` -> `alloc_frame` (2521) -> `read_iosurface` (2534) -> `present_frame` (2601), under a comment naming the headless capture sink |

Readback therefore runs, every frame, and fills the buffer with black. That the file is rewritten
at ~2 Hz is the independent confirmation that `present_frame` keeps being called.

## What readback actually resolves

Measured in one boot with `LIMINA_READBACK_TRACE=1` (ids are comparable only within a boot):

    pre-push mints      IOSurface 21, 23, 27      desktop painting, 537 colours
    post-push mints     IOSurface 63, 75, 21      (21 is a REUSED id, not the same surface)
    blank readbacks     IOSurface 63, 75, 21      1440 of 1440 rows read, every flip

**Readback resolves the post-push surfaces, and they are empty.** Three resources page-flip in
strict rotation for the whole black period; each returns the full row count, so the copy
succeeds and the surface it copies from simply has nothing in it. The pre-push set works, so
neither minting nor readback is broken in general -- it is the surfaces minted *at the push*
that renders never reach, while each one's mint line claims "renders land in the surface
directly".

Two guards on that claim. `21` appears in both lists, so for that surface alone old and new are
indistinguishable -- an IOSurface id names a surface only while it lives. 63 and 75 are
unambiguously post-push and carry the conclusion on their own. And "blank" here is the first row,
which is what the trace tests; a frame with content only below row 0 would be misread, though
nothing else about the capture suggests one.

So the open question is no longer where the pixels go. It is why a scanout resource minted at
the push does not receive renders when an identically-minted one from thirty seconds earlier
does.

## Renders keep going to the surfaces from before the push

Measured in one boot with both traces armed (`LIMINA_READBACK_TRACE=1`):

    mints            pre-push 7, 25, 26      post-push adds 49, 69
    render targets   7, 25, 26, 31, 44, 47, 50, 51, 61, 63, 72   (116 attachments)
    blank readbacks  none this run

**The surfaces minted at the push are never attached as a render target.** The pre-push scanouts
are, repeatedly; 49 and 69 appear nowhere in 116 attachments. So the compositor goes on rendering
into the surfaces it had while `SET_SCANOUT` presents the new ones -- two holders of "the current
scanout" with only one of them updated. The presented surface is one nothing draws into, which is
why it is black immediately, black forever, and error-free on every path.

This is the reading that "the new surfaces are empty" could not establish on its own: empty is
equally what nothing-is-rendering looks like. The render-target list rules that out -- rendering
continues, at volume, to the wrong surfaces.

**Unexplained, and left that way:** this boot produced *no* blank-readback lines, where the two
before it produced them continuously, and the capture was black in all three. The readback path
therefore differs run to run. The `sync_iosurface` latch clearing `s.iosurface_id` and diverting
to `read_2d_resource` (which this trace does not cover) would produce exactly that, but nothing
here measures it. It does not affect the finding above, which is measured on the render side.

## Ruled out along the way, and the cheapest cut

In the classic (`ctx_id == 0`) branch of `flush_resource`, a failing `sync_iosurface` logs a
warning and **clears `s.iosurface_id = None`** (`virtio_gpu.rs:2425-2440`), permanently switching
that scanout to a different readback path for the rest of the session. A one-way switch of this
shape is the only mechanism found so far that explains black *forever* rather than black for a
frame. Whether it fires here is one grep away and has never been checked, because every log
filter this spike used matched `IOSurface` case-sensitively and the device spells its own
diagnostic `vrend iosurface sync failed for res N`.

That grep can only ever confirm, never exclude. The warning fires once, at the instant the
switch flips, and the supervisor log is read as a retained tail -- so a run that comes back with
no match is inconclusive, not negative. Confirming the switch *did not* happen needs the state
made queryable rather than the event caught: a counter dumped at teardown, or the fallback
re-logged on each flush that takes it.

The other half of the question -- whether readback writes zeros or does not write at all -- is
answerable in any format by pre-filling the staging buffer with a sentinel before readback
instead of trusting that zero means untouched. Black then means something wrote black; sentinel
means nothing wrote. One memset in a debug path, and it also separates reading the wrong surface
from reading no surface.

Not yet measured, and worth having in the same run: the count of `iosurface scanout` mints
*before* the push, so "the push switches the scanout path" is a measurement rather than an
inference from a log window that only ever showed the last 80 matching lines.

## The uncontrolled variable

`0292db7` records 4/4 passes at ~13:56. Every failure here -- 14 of them -- is from 16:08 on.
The host was restarted at ~15:43, between the two. Nothing distinguishes the two sets that has
been controlled for, so this is a reproducible failure with an unexplained set of passes behind
it, not a defect proved deterministic.

## The rules this earned

A gate that panics on a measurement must keep the artifact the measurement was taken from,
*before* the assertion. Every failing run of this test discarded its own frame, because the PNG
was saved after the assert -- so seven runs produced a colour count and no picture, and the one
probe that looked at the desktop before the push settled in a single run what three rounds of
counting could not.

A diagnostic filter is a claim about what the code logs, and it fails silently. Match log
needles case-folded: the one line that would have named this mechanism was excluded for three
rounds by an `IOSurface` that the emitting code spells `iosurface`.

A diagnostic for a latching condition must be readable after the fact, not only at the instant
it latches. A one-shot warning about a state that then persists forever is unfindable by anyone
who starts looking afterwards -- which is everyone. Log the state on each use, or expose a
counter; do not make the reader have been listening at the right moment.

A satisfying mechanism is not a passing test. The staging-buffer theory explained every symptom,
had a unit test, and was wrong; it was announced before it was run. Verify first, then say it.
