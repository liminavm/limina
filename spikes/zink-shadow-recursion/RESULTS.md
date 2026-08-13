# zink shadow-attachment blit recursion (the Epiphany WebKitWebProcess crash)

**Status: root-caused and FIXED, 2026-08-13.** Upstream mesa bug, still unfixed at
mesa `main` tip `2b7a72457a5`; `zink_render_attachment_shadow` is byte-identical in
upstream, our host `limina-kk` branch and our guest `limina-guest` branch.

## The bug

`zink_render_attachment_shadow` replicate-blits a texture into its hidden MSAA
"transient" image — zink's emulation of `EXT_multisampled_render_to_texture` on a
Vulkan driver without `VK_EXT_multisampled_render_to_single_sampled`, which is every
KosmicKrisp today. The transient is marked `valid` only **after** the blit.

`util_blitter` rebinds the framebuffer, and `zink_set_framebuffer_state` flushes
pending clears whenever the bound attachments change. The old code masked off every
attachment's pending clears across the blit **except the one being shadowed**, so
that flush re-entered `zink_flush_clears → zink_batch_rp → begin_rendering` with the
transient still invalid — and the shadow blit started over. Unbounded recursion until
the stack ran out. `u_blitter`'s "Caught recursion. This is a driver bug." fires every
lap but only logs; it does not break the cycle.

Guest-side exposure: one WebKit process dies (the 2026-08-10 SIGSEGV). Host-side
exposure is worse: a guest GL app reaching this through the vrend tier overflows the
**worker's** stack and takes the whole VM down.

## Reproducing it needs four things, not three

This is the part that cost the time — the first three ingredients are not enough, and
a probe with only those **passes**, which reads as "not reproducible":

1. an attachment with `pipe_surface.nr_samples != 0`. That is specifically
   `EXT_multisampled_render_to_texture` (`att->NumSamples → rb->rtt_nr_samples →
   rb->surface.nr_samples`), **not** a plain MSAA renderbuffer, which carries its
   samples on the resource instead;
2. a **partial (scissored)** clear pending on that same attachment — a full clear
   takes the "skip replicate blit if the image will be full-cleared" early-out;
3. a draw, to open the renderpass;
4. **an invalidated transient.** `unbind_fb_surface` is the only thing that clears
   `transient->valid`, and only when the MSRTT attachment is genuinely unbound. Once
   any earlier pass has populated the transient it stays valid, the shadow path is
   entered but takes `if (transient->valid) continue`, and nothing blits.

Two false negatives on the way to (4), both worth knowing:

- **Binding framebuffer 0 unbinds nothing under surfaceless EGL** — there is no
  default framebuffer, so the bind never reaches `set_framebuffer_state`.
- **A bind/rebind pair with no work between them unbinds nothing either.** The state
  tracker validates the framebuffer lazily, at the next draw or clear. You have to
  actually *use* the other FBO.

A browser compositor unbinds and rebinds layer FBOs constantly, which is why WebKit
hits this and a naive probe does not.

The trace (`zink-shadow-trace.patch`, `LIMINA_ZINK_SHADOW_TRACE=1`) is what settled
it: the shadow path was being entered all along, with `valid=1` every time. Without
that instrumentation the reasonable conclusion was "the path isn't reached", which is
the wrong half of the search space.

## The fix

Mask **all** pending clears across the blit, this attachment's own included, and
restore both `clears_enabled` and `rp_clears_enabled` symmetrically afterwards. With
the mask at zero no flush can fire mid-blit, so the cycle cannot start.

Nothing is lost: `zink_fb_clear_enabled` reads that same mask, so
`fb_clears_apply_internal` early-returns while it is zeroed and the `fb_clears[]`
entries are untouched. The clear is applied by the renderpass that follows, against
the now-populated transient — the order the application asked for, since the clear was
issued after the contents being replicated.

Rejected alternatives:
- **Setting `transient->valid = true` before the blit**, or a bare re-entrancy guard.
  Either stops the recursion by letting the re-entrant flush clear an *unpopulated*
  transient, which the replicate blit then overwrites: a crash traded for silent
  corruption. The pixel checks below exist to catch exactly that.
- **Teaching KosmicKrisp `VK_EXT_multisampled_render_to_single_sampled`.** A real
  improvement (see `limina-kk-feature-gaps`) but a feature, not this fix — and it
  would leave the recursion live for every other driver lacking MSRTSS.

## The oracle

`shadow-recursion.c` + `build-and-run.sh` — host zink-on-KK, surfaceless EGL, no VM
in the loop (the zink code is identical in guest and host builds).

It asserts **pixels**, not just survival. Pass 1 fills red; pass 2 scissor-clears
green and draws blue; pass 3 leaves a clear pending across a framebuffer change; pass
4 invalidates the transient and repeats with magenta. All five sampled regions must
be right afterwards — the preserved red in the middle is the load-bearing one, since
it is only there if the replicate blit really carried the old contents across.

    RED   (before): 243 "Caught recursion" laps and still spinning when killed at 300s
    GREEN (after):  0 warnings, all 5 regions OK, RESULT PASS

Same binary, same command, only the installed zink differs.

## Not covered

The probe only drives the **color** path (`i < PIPE_MAX_COLOR_BUFS`). The
depth/stencil shadow attachment runs through the same rewritten block, and the old
code's zs mask preserved `PIPE_CLEAR_DEPTHSTENCIL` — its own clears — exactly as the
color case preserved bit `i`, so the same cycle and the same fix apply. That is
reasoning, not measurement: treat the zs leg as untested-but-identical-shape.
