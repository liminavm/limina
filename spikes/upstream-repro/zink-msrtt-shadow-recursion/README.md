# zink: shadow attachments of the MSRTT emulation

Without `VK_EXT_multisampled_render_to_single_sampled` (MSRTSS), zink emulates
`EXT_multisampled_render_to_texture` (MSRTT) with a transient MSAA image per attachment, which
`zink_render_attachment_shadow()` fills by a replicate blit (u_blitter) from the single-sampled
texture. Three bugs sit on that path; the series in `series/` fixes them, one commit each. Needs a
Vulkan driver without MSRTSS under zink. Measured: anv on Ice Lake lacks it, and so does venus on
that anv in a QEMU guest; lavapipe has it (`zink_screen.c` also disables it for panvk).

1. **Unbounded recursion** (`0001`, "zink: don't recurse forever populating a shadow
   attachment"). The transient is marked valid only after its blit returns, and pending clears
   are masked across the blit for every attachment *except the one being shadowed*. util_blitter
   rebinds the framebuffer, `zink_set_framebuffer_state()` flushes that attachment's pending
   clear, the flush begins a renderpass, the transient is still invalid, and the replicate blit
   starts again: one `u_blitter:557: Caught recursion. This is a driver bug.` per lap, then a
   stack overflow. The fix masks all pending clears across the blit and restores them in full;
   `zink_blit()` already does the same around blits whose destination is not bound
   (`zink_blit.c`, "avoid applying clears from fb unbind by storing and re-setting them after the
   blit"). The old restore assigned back only the masked bits, so it also dropped the shadowed
   attachment's own clear bit. The clear is deferred, not dropped: `zink_fb_clear_enabled()` reads
   the same mask and the `fb_clears` entries are untouched, so the next renderpass applies it on
   top of the replicated content.
2. **Only the last transient of a multi-attachment shadow stays valid** (`0002`, "zink: keep
   every transient a shadow call populated valid"). Every replicate blit rebinds the framebuffer,
   and `unbind_fb_surface()` marks the transient of every MSRTT attachment of the real framebuffer
   invalid, including the ones the call has just populated. Nothing renders to them between their
   blit and the end of the loop, so the function now marks everything it populated valid after
   it.
3. **The `glFlush` route leaves other MSRTT attachments stale** (`0003`, "zink: populate shadow
   attachments before reducing the fb to flush clears"). When it is not blitting,
   `zink_flush_clears()` reduces `ctx->fb_state` to the attachments with pending clears, begins a
   renderpass to apply them, and puts `fb_state` back by plain assignment. A shadow blit inside
   that renderpass setup makes u_blitter rebind the *reduced* framebuffer through
   `zink_set_framebuffer_state()`, which recomputes `transient_attachments` and the rest of the
   framebuffer-derived state for it; the plain assignment restores none of that, so an MSRTT
   attachment without a pending clear drops out of `transient_attachments` and its invalidated
   transient is later loaded without a replicate blit. The fix populates the transients the clears
   need before reducing the framebuffer, so the blit rebinds the real one and the reduced
   renderpass blits nothing. On main this route recurses (bug 1), so 0001 is what makes it
   reachable; 0003 needs 0002 whenever two or more attachments have pending clears.

## Reproducer

`zink-msrtt-recursion.c` (GLES 3, surfaceless EGL): render to a texture through an MSRTT FBO
(blue), bind another FBO (unbinding invalidates the transient), bind the MSRTT FBO again, issue a
scissored clear (red, lower-left), draw (green, upper-right); then read back the cleared, the
drawn and an untouched quadrant.

    cc -o zink-msrtt-recursion zink-msrtt-recursion.c -lEGL -lGLESv2
    MESA_LOADER_DRIVER_OVERRIDE=zink ./zink-msrtt-recursion [mode]

| mode | what it changes | correct result |
|---|---|---|
| `color` (default) | the sequence above | red, green, blue |
| `zs` | depth attached through MSRTT too; the scissored clear is on depth, the draw at depth 0.75 with `GL_LESS` | green, blue, blue |
| `flush` | a `glFlush` between the clear and the draw: the shadow runs from `zink_flush` → `zink_flush_clears` | red, green, blue |
| `rebind` | the other FBO bound and back between the clear and the draw | red, green, blue |
| `mrt` | a second MSRTT attachment; clear on COLOR0 only; COLOR1's texture overwritten with yellow by `glTexSubImage2D` before pass 3 (only a replicate blit can carry it into the transient); `glFlush` | COLOR0 as `color`; COLOR1 yellow, green, yellow |
| `mrt-rebind` | `mrt` with the FBO round trip instead of the `glFlush` | as `mrt` |
| `mrt3` | three MSRTT attachments; clears on COLOR0 and COLOR1, yellow upload on COLOR2; `glFlush` | COLOR0 and COLOR1 as `color`; COLOR2 yellow, green, yellow |
| `fbfetch` | `mrt` with the clear on COLOR1 and a `GL_EXT_shader_framebuffer_fetch` shader adding green to COLOR0, bound before the `glFlush` | COLOR0 blue, cyan, blue; COLOR1 red, green, yellow |

`zs` is a control: zink's depth/stencil shadow leg never runs on main, because `begin_rendering()`
builds the shadow mask from colour attachments only (the line that would add the depth/stencil bit
is commented out in `zink_context.c`, "maybe TODO but also not handled by legacy rp"). Without the
FBO unbind (a scissored clear on a fresh MSRTT attachment) nothing recurses. `fbfetch` needs a zink
that exposes framebuffer fetch; Ice Lake anv does not, so no run covers it.

## Results

Host: Fedora 44, Intel Iris Plus G7 (Ice Lake), system anv from Fedora mesa 26.1.8 as the Vulkan
driver; zink from the Mesa under test. "Venus" rows run zink and venus from the Mesa under test in
the rig's QEMU 10.2 + virglrenderer 1.3.0 guest, on the same host anv. "+0001" is the first
commit alone, "+series" all three (`86968e1a8f7`).

| Mesa (zink) | Driver | Modes | Result |
|---|---|---|---|
| `main` 92b45bd0f2e | anv (no MSRTSS) | `color` | SIGSEGV after ~4360 "Caught recursion" lines, 5/5 |
| `main` 92b45bd0f2e | venus on anv | `color` | SIGSEGV after ~4358 "Caught recursion" lines, 5/5 |
| `main` 92b45bd0f2e | lavapipe (`LIBGL_ALWAYS_SOFTWARE=1`, has MSRTSS) | `color` | correct (control: emulation not taken) |
| `main` 92b45bd0f2e | anv | `flush`, `rebind`, `mrt`, `mrt-rebind`, `mrt3` | SIGSEGV after ~4360 "Caught recursion" lines, 3/3 each |
| `main` 92b45bd0f2e | anv | `zs` | correct, 5/5 |
| +0001 | anv | `color`, `zs` | correct, 5/5 each |
| +0001 | venus on anv | `color` | correct, 5/5 |
| +0001 | anv | `flush`, `rebind`, `mrt-rebind` | correct, 3/3 each |
| +0001 | anv | `mrt` | **COLOR1 blue, green, blue** (stale transient), 3/3 |
| +0001 | anv | `mrt3` | **COLOR2 blue, green, blue**, 3/3 |
| +0001 and 0003 without 0002 | anv | `mrt3` | **COLOR2 blue, green, blue**, 3/3; the other modes correct |
| +series | anv | `color`, `zs`, `flush`, `rebind`, `mrt`, `mrt-rebind`, `mrt3` | correct, 5/5 each |

Measured 2026-10-08 (`color`, `zs`, venus, lavapipe) and 2026-10-16 (the rest); `main` `b39d173ca93`
gave the same `color` results on 2026-10-05.

Under gdb (`mesa_glthread=false GALLIUM_THREAD=0`, a breakpoint where `begin_rendering()` masks
the shadow request): on `mrt` with 0001 alone, `transient_attachments` goes from `0x3` to `0x1`
across the `glFlush` and stays there for the draw; with the series the shadow runs from
`zink_flush_clears()`, the reduced renderpass's shadow call blits nothing, and
`transient_attachments` stays `0x3` (`mrt`) and `0x7` (`mrt3`) through the draw.

**piglit** (`92b45bd0f2e`, zink over venus in the rig guest, surfaceless EGL, `quick` filtered to
`ext_multisampled_render_to_texture`, `fbo`, `clear`, `framebuffer_srgb`,
`ext_framebuffer_multisample`, `arb_framebuffer_object`, `ext_framebuffer_object`,
`arb_texture_view` and `ext_texture_srgb`, 1379 tests): +0001 (2026-10-08) and +series
(2026-10-16) give identical results test for test: pass 1091, fail 21, crash 26, skip 231,
warn 10. Against `main` the one difference, `ext_texture_array/fbo-depth-array stencil-clear`,
stalled main's run once and passes 3/3 on every build alone. The sRGB and texture-view groups are
there because format-view shadowing runs the patched function. piglit's only MSRTT test,
`ext_multisampled_render_to_texture-clear_color_and_depth`, is in no profile; run alone
(`-auto -fbo`, which surfaceless EGL needs) it passes 3/3 on main and +0001, since its one
unscissored clear on a fresh framebuffer never needs a replicate blit.

## Alternatives

The fix-alternatives step (`docs/upstreaming/patch-diligence.md` §5) ran on bugs 1 and 3.

- **Bug 1:** all four blind proposers wrote 0001's change line for line. A skeptic found the
  `glFlush` route, which became bug 3 (`mrt`).
- **Bug 3:** the proposers restored the derived state after the reduced renderpass instead: a
  snapshot of the fields, a helper that re-derives them, or a second `set_framebuffer_state()`.
  Reviewers found each misses state the nested rebind changes (the framebuffer-fetch input
  attachment, feedback-loop bits), and the second bind also drops `needs_present` on a swapchain
  framebuffer; none was built. Populating before reducing removes the nested rebind instead. Its
  first version broke on two cleared attachments (`mrt3`), which is bug 2.

**Not covered by any run**, found by the reviewers and present on main too: the shadow blit's
`set_framebuffer_state()` clears `ctx->rp_tc_info_updated`, which the `zink_texture_barrier`
caller does not set again (tracked renderpasses under threaded context); framebuffer-fetch paths;
kopper/swapchain MSRTT attachments; two attachments sharing one transient (layers of one texture)
share one `valid` flag.

## As sent

Branch `upstream/zink-msrtt-shadow` in `/Volumes/mesa-cs/mesa-upstream` (a worktree at
`/Volumes/mesa-cs/mesa-zink-msrtt`), three commits on `main` `92b45bd0f2e`, exported to `series/`;
the tree is the one measured as +series. 0001 is also on `upstream/guest-2026-10`.

- 0001 `Fixes: 82add9f2e99` ("zink: avoid recursion during msrtss blits from flushing clears"):
  that commit added the clear save/restore around the replicate blit but left the shadowed
  attachment's own clears enabled. The loop is older: it needs only the emulation
  (`fbff2b6c652`, 2021) and the clear flush on attachment change (`66ceea7ed9a`, 22.2.0);
  `82add9f2e99` first shipped in 23.3.0 and `staging/26.2` carries it.
- 0002 `Fixes: 635f0c5f4fe` ("zink: delete transient attachments instead of leaking them",
  25.2): it added the unbind-time invalidation of the transient (absent at its parent);
  `ef3f7989574` later rewrote it as `res->transient->valid = false`.
- 0003 has no `Fixes:`; the reduced framebuffer dates from `b122c3eaa9e` ("zink: unify/fix clear
  flushing", 25.3), but a shadow inside it recursed until 0001. `Cc: mesa-stable` so it travels
  with 0001.
- All three carry `Cc: mesa-stable` and `Assisted-by: Claude Code`; the messages and comments are
  to be rewritten by the submitter.

Points for the MR description: the three bugs and their order of dependence (0001 exposes 3;
0003 needs 2 for multiple cleared attachments); the reproducer and its modes, before and after;
the `zink_blit()` precedent for 0001; why 0003 populates before reducing instead of restoring
derived state; piglit unchanged; observed in the wild as a browser renderer process crash. Review:
zink is maintained by the author of the commits named above (`82add9f2e99`, `635f0c5f4fe`,
`b122c3eaa9e`).
