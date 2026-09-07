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

Guest-side during the black period: no EGL, venus, zink or renderer errors. gnome-shell logs
only `clutter_actor_has_allocation` assertions and `Can't update stage views ... needs an
allocation`, which is a compositor mid-relayout. The push is what provokes the relayout -- it
changes the EDID so the guest drops from 250% scale to 100%.

## What is ruled out, and how

| suspect | result |
|---|---|
| r26 guest image | 3/3 fail on a pre-r26 clone of the same test golden |
| the virglrs budget commits | 549 -> 1 colours identically at `0292db7` and at `0a0a75f` |
| KosmicKrisp | `libvulkan_kosmickrisp.dylib` dated Sep 6 10:49, unchanged across every run |
| the sampler-view GL objects (`0292db7`) | never implicated: the desktop paints until the push |
| the coexist display path as such | `vrend_session_restore` passes on the same EFI-seated vehicle with `with_coexist_display` |
| a slow relayout the test does not wait out | disproved: it is black continuously to +60 s, not late |

## The uncontrolled variable

`0292db7` records 4/4 passes at ~13:56. Every failure here -- 10 of them -- is from 16:08 on.
The host was restarted at ~15:43, between the two. Nothing distinguishes the two sets that has
been controlled for, so this is a reproducible failure with an unexplained set of passes behind
it, not a defect proved deterministic.

## Next

Score it against the C reference leg: that separates a virglrs scanout defect from a guest
mutter one, and nothing cheaper does. A screenshot taken inside the guest during the black
period answers the same question from the other side -- guest painted and host black is
scanout; black on both is the compositor.

## The rule this earned

A gate that panics on a measurement must keep the artifact the measurement was taken from,
*before* the assertion. Every failing run of this test discarded its own frame, because the PNG
was saved after the assert -- so seven runs produced a colour count and no picture, and the one
probe that looked at the desktop before the push settled in a single run what three rounds of
counting could not.
