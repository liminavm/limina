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

## As sent

The commit — message, `Fixes:`, trimmed comments — is on branch `upstream/guest-2026-10` of
`liminavm/mesa`. `Fixes: 82add9f2e99` because that commit added the clear save/restore
around the replicate blit but left the shadowed attachment's own clears enabled; the emulation
itself (fbff2b6c652) predates the clear flush on attachment change. Both reach every live stable
branch.
