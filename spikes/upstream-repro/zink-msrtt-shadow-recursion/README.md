# zink: don't recurse forever populating a shadow attachment

**Bug.** Without `VK_EXT_multisampled_render_to_single_sampled`, zink emulates
`EXT_multisampled_render_to_texture` with a transient MSAA image, which
`zink_render_attachment_shadow()` fills by a replicate blit from the single-sampled texture and
marks valid only after the blit returns. Pending clears are masked across that blit for every
attachment *except the one being shadowed*. util_blitter rebinds the framebuffer,
`zink_set_framebuffer_state()` flushes that attachment's pending clear, the flush begins a
renderpass, the transient is still invalid, and the replicate blit starts again: unbounded
recursion, one `u_blitter:557: Caught recursion. This is a driver bug.` per lap, then a stack
overflow.

**Reproducer.** `zink-msrtt-recursion.c` (GLES 3, surfaceless EGL): render to a texture through
an MSRTT FBO, bind another FBO (unbinding invalidates the transient), bind the MSRTT FBO again,
issue a scissored clear, draw; then read back three pixels — the cleared quadrant (red), the drawn
quadrant (green), and an untouched quadrant that must keep the first pass's blue. Needs a Vulkan
driver without MSRTSS under zink. Measured: anv on Ice Lake lacks it, and so does venus on that anv in a
QEMU guest; lavapipe has it (`zink_screen.c` also disables it for panvk).

    cc -o zink-msrtt-recursion zink-msrtt-recursion.c -lEGL -lGLESv2
    MESA_LOADER_DRIVER_OVERRIDE=zink ./zink-msrtt-recursion

Without the unbind (scissored clear on a fresh MSRTT attachment) it does not recurse, so the
invalidated-transient step is required.

## Results

Host: Fedora 44, Intel Iris Plus G7 (Ice Lake), system anv from Fedora mesa 26.1.8 as the
Vulkan driver; zink from the Mesa under test.

| Mesa (zink) | Vulkan driver | Result |
|---|---|---|
| `main` b39d173ca93 | anv, ICL (no MSRTSS) | SIGSEGV, ~4360 "Caught recursion" lines before it, 5/5 |
| `main` + fix (series tip e09e44d2d0d) | same | `pixel 0: 255 0 0 255`, `pixel 1: 0 255 0 255`, `pixel 2: 0 0 255 255`, `ok`, exit 0, 5/5 |
| `main` b39d173ca93 | lavapipe (has MSRTSS; `LIBGL_ALWAYS_SOFTWARE=1`) | `ok`, exit 0 (control: emulation path not taken) |

Measured 2026-10-05.

The fixed run's pixels check the "deferred, not dropped" claim: the red quadrant is the scissored
clear applied after the transient was repopulated, and the blue quadrant is content the replicate
blit carried over.

## MR description (draft)

> **zink: don't recurse forever populating a shadow attachment**
>
> When the Vulkan driver lacks VK_EXT_multisampled_render_to_single_sampled, the
> EXT_multisampled_render_to_texture emulation fills its transient MSAA image with a replicate
> blit. Pending clears on the other attachments are masked across that blit, but not the one on
> the attachment being shadowed: u_blitter rebinds the framebuffer, zink_set_framebuffer_state
> flushes that clear, the flush begins a renderpass while the transient is still invalid, and the
> blit starts over until the stack overflows ("Caught recursion" from u_blitter on every lap).
>
> Reproducer (attached; zink on anv/Ice Lake, which has no MSRTSS): render through an MSRTT FBO,
> bind another FBO, rebind, scissored clear, draw. Before: SIGSEGV 5/5. After: exit 0 5/5 with the
> expected pixels — the clear lands after the replicated content, and untouched content survives.
> Observed in the wild as a browser renderer process crash.
>
> The fix masks all pending clears across the blit and restores them afterwards. Nothing can
> apply a clear while its bit is masked (zink_fb_clear_enabled reads the same mask) and the
> fb_clears entries are untouched, so the clear is only deferred to the following renderpass.

## Proposed commit message

```
zink: don't recurse forever populating a shadow attachment

zink_render_attachment_shadow() replicate-blits a texture into its
transient MSAA image (the EXT_multisampled_render_to_texture emulation
used when the driver has no VK_EXT_multisampled_render_to_single_sampled)
and marks the transient valid only once that blit returns.

util_blitter rebinds the framebuffer, and zink_set_framebuffer_state()
flushes pending clears when the bound attachments change. Every
attachment's clears were masked off across the blit except the one being
shadowed, so that flush re-entered begin_rendering() while the transient
was still invalid and the replicate blit started over, recursing until
the stack ran out.

Mask all pending clears across the blit and restore them afterwards. The
clear is deferred, not dropped: zink_fb_clear_enabled() reads the same
mask, so nothing can apply it while it is masked, and the fb_clears
entries are untouched; the following renderpass applies it to the
now-populated transient, which is the order the application asked for.

Fixes: 82add9f2e99 ("zink: avoid recursion during msrtss blits from flushing clears")
Cc: mesa-stable
Signed-off-by: Gustavo Noronha Silva <gustavo@noronha.dev.br>
```

**`Fixes:` — recommended 82add9f2e99, your call.** That commit (`Part-of: !22577`, first in
23.2) added the clear save/restore around the replicate blit for exactly this re-entry, but its
mask deliberately leaves the shadowed attachment's own clears enabled — that exclusion is what this
patch removes. The MSRTT emulation itself is older (fbff2b6c652, "zink: implement
GL_EXT_multisampled_render_to_texture", 21.3), but at that commit `zink_set_framebuffer_state()`
did not flush pending clears on an attachment change, so the re-entry path did not exist there;
the exact commit that made the own-attachment flush reachable was not pinned down. Either sha is in
every live stable branch, so the backport reach is the same.

## Recommended code-comment trim

The patch's comment narrates the bug; upstream prefers the invariant (the story is in the commit
message).

Before:

```c
      /* Mask off ALL pending clears across the blit, this attachment's own
       * included, and restore them afterwards.
       *
       * util_blitter rebinds the framebuffer, and zink_set_framebuffer_state
       * flushes pending clears when the bound attachments change. Leaving this
       * attachment's clear enabled meant that flush re-entered
       * zink_batch_rp -> begin_rendering while the transient was still invalid
       * (it is only marked valid once the blit below returns), so the replicate
       * blit started over: unbounded recursion until the stack ran out.
       * u_blitter's "Caught recursion" only logs, it does not break the cycle.
       *
       * The clear is not lost, only deferred: restored below, it is applied by
       * the renderpass that follows, against the now-populated transient. That
       * is the order the application asked for anyway — the clear was issued
       * after the contents this blit is replicating.
       */
```

After:

```c
      /* mask all pending clears, this attachment's included: the blit rebinds
       * the framebuffer, and flushing a clear here would begin a renderpass
       * that starts this replicate blit again. they are restored below and
       * applied by the next renderpass
       */
```
