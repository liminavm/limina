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
    MESA_LOADER_DRIVER_OVERRIDE=zink ./zink-msrtt-recursion [color|zs|flush|rebind|mrt|mrt-rebind]

`color` (the default) is the sequence above. `zs` attaches a depth texture through MSRTT as well and
leaves the scissored clear on depth instead: pass 1 writes depth 0.5, pass 3 clears the lower-left
quadrant's depth to 1.0 and draws green at depth 0.75 with `GL_LESS`, so green shows where the
clear landed and pass 1's blue wherever the replicated depth survived. It is a control: zink's
depth/stencil shadow leg never runs on main, because `begin_rendering()` builds the shadow mask
from colour attachments only (the line that would add the depth/stencil bit is commented out in
`zink_context.c`, "maybe TODO but also not handled by legacy rp"). The patch changes that leg's
mask too, without effect.

Without the unbind (scissored clear on a fresh MSRTT attachment) it does not recurse, so the
invalidated-transient step is required.

The other modes change how the pending clear is reached. `flush` puts a `glFlush` between the
clear and the draw, so the shadow runs from `zink_flush` → `zink_flush_clears` (backtrace on
main) instead of from the draw. `rebind` binds the other FBO and back instead; the recursion
then still starts at the draw. `mrt` and `mrt-rebind` add a second MSRTT colour attachment,
clear only COLOR0, and overwrite COLOR1's texture with yellow by `glTexSubImage2D` before
pass 3. Only a replicate blit can carry that yellow into COLOR1's transient, so a stale
transient reads pass 1's blue.

## Results

Host: Fedora 44, Intel Iris Plus G7 (Ice Lake), system anv from Fedora mesa 26.1.8 as the
Vulkan driver; zink from the Mesa under test. The venus rows run zink and venus from the Mesa
under test inside the rig guest, on the same host anv.

| Mesa (zink) | Vulkan driver | Result |
|---|---|---|
| `main` b39d173ca93 | anv, ICL (no MSRTSS) | SIGSEGV, ~4360 "Caught recursion" lines before it, 5/5 |
| `main` + fix (series tip e09e44d2d0d) | same | `pixel 0: 255 0 0 255`, `pixel 1: 0 255 0 255`, `pixel 2: 0 0 255 255`, `ok`, exit 0, 5/5 |
| `main` b39d173ca93 | lavapipe (has MSRTSS; `LIBGL_ALWAYS_SOFTWARE=1`) | `ok`, exit 0 (control: emulation path not taken) |
| `main` 92b45bd0f2e | anv, ICL (no MSRTSS) | SIGSEGV, ~4360 "Caught recursion" lines before it, 5/5 |
| `main` 92b45bd0f2e + fix | same | the same three pixels, `ok`, exit 0, 5/5 |
| `main` 92b45bd0f2e | system lavapipe (`LIBGL_ALWAYS_SOFTWARE=1` plus `VK_DRIVER_FILES` naming its ICD) | the same three pixels, `ok`, exit 0 (control) |
| `main` 92b45bd0f2e | venus on that anv, QEMU 10.2 + virglrenderer 1.3.0 guest | SIGSEGV, ~4358 "Caught recursion" lines before it, 5/5 |
| `main` 92b45bd0f2e + fix | same | the same three pixels, `ok`, exit 0, 5/5 |
| `main` 92b45bd0f2e, `zs` | anv, ICL | green, blue, blue, `ok`, exit 0, 5/5 (control) |
| `main` 92b45bd0f2e + fix, `zs` | same | green, blue, blue, `ok`, exit 0, 5/5 |

| `main` 92b45bd0f2e, `flush`, `rebind`, `mrt`, `mrt-rebind` | anv, ICL | SIGSEGV, ~4360 "Caught recursion" lines before it, 3/3 each |
| `main` 92b45bd0f2e + fix, `flush`, `rebind` | same | red, green, blue, `ok`, 3/3 each |
| `main` 92b45bd0f2e + fix, `mrt-rebind` | same | COLOR0 red, green, blue; COLOR1 yellow, green, yellow; `ok`, 3/3 |
| `main` 92b45bd0f2e + fix, `mrt` | same | COLOR0 red, green, blue; **COLOR1 blue, green, blue** (stale transient), `WRONG PIXELS`, 3/3 |

Measured 2026-10-05 (`b39d173ca93`), 2026-10-08 and 2026-10-16 (`92b45bd0f2e`).

**Open: the `glFlush` route leaves other MSRTT attachments stale.** When the shadow runs from
`zink_flush_clears()` outside a blit, the framebuffer it sees is the one `zink_flush_clears()`
doctored: every attachment without a pending clear is NULLed. The replicate blit's u_blitter
save/restore calls `zink_set_framebuffer_state()` with that doctored state, which recomputes
`ctx->transient_attachments` (and the bind counts and other derived fields) from it, and
`zink_flush_clears()` then puts the real `fb_state` back by plain assignment without
recomputing them. Under gdb on `mrt` with the fix, `transient_attachments` goes from `0x3` to
`0x1` across the `glFlush` and stays there for the draw, so COLOR1's invalidated transient is
loaded without a replicate blit; on `mrt-rebind` the draw sees `0x3`. On main this route
recurses, so the fix is what makes it reachable.

**piglit, main vs fix** (2026-10-08, `92b45bd0f2e`): zink over venus in the rig guest, surfaceless
EGL, `quick` filtered to `fbo`, `clear`, `framebuffer_srgb`, `ext_framebuffer_multisample`,
`arb_framebuffer_object`, `ext_framebuffer_object`, `arb_texture_view` and `ext_texture_srgb`
(1379 tests, 3496 results): no regressions, identical fail (21), crash (26) and skip (309) sets.
The one difference, `ext_texture_array/fbo-depth-array stencil-clear`, stalled the main run once
and passes 3/3 on both builds alone. The sRGB and texture-view groups are there because
format-view shadowing runs the patched function. piglit's only MSRTT test,
`ext_multisampled_render_to_texture-clear_color_and_depth`, is built but in no profile; run alone
(`-auto -fbo`, which surfaceless EGL needs) it passes 3/3 on both, since its one unscissored clear
on a fresh framebuffer never needs a replicate blit.

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
around the replicate blit but left the shadowed attachment's own clears enabled. The loop itself
is older: it needs only the emulation (`fbff2b6c652`, 2021) and the clear flush on attachment
change (`66ceea7ed9a`, "zink: lift clearing on fb state change up a level", 22.2.0).
`82add9f2e99` first shipped in 23.3.0 and `staging/26.2` carries it, so the stable reach is the
same whichever commit the tag names.
