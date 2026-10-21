# zink: shadow attachments of the MSRTT emulation

Without `VK_EXT_multisampled_render_to_single_sampled` (MSRTSS), zink emulates
`EXT_multisampled_render_to_texture` (MSRTT) with a transient MSAA image per attachment, which
`zink_render_attachment_shadow()` fills by a replicate blit (u_blitter) from the single-sampled
texture. Three bugs sit on that path; the series in `series/` fixes them in `0002`–`0004`, one
commit each, after `0001` fixes a fourth that current main added on the same path (below the list).
Needs a Vulkan driver without MSRTSS under zink. Measured: anv on Ice Lake lacks it, and so does venus on
that anv in a QEMU guest; lavapipe has it (`zink_screen.c` also disables it for panvk).

1. **Unbounded recursion** (`0002`, "zink: don't recurse forever populating a shadow
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
2. **Only the last transient of a multi-attachment shadow stays valid** (`0003`, "zink: keep
   every transient a shadow call populated valid"). Every replicate blit rebinds the framebuffer,
   and `unbind_fb_surface()` marks the transient of every MSRTT attachment of the real framebuffer
   invalid, including the ones the call has just populated. Nothing renders to them between their
   blit and the end of the loop, so the function now marks everything it populated valid after
   it.
3. **The `glFlush` route leaves other MSRTT attachments stale** (`0004`, "zink: populate shadow
   attachments before reducing the fb to flush clears"). When it is not blitting,
   `zink_flush_clears()` reduces `ctx->fb_state` to the attachments with pending clears, begins a
   renderpass to apply them, and puts `fb_state` back by plain assignment. A shadow blit inside
   that renderpass setup makes u_blitter rebind the *reduced* framebuffer through
   `zink_set_framebuffer_state()`, which recomputes `transient_attachments` and the rest of the
   framebuffer-derived state for it; the plain assignment restores none of that, so an MSRTT
   attachment without a pending clear drops out of `transient_attachments` and its invalidated
   transient is later loaded without a replicate blit. The fix populates the transients the clears
   need before reducing the framebuffer, so the blit rebinds the real one and the reduced
   renderpass blits nothing. On main this route recurses (bug 1), so 0002 is what makes it
   reachable; 0004 needs 0003 whenever two or more attachments have pending clears.

**The nested draw starts no renderpass on current main** (`0001`, "zink: keep the renderpass atom
dirty while the renderpass starts"). Since `769b356b5b6` ("zink: emit the draw synchronization and
the renderpass as atoms", !45263, merged 2026-10-16) `zink_draw_atoms()` clears each atom's bit
before running it, so `ZINK_ATOM_RENDERPASS` is clean while `zink_batch_rp()` runs. When
`begin_rendering()` shadows an attachment from a draw, the u_blitter draw nested in it is outside a
renderpass with the atom clean, so it starts none: debug builds abort on `validate_atoms()`
(`assert(ctx->in_rp || zink_is_atom_dirty(ctx, ZINK_ATOM_RENDERPASS))`), release builds record the
replicate draw outside any renderpass. Everything else relies on "not in a renderpass ⇒ the atom is
dirty" (`zink_batch_no_rp()` marks it; `zink_set_framebuffer_state()` does not), so the fix keeps
the atom dirty across `zink_batch_rp()` and leaves it dirty afterwards only if `rp_layout_changed`
was set again during the call. Main-only: no release branch has `769b356b5b6`, so stable and the
guest branch take `0002`–`0004` without it.

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
the rig's QEMU 10.2 + virglrenderer 1.3.0 guest, on the same host anv. "+0002" is the recursion
fix alone, "+series" `0002`–`0004` (`86968e1a8f7` on `92b45bd0f2e`, `e12c40e5f57` on
`af2f67b2b36`), "+0001" the atom fix alone and "+all four" the branch as sent (`723e69750c7`).
"Release" builds add `-Db_ndebug=true`.

| Mesa (zink) | Driver | Modes | Result |
|---|---|---|---|
| `main` 92b45bd0f2e | anv (no MSRTSS) | `color` | SIGSEGV after ~4360 "Caught recursion" lines, 5/5 |
| `main` 92b45bd0f2e | venus on anv | `color` | SIGSEGV after ~4358 "Caught recursion" lines, 5/5 |
| `main` 92b45bd0f2e | lavapipe (`LIBGL_ALWAYS_SOFTWARE=1`, has MSRTSS) | `color` | correct (control: emulation not taken) |
| `main` 92b45bd0f2e | anv | `flush`, `rebind`, `mrt`, `mrt-rebind`, `mrt3` | SIGSEGV after ~4360 "Caught recursion" lines, 3/3 each |
| `main` 92b45bd0f2e | anv | `zs` | correct, 5/5 |
| +0002 | anv | `color`, `zs` | correct, 5/5 each |
| +0002 | venus on anv | `color` | correct, 5/5 |
| +0002 | anv | `flush`, `rebind`, `mrt-rebind` | correct, 3/3 each |
| +0002 | anv | `mrt` | **COLOR1 blue, green, blue** (stale transient), 3/3 |
| +0002 | anv | `mrt3` | **COLOR2 blue, green, blue**, 3/3 |
| +0002 and 0004 without 0003 | anv | `mrt3` | **COLOR2 blue, green, blue**, 3/3; the other modes correct |
| +series | anv | `color`, `zs`, `flush`, `rebind`, `mrt`, `mrt-rebind`, `mrt3` | correct, 5/5 each |
| `main` af2f67b2b36 | anv | all but `zs` | SIGSEGV after ~4395 "Caught recursion" lines, 3/3 each |
| `main` af2f67b2b36 | anv | `zs` | **SIGABRT**, `validate_atoms` assertion, 3/3 |
| +series on af2f67b2b36 | anv | `flush`, `mrt`, `mrt3` | correct, 5/5 each |
| +series on af2f67b2b36 | anv | `color`, `zs`, `rebind`, `mrt-rebind` | **SIGABRT**, `validate_atoms` assertion, 5/5 each |
| `main` 7d6e9088821 | anv | all but `zs` | SIGSEGV after ~4395 "Caught recursion" lines, 5/5 each |
| `main` 7d6e9088821 | anv | `zs` | **SIGABRT**, `validate_atoms` assertion, 5/5 |
| `main` 7d6e9088821, release | anv | `zs` | correct pixels, 5/5; the replicate draw runs with `in_rp == 0` (gdb) |
| `main` 7d6e9088821, release | venus on anv, validation layer | `zs` | correct pixels; `VUID-vkCmdDraw-renderpass` ×2, `VUID-vkCmdDraw-None-08644` ×2 |
| +0001 | anv | `zs` | correct, 5/5; `in_rp == 1` after the replicate draw |
| +0001 | venus on anv, validation layer | `zs` | correct, no validation messages |
| +0001 | anv | all but `zs` | SIGSEGV after ~4395 "Caught recursion" lines, 5/5 each |
| +all four | anv | all seven | correct, 5/5 each |
| `limina-guest` (26.2.5 + the series) | anv | all seven | correct, 5/5 each |

Measured 2026-10-08 (`color`, `zs`, venus, lavapipe) and 2026-10-16 (the rest); `main` `b39d173ca93`
gave the same `color` results on 2026-10-05.

Since `769b356b5b6`, main reaches the atom bug only in `zs`, because the other modes recurse
first; with `0002`–`0004` alone, every draw-started shadow (`color`, `zs`, `rebind`, `mrt-rebind`)
reaches it. Shadows started by `glFlush` (`flush`, `mrt`, `mrt3`) run from `zink_flush_clears()`,
outside a draw, and never hit it. The `zs` abort backtrace runs `zink_draw_atoms()` →
`zink_batch_rp()` → `begin_rendering()` → `zink_render_attachment_shadow()` →
`util_blitter_blit_generic()` → `zink_draw_vbo()` → `validate_atoms()`.

Under gdb (`mesa_glthread=false GALLIUM_THREAD=0`, a breakpoint where `begin_rendering()` masks
the shadow request): on `mrt` with 0002 alone, `transient_attachments` goes from `0x3` to `0x1`
across the `glFlush` and stays there for the draw; with the series the shadow runs from
`zink_flush_clears()`, the reduced renderpass's shadow call blits nothing, and
`transient_attachments` stays `0x3` (`mrt`) and `0x7` (`mrt3`) through the draw.

**piglit** (zink over venus in the rig guest, surfaceless EGL, `quick` filtered to
`ext_multisampled_render_to_texture`, `fbo`, `clear`, `framebuffer_srgb`,
`ext_framebuffer_multisample`, `arb_framebuffer_object`, `ext_framebuffer_object`,
`arb_texture_view` and `ext_texture_srgb`, 1379 tests): on `92b45bd0f2e`, +0002 (2026-10-08) and
+series (2026-10-16); on `7d6e9088821`, `main` and +all four (2026-10-19): all four runs give
identical results test for test, pass 1091, fail 21, crash 26, skip 231, warn 10. Against `main`
`92b45bd0f2e` the one difference, `ext_texture_array/fbo-depth-array stencil-clear`,
stalled main's run once and passes 3/3 on every build alone. The sRGB and texture-view groups are
there because format-view shadowing runs the patched function. That run's piglit predates the
`ext_multisampled_render_to_texture`/`…texture2` conformance tests (piglit `61bc1006c`,
`c51282639`); its one MSRTT test, `clear_color_and_depth`, run alone (`-auto -fbo`, which
surfaceless EGL needs) passes 3/3 on main and +0002, since its one unscissored clear on a fresh
framebuffer never needs a replicate blit.

## Suite test

The `zs` mode as a piglit test (`docs/upstreaming/patch-diligence.md` §7):
`ext_multisampled_render_to_texture2-depth-texture-partial-clear`, branch
`msrtt2-depth-partial-clear` on `gitlab.freedesktop.org/kov/piglit` (`44714bf80`, on piglit
`cff83ebb6`). It checks only the cleared quadrant, since the spec discards a multisampled depth
texture's contents. 2026-10-21, zink on venus in the rig guest, debug builds, `-auto -fbo`:

| Mesa | result |
|---|---|
| `main` `99476694236` | abort on `validate_atoms`, 3/3 |
| `main` `7d6e9088821` | abort, 3/3 |
| `99476694236` + 0001 alone (the !45485 commit) | pass, 5/5 |
| `7d6e9088821` + all four | pass, 3/3 |

On lavapipe the test skips: lavapipe has `VK_EXT_multisampled_render_to_single_sampled`, and zink
on it exposes no `GL_EXT_multisampled_render_to_texture2`. Of Mesa's zink CI drivers anv and nvk
lack MSRTSS (lavapipe, radv and turnip advertise it). `zink-anv-tgl` runs piglit `quick_gl` on a
`debugoptimized` build, which keeps asserts, so it can fail the test once Mesa CI's piglit pin
moves past it. Not checked: whether CI's piglit build has the GLES tests, and what the nvk jobs
run.

## Alternatives

The fix-alternatives step (`docs/upstreaming/patch-diligence.md` §5) ran on bugs 1 and 3.

- **Bug 1:** all four blind proposers wrote 0002's change line for line. A skeptic found the
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

Branch `zink-msrtt-shadow` on `gitlab.freedesktop.org/kov/mesa` (local branch of the same name
in `/Volumes/mesa-cs/mesa-upstream`), four commits on `main` `7d6e9088821`, exported to `series/`;
the tree is the one measured as "+all four". `upstream/zink-msrtt-shadow` keeps `0002`–`0004` on
`92b45bd0f2e`, the base the gdb traces of bugs 2 and 3 ran on. The guest branch `limina-guest`
carries `0002`–`0004`; at 26.2.5 it has no atom loop and needs no `0001`.

- 0001 `Fixes: 769b356b5b6` ("zink: emit the draw synchronization and the renderpass as atoms");
  main only, so no `Cc: mesa-stable`. Sent alone as mesa!45485, reworded by the submitter; the
  maintainer asked for a piglit test before review, which is the suite test above.

- 0002 `Fixes: 82add9f2e99` ("zink: avoid recursion during msrtss blits from flushing clears"):
  that commit added the clear save/restore around the replicate blit but left the shadowed
  attachment's own clears enabled. The loop is older: it needs only the emulation
  (`fbff2b6c652`, 2021) and the clear flush on attachment change (`66ceea7ed9a`, 22.2.0);
  `82add9f2e99` first shipped in 23.3.0 and `staging/26.2` carries it.
- 0003 `Fixes: 635f0c5f4fe` ("zink: delete transient attachments instead of leaking them",
  25.2): it added the unbind-time invalidation of the transient (absent at its parent);
  `ef3f7989574` later rewrote it as `res->transient->valid = false`.
- 0004 has no `Fixes:`; the reduced framebuffer dates from `b122c3eaa9e` ("zink: unify/fix clear
  flushing", 25.3), but a shadow inside it recursed until 0002. `Cc: mesa-stable` so it travels
  with 0002.
- 0002–0004 carry `Cc: mesa-stable`, all four `Assisted-by: Claude Code`; the messages and comments are
  to be rewritten by the submitter.

Points for the MR description: the four bugs and their order of dependence (0001 is what lets
draw-started shadows work on main once 0002 stops the recursion; 0002 exposes bug 3; 0004 needs
0003 for multiple cleared attachments); the validation messages release main gives in `zs`; the reproducer and its modes, before and after;
the `zink_blit()` precedent for 0002; why 0004 populates before reducing instead of restoring
derived state; piglit unchanged; observed in the wild as a browser renderer process crash. Review:
zink is maintained by the author of the commits named above (`769b356b5b6`, `82add9f2e99`,
`635f0c5f4fe`, `b122c3eaa9e`).
