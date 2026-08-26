# Card widgets in the GNOME shell lose their contents on the accelerated GL path

A rounded-rect "card" in gnome-shell keeps its background but loses some or all of its text and
icons. Booked in `docs/hardening-backlog.md`.

Canonical symptom, on the notification banner posted by `calib.sh`:

- A **healthy** card shows a header row (small symbolic icon + app name + `Just now`), then a bold
  title, then a large round icon with the body text beside it.
- A **damaged** card is pixel-identical to the healthy one except that **the header row and the
  title are simply not painted**. Background, large icon and body text are all present *at exactly
  the same pixel positions*, and the card's height is unchanged. Nothing is shifted or reflowed.

So the allocation is correct and a contiguous top band of card *content* goes unpainted. Other
observed shapes: glyph outlines rendered shredded/partial, and coloured specks piled at a text row's
left origin.

## What is established

- **Not venus.** Reproduced on a **stock guest** — stock kernel `6.19.10-300.fc44`, stock mesa
  `26.0.3-4.fc44`, no limina guest components. gnome-shell is a GL compositor and GL rides **vrend**
  on both tiers, so venus is not on this path. (The backlog's original "zink→venus→KK→Metal"
  attribution was wrong.)
- **Scoped to the accelerated GL path.** virgl → vrend → zink-on-KK → Metal: **39 damaged / 0
  clean**. `--gpu-software-2d` (guest `-virgl`, llvmpipe, dumb blit): **0 damaged / 40 clean**.
- **Not our host scanout/present path.** The guest's **own** screenshot, taken by gnome-shell inside
  the guest and read off the guest filesystem, shows the identical damage. Whatever goes wrong is
  already wrong in the guest's framebuffer.
- **Not notification-specific.** In one frame of the open clock menu, plain labels drawn straight
  onto the popup background (`Monday`, `August 24 2026`, the weekday letters, every date digit)
  render perfectly, while **every** rounded-rect card in the same popup loses its contents and keeps
  its background: the notification cards, `Today / No Events`, `Add World Clocks…`,
  `Select Weather Location…`.
- **A card can render correctly in one place and damaged in another, but neither placement is
  safe.** The clock popover's notification list has been seen complete while the banner form of the
  same notification was damaged — but the popover is *also* a place the damage appears, in the
  notification cards and in unrelated bubbles alike. Do not build on "the list is the calm copy".
- **It rides a repaint, not first render.** User, watching it live: *"it appears with text, then
  animates to grow into the full bubble completely empty."*
- **gnome-shell breaks, synoik does not.** synoik is a Vulkan/venus compositor, gnome-shell a
  GL/cogl one.

## Incidence is session-unstable — only large gaps mean anything

The virgl damage rate has been measured at 52.5% in one session and ~100% in others, both verified
against pixels. **A small difference between arms is noise.** Every arm below was run in a session
whose baseline was independently measured, and only unmistakable gaps are reported as results.

Seven consecutive 13-20 sample runs of the *same* default arm scored 29, 29, 32, 47, 55, 58, 69 and
77% damaged. That band swallows any partial effect, so at these sample sizes **the only readings
worth taking are cured and not-cured**. A real cure drives the rate to ~0: at even the lowest
observed baseline, 0/19 has probability 0.71^19 ~ 0.002, so one clean run of 19 is strong and a
second makes it conclusive. Never run an arm expecting to interpret a partial shift -- that is how
noise gets written down as a lead.

The human cannot reproduce on demand either: *"I just send notifications, wait around, open and
close stuff until one of them misrenders."* The blocker is incidence everywhere, not a missing
trigger.

**Which card is posted matters more than which session.** Measured back to back in ONE session, on
one host build, both classes pixel-confirmed:

| vehicle | card | damaged |
|---|---|---|
| `calib.sh` | source icon + "Critical Updates" / "Install critical updates as soon as possible" | 8/10 |
| `run-trials.sh` | `notify-send -p 'notifyprobe N' 'MMMM WWWW MMMM N'`, no source icon | **0/32** |

So a run that scores zero is not evidence of a cure until the *same vehicle* is shown to reproduce
in the same session -- 0/32 here was a stock build with the bug plainly live minutes earlier. This
also revises the reading that unique per-trial text is what makes the fault appear: the vehicle that
reproduces uses a *constant* string, and the one that varies text per card does not reproduce at
all. What the reproducing card has that the other lacks is the symbolic source icon, which is
consistent with the icon dying together with the header and title.

**Score an arm with a vehicle whose baseline you measured in the session you are scoring**, and
prefer `calib.sh` for cure/no-cure questions.

## Detection

`calib.sh` posts a notification with a symbolic source icon and measures ink in a strip holding only
the header text — above the large icon, left of the close button. It separates cleanly: **0 when the
header is absent, ~420–450 when present**, both classes confirmed against pixels
(`evidence/banner-header-present-ink452.png`, `evidence/banner-header-missing-ink0.png`). The title
dies with the header every time observed.

`NTITLE` / `NBODY` override the posted strings, because *which* card is posted turned out to be a
variable of the experiment rather than decoration.

**The header strip alone is not enough to score an arm.** It rests on "the title dies with the
header", and any arm that preserves stale pixels breaks it: the header text is *constant* across
cards ("Software", "Just now"), so a header held over from the previous frame is pixel-identical to
a freshly drawn one, while the title — which carries the per-card counter — is not. Under
`KK_LIMINA_FORCE_LOAD` the header detector duly called a clean sweep on cards whose titles were
plainly gone. That says nothing about whether the header was rescued; it says the header strip is
**unreadable by construction** on any content-preserving arm, and only the title score carries
weight there. `score-title.py` measures the title independently from the PNGs every run already
saves -- crop rows 83-96 carry 41-113 lit pixels when the title renders and exactly 0 when it does
not. **Score every arm both ways**; they agree on all the ordering and unroll arms, which is what
makes the one disagreement worth having caught.

A **validity gate** is mandatory: a sample counts only if the BODY strip is inked, proving a banner
was really on screen. Without it every no-banner state — an idle session, the Activities overview a
fresh boot lands in — reads as zero header ink and inflates the damage rate straight to 100%. The
gate has a matching failure mode: **a body string too short to cover the body rect reads as
NOBANNER and voids the arm**, so short-text variants need their own gate.

## Where the fault is NOT

Every arm below ran against an independently measured baseline in the same session.

**Booting the vehicle for this harness requires `LIMINA_GLOBAL_SCANOUT=1`.** Scanout IOSurfaces are
created non-global and passed to the supervisor over a Mach port, so a cross-process probe cannot
`IOSurfaceLookup` them; without the variable `bannerprobe` binds to a stale 1280x800 surface from
early boot and every post scores `NO BANNER`. The validity gate catches this and discards rather
than inflating the rate to 100%, so the failure is loud -- but three separate runs were spent on it.

| arm | knob | result |
|---|---|---|
| baseline | — | 39/0, and separately 19/19 |
| cogl texture atlas | `COGL_DEBUG=disable-atlas` | 39/0 — no effect |
| cogl atlas, both kinds | `COGL_DEBUG=disable-atlas,disable-shared-atlas` | 19/0 — no effect |
| per-StLabel offscreen FBOs | `CLUTTER_PAINT=disable-offscreen-redirect` | persists, changed shape |
| guest driver transfer optimizations | `VIRGL_DEBUG=xfer` | 19/19 — no effect |
| host upload ordering, vs prior work | `LIMINA_VREND_TRANSFER_FORCE_SYNC=1` | 17/19 — no effect |
| host upload ordering, vs consuming draw | `LIMINA_VREND_TRANSFER_SYNC_AFTER=1` | 12/13 — no effect |
| damage region / clip / buffer age / culling | `CLUTTER_PAINT=disable-clipped-redraws` | 13/13 — no effect |
| zink completion-bookkeeping races made atomic | mesa `a0d96c18f02` | 18/19 — no effect |
| vrend's threaded sync removed entirely | `VIRGL_DISABLE_MT=1` | 16/19 — no effect |
| zink command-stream reordering | `ZINK_DEBUG=noreorder` | 19/19 — no effect |
| cached glyph VBO rebuilt every draw | `LIMINA_TEXT_NOCACHE=1` | 11/11 — no effect |
| software-2D | `--gpu-software-2d` | **0/40 — cures** |

- **Not a data race between virglrenderer and zink.** ThreadSanitizer over the host zink +
  KosmicKrisp stack reports 19 races on a plain boot, and the suspicious ones sit on exactly the
  seam this bug's symptoms suggested: virglrenderer's fence thread makes a *second, shared* GL
  context current and calls `glClientWaitSync`, so it walks into zink's screen-level completion
  bookkeeping while zink's own submit thread is in it. Two independent arms kill the theory:
  making that bookkeeping atomic changes nothing (18/19), and *deleting the sync thread outright*
  changes nothing (16/19). The second arm is the decisive one -- it removes the whole seam, not one
  field of it.
  The arm is self-evidencing: with threaded sync off there is no poll eventfd, so nothing pumped
  `virgl_renderer_poll()` and no fence ever retired -- the window froze on the last pre-desktop
  frame. It only produced a desktop once the worker pumped the poll itself, which it only needs to
  do when the sync thread is genuinely absent. A run that presents a desktop under this flag has
  therefore proved the thread is gone.
  The races are real undefined behaviour and have been fixed on their own merits -- 23 reports down
  to 1, see `docs/hardening-backlog.md` -- but they are not this bug. The fixed stack was re-measured
  here and the damage rate did not move.
- **Not zink's command-stream reordering either.** Zink hoists unsynchronised uploads into a
  reordered command buffer that runs ahead of the batch's draws, which would explain a cached VBO
  being read before its upload lands -- and any explicit sync curing it, by forcing a batch
  boundary. `ZINK_DEBUG=noreorder` disables that wholesale: 19/19 damaged, the top of the band.
  Verified present in the worker's own environment before scoring, not just exported.

- **Not the cogl glyph atlas**, shared or otherwise.
- **Not the offscreen-redirect FBO path.** With the redirect disabled the text is drawn
  direct-to-framebuffer and *still* corrupts, as shredded dark glyph fragments shaped like the words
  (`evidence/no-redirect-body-glyphs-shredded.png`). Text that never goes through an FBO still
  corrupts, so the FBO is not where content is lost. (This knob also suppresses banners, so it gives
  a qualitative result only, never a rate.)
- **Not the guest virgl driver's opportunistic transfer optimizations.** `VIRGL_DEBUG=xfer` disables
  the uninitialized-range fast path, the discard→staging path, and `buffer_subdata` queue-extend
  (`virgl_resource.c:201,213,979` in mesa). It changes nothing. Note this is a **partial**
  exoneration: `wait = !(usage & PIPE_MAP_UNSYNCHRONIZED)` is not gated by that flag, so an
  *explicit* unsynchronized map from cogl still reaches vrend's mid-buffer unsynchronized branch.
- **Not vrend-level upload ordering, in either direction.** Two levers on the virglrenderer fork,
  each verified engaged via its stderr marker in the worker log. `LIMINA_VREND_TRANSFER_FORCE_SYNC=1`
  `glFinish()`es **before** every `vrend_renderer_transfer_write_iov` and forces a plain synchronized
  `GL_MAP_INVALIDATE_RANGE_BIT` map, bypassing the orphan/unsynchronized heuristic — that orders a
  transfer against *prior* GL work. `LIMINA_VREND_TRANSFER_SYNC_AFTER=1` `glFinish()`es **after** it —
  that orders it against the *subsequent consuming draw*. Damage survives both. Note the second lever
  exists because the first alone proves much less than it appears to: GL guarantees upload→draw
  ordering within the API, but here that guarantee is implemented by zink's barrier tracking and
  KosmicKrisp's Metal encoding, so it is a real suspect that no pre-transfer sync can touch.
- **Not the damage region, clip stack, buffer age, partial swap, or actor culling.**
  `CLUTTER_PAINT=disable-clipped-redraws` (`clutter-context.c:72`, honored at `meta-stage-impl.c:456`
  in mutter 50) forces a full-view repaint every frame, removing damage regions, buffer-age unions,
  the stencil region clip, and effectively all on-screen frustum culling. 13/13 damaged.

### Host-side: zink and KosmicKrisp

Once the host split convicted zink/KK (below), the same discipline was applied inside them. Every
arm is self-evidencing -- it prints its own engagement to stderr in the worker log -- because an
arm that silently fails to engage is indistinguishable from a clean exoneration.

| arm | knob | result |
|---|---|---|
| zink copy barriers | `ZINK_DEBUG=sync` | 13/13 — no effect |
| zink scheduling, wholesale | `ZINK_DEBUG=sync,noreorder,norp,nogeneral` | 13/13 — no effect |
| KK barrier pass-restart | `KK_LIMINA_BARRIER=norestart` | path never runs (see counters) |
| all cross-command-buffer ordering | `KK_LIMINA_SERIALIZE=1` | 9/13 — no effect |
| poly-heap reset aliasing | `KK_LIMINA_HEAP_NORESET=1` | 9/19 — no effect |
| geometry unrolling, wholesale | `LIMINA_ZINK_NO_FANS=1 KK_LIMINA_NO_PROMOTE=1` | 11/20 — no effect |

- **Not zink's scheduling.** `sync` puts a full `VkMemoryBarrier` around every copy; the combined
  arm additionally disables command reordering, renderpass tracking and GENERAL-layout use. No
  effect, so zink's request side is out.
- **Not a race between command buffers.** Metal 4 removed automatic hazard tracking, and KK submits
  an upload (`pre_gfx`) separately from the draws that consume it (`gfx`), so this was a live
  suspect. `KK_LIMINA_SERIALIZE` chains every command buffer on a queue event, making concurrent or
  out-of-order execution impossible. Damage survives. Note precisely what this does and does not
  settle: it rules out a *race*, not a deterministic mis-ordering, since it enforces the existing
  submit order rather than questioning it.
- **Not the pass-restart paths.** KK tears down and restarts a live render pass in two places --
  `kk_CmdPipelineBarrier2` when a gfx encoder is open, and `kk_flush_render_pass` on a
  colour-attachment-map or sample-location change. A restart that failed to reload would lose
  exactly what this bug loses. Counters say **neither ever fires** in a gnome-shell session:
  `restarts=0/0`. Nothing to exonerate.
- **Not the geometry-unroll path, in whole.** Every compute dispatch KK hoists ahead of an open
  render pass is geometry unrolling (2164 fans + 1505 index promotions; `dladdr` attributes all of
  them to `kk_unroll_geometry` and nothing else). It also converts direct draws to indirect and
  stages indices through one device-wide heap, and llvmpipe -- the clean control -- has no
  equivalent. Rather than test its hazards one by one, the path was **drained**: with fans lowered
  by zink, uint8 index buffers refused, and restart-disabled promotion skipped, the counters read
  `fan=0 promote=0 robust=0 restart=0` and the damage is unchanged. Unrolling, the indirect
  conversion, the shared index heap and the hoisting are all out together.
- **Not the poly heap.** With its reset suppressed for a whole session the bump allocator reaches
  276080 bytes of 134217712 (0.21%) and never trips its overflow abort, so allocations cannot be
  wrapping onto each other; and making every allocation unique does not move the damage.

- **Not content discarded at pass start.** With ordering exhausted, the remaining host-side
  suspect was what a pass *encodes*: a render pass beginning on a texture that already holds
  drawing, with a load action of CLEAR or DONT_CARE, throws that drawing away. A detector in
  `cs_start_render` (a never-emptied set of texture pointers, checked against the load action
  actually written into the descriptor) finds 155 such starts per session, 59 of them on
  attachments small enough to be a label or icon offscreen. Firing is not guilt -- clearing a
  reused offscreen before redrawing it is ordinary -- so it was turned into an arm:
  `KK_LIMINA_FORCE_LOAD=small` makes every small drawn target load instead. **32% damaged, no
  effect** -- and confirmed engaged after the fact from the same run's log (`FORCED to LOAD on small
  drawn targets`, `reload_hazard=160 (small=64)`), so 64 discards really were converted to loads.
  One hole it cannot close: a *fresh* texture is not in the seen-set, so a first-pass offscreen is
  never forced -- and it cannot be, since loading an undrawn target reads uninitialized memory. (The unrestricted `=1` form is void, not a cure: with card backgrounds no longer
  cleared, previous frames pile up inside them and the leftover ink scores as healthy. It reads
  0/20 on both detectors while the pixels show the same title drawn twice over.)

One real bug was found on the way and is **not** this one: the force-LOAD loop in
`kk_CmdBeginRendering` indexes the Metal descriptor by the raw Vulkan attachment index while every
other site uses `dyn->cal.color_map[i]`, so a non-identity map would write LOAD to the wrong slot
and a restart would discard earlier drawing. Measured `color_map_nonidentity=0/5448` -- the map is
always the identity here, and restarts never fire, so this workload cannot reach it. Latent, worth
fixing, unrelated.

**Partial effect, not a cure:** disabling GNOME animations
(`gsettings set org.gnome.desktop.interface enable-animations false`) raised the clean rate from
2/19 to 5/12. Consistent with the fault riding the insertion/expand repaint, but it does not
eliminate it.

## Synthetic vehicles that failed to reproduce

Two hand-written surfaceless GLES3 programs, run in the guest on the virgl path. Both are committed
because a negative result on a faithful-looking imitation is itself a conclusion: **the essential
ingredient of the real workload is not yet imitated.**

- `texupload.c` — small R8 texture, banded `glTexSubImage2D` sub-rect uploads, texture churn through
  a slot pool, verification deferred by several iterations so a race has room to land.
  **0 mismatches in 3976 checks.**
- `bufstream.c` — one reused streaming VBO written with
  `GL_MAP_WRITE_BIT | GL_MAP_FLUSH_EXPLICIT_BIT` plus `GL_MAP_INVALIDATE_BUFFER_BIT` at offset 0 /
  `GL_MAP_UNSYNCHRONIZED_BIT` elsewhere (mimicking cogl's journal), rendering colour-coded cells with
  no per-iteration sync. **0 wrong in 20 passes × 1024 cells.**

## Vehicles for isolating the real code, and how they fail

- **`LIBGL_ALWAYS_SOFTWARE=1` on gnome-shell is unavailable.** The shell **SEGVs in
  `dri2_drm_swap_buffers()`** — mutter's gbm/KMS renderer cannot take swrast here — and gdm loops
  until it gives up. The guest-llvmpipe-with-our-scanout split cannot be run this way.
- **apitrace on the session gnome-shell does not work.** `LD_PRELOAD` is inherited by **Xwayland**,
  which the shell spawns; both processes then write the same `TRACE_FILE`, Xwayland aborts, and the
  trace freezes. Scoping the drop-in per systemd instance does not help — the collision is
  parent→child. A `--no-x11` vehicle avoids it.
- **`gnome-shell --nested` no longer exists in 50.** Nested is the *default*; `--display-server`
  opts into being the real one. But launching a second shell without `--headless` still contended for
  the display (a `MUTTER_DEBUG_DUMMY_MODE_SPECS` mode change lands on the real scanout) and took the
  outer session down with it. Use **`--headless --virtual-monitor WxH --no-x11`**, which contends
  with nothing, can screenshot itself, and is the one process to `LD_PRELOAD`.
- **The headless shell runs, but as an oracle it is inert until something drives its frame clock.**
  `nested-start.sh` / `nested-run.sh` bring it up on a private bus and it accepts notifications, but
  **a headless virtual monitor with no consumer paints once at startup and then never again**:
  measured, 6 screenshots over 4.2 s after a `Notify` were pixel-identical and the banner never
  appeared, and 4 sampled posts all returned the same startup frame. Every sample scores clean, so
  the freeze reads exactly like a cure -- treat any clean result from this vehicle as void until a
  frame driver is proven live. Attaching a screencast does unfreeze it (the banner appears), which
  is how headless GNOME really runs, but the recorder then fails PipeWire format negotiation
  (`no more input formats`) and the paints stop again. **Unfinished; not needed for instrumenting
  our own code**, since a `printf` compiled into libmutter works in the session shell and only
  `LD_PRELOAD` tools require this vehicle.
- Two traps worth keeping from building it: the shell's `Screenshot`/`Screencast` D-Bus methods are
  **sender-gated**, and a caller that owns an allowlisted name (`org.gnome.Screenshot`) is still
  refused if it calls immediately -- the shell resolves those names asynchronously, so **sleep after
  `RequestName`** or a race reads as a hard permission wall. And `pkill -f 'gnome-shell --headless'`
  matches the ssh command line that contains that string, killing its own wrapper; anchor with `^`.

## Where this leaves the fault

**The label FBOs are painted, and their content is wrong.** That is forced by the guest screenshot.
gnome-shell's screenshot goes through `clutter_stage_paint_to_buffer` (`src/shell-screenshot.c:339`),
which builds a paint context with no view, so `clutter_paint_context_is_drawing_off_stage()` is TRUE
(`clutter-paint-context.c:218-224`) and `cull_actor()` bails without culling
(`clutter-actor.c:3227-3235`) — no damage region, no clip, no buffer age. A label that had merely
been *skipped* on screen would have a NULL offscreen and would repaint fresh and correct there. It
came out damaged, so it had been painted, into an FBO whose content is wrong
(`clutter-offscreen-effect.c:569-575` composites the cached texture for a non-dirty actor, which is
also why a bad frame leaves the widget blank until something re-dirties that label).

A tempting reading — "a contiguous top band is unpainted, so something clipped the repaint" — is
therefore **wrong**, and the `disable-clipped-redraws` arm confirms it empirically. A pixel-level
clip would mask the card background exactly as it masks the text, and the background is present.

The best available reading is **late, not corrupt**: the texels are fine once they have landed, and
what fails is sampling them too early, in the same churn frame that rasterizes and uploads them. It
unifies all three damage shapes — nothing uploaded yet → zero coverage → absent row; partially
landed → shredded fragments; stale allocation bytes → coloured specks at origin — and it explains why
disabling animations helps without curing (fewer frames between rasterize and sample), and why every
damaged card is one that was freshly built or animated in the frame that damaged it.

**It is weaker than a "calm copy renders fine" argument would make it look.** The same notification's
text has been seen complete in the clock popover's list while its banner form was damaged, which
invites the inference that quiet repaints are safe and glyph texels are therefore fine at rest. That
inference is **not supported**: the popover is also a place the damage appears, in notification cards
and unrelated bubbles alike. So "damaged only during churn" is a description of what has been
observed, not an established property, and glyph-texel corruption at rest has not actually been
excluded.

Since vrend-level ordering is covered in both directions, the remaining suspects are **below vrend**:
zink's staging-copy → sampling-draw barrier tracking and the render-to-texture → sample path for the
label FBO, over KosmicKrisp's Metal encoding. This host has prior form for exactly this fault class —
the WebRender tile-displacement tear (`vrend_renderer.c:9685-9705`, "Reproduced through
guest→virgl→zink→KK; clean host-direct") and the IOSurface transfer fence gap
(`vrend_renderer.c:9979-10000`).

## The host-implementation split: zink/KosmicKrisp is convicted

The locus split has been **run**. Host mesa rebuilt with `-Dgallium-drivers=zink,llvmpipe` into
`/Volumes/mesa-cs/zink-llvmpipe-prefix`; the worker selects between them with
`LIMINA_HOST_GALLIUM` (`boot-enhanced-efi-kk.sh`). Identical guest, identical virgl protocol,
identical vrend GL stream, identical driver script and oracle — the *only* difference is the host GL
implementation underneath vrend.

| host GL | damaged | clean |
|---|---|---|
| zink → KosmicKrisp → Metal | **4** | 4 |
| llvmpipe | **0** | 16 |

Same card in both arms (GNOME's own "Screen Capture / Screenshot captured" notification, which the
screenshot oracle raises), scored identically by `score-guest-shots.py`, driven by
`guest-shot-run.sh`. Evidence: `evidence/hostsplit-zinkkk-header-title-lost.png` versus
`evidence/hostsplit-llvmpipe-same-card-clean.png`.

**The fault is below vrend, in zink/KosmicKrisp.** Everything above it — the guest virgl driver, the
virgl command stream, vrend's own transfer handling — is held constant across those two arms and
produces a correct image on one of them.

Two things make this an apples-to-apples comparison rather than the asymmetric one it could easily
have been. Both arms use the **guest's own screenshot** as the oracle, not the host scanout probe:
llvmpipe cannot run with our IOSurface-backed scanout at all (it asserts in `init_scene_texture`
when the map fails), so that arm needs `LIMINA_VREND_IOSURFACE=0` and has no host window to probe.
And both arms score the **same card**, because the screenshot oracle's own notification tends to own
the banner slot in both.

**Caveat, stated plainly:** llvmpipe is much slower, so some of its cleanliness may be timing
suppression rather than correctness. The result is strong evidence, not proof. It is not
symmetrical, though — the *damaged* half is unambiguous.

## The compositor build: the damaged draw is submitted, and a GL flush cures it

mutter 50.0 built from the exact Fedora source the guest runs (`mutter-50.0-1.fc44`, which carries
**zero** distro patches, so source and running binary match), instrumented, installed over the stock
libraries and verified loaded out of `/proc/PID/maps`. Scripts: `install-mutter-50.sh` (install +
prove-loaded), `glyph-arm.sh` (select an arm), patch: `mutter-glyph-instrument.patch`.

**State the confounder first: this build damages at 92-100%, against 29-77% for stock Fedora
mutter.** Same fault by pixels and same shapes, and a near-certain reproduction is what makes a
single run decisive at last -- but it is a changed system under test, and every rate below belongs
to this build, not to the shipped one.

| arm | what it forces | title | body |
|---|---|---|---|
| baseline (29 samples over 3 arms) | nothing | **damaged 29/29** | intact |
| `LIMINA_GLYPH_SYNC` | glyph texels read back at the UPLOAD site | damaged 15/15 | intact |
| `LIMINA_TEXT_SYNC=journal` | cogl journal flushed after the label draw | **clean 10/10** | **shredded 10/10** |
| `LIMINA_TEXT_SYNC=flush` | journal flush + driver flush (sync fd + `glFlush`) | **clean 10/10** | **clean 10/10** |
| `LIMINA_TEXT_SYNC=flush` + `LIMINA_COGL_NO_SYNCFD` | journal flush + bare `glFlush`, no sync fd | **clean 10/10** | **clean 10/10** |
| `LIMINA_TEXT_SYNC=sleep` (5 ms) | nothing; waits only | damaged 9/9 | intact |
| `LIMINA_TEXT_READBACK` | full readback of the label offscreen | clean 6/6 | clean 6/6 |

- **The damaged text IS drawn.** Every damaged title logged its own draw, twice each, 10 of 10.
  This is the split the whole investigation needed: the fault is not a missed paint, a cull, or an
  actor left unset. It is submitted work that does not survive.
- **Not the glyph texels.** Glyphs are rasterized and uploaded from `clutter_text_create_layout`
  (not from the paint path -- a paint-site probe read zero and would have exonerated the wrong
  thing). Forcing a blocking readback so the texels have provably landed before any draw -- evidenced
  per batch, `sync readback: 1664 byte(s)` -- leaves the damage at 15/15. The atlas is also not the
  variable: the body label draws from the same atlas and renders correctly in the same frame.
- **Not offscreen redirection as such.** Title and body BOTH draw into fresh per-card offscreens
  (968x44 and 568x44). Redirection cannot be the discriminator when the intact half is redirected too.
- **A GL flush after the label offscreen is drawn cures it.** `cogl_framebuffer_flush()` is not a
  CPU wait and not a readback: it flushes the journal and calls the driver flush
  (`_cogl_context_update_sync` + `glFlush`). Splitting the two halves is what localizes the fault --
  the journal half alone cures the title and leaves the body shredded, and only adding the driver
  half cures both. **Rendered content is not visible to the draw that samples it unless a GL-level
  flush is forced.** GL orders commands within a context on its own, so needing one is a driver
  fault, which is exactly where the llvmpipe-versus-zink/KK host split already pointed.
- **It is not lateness.** Every curing lever also delays, so "heavier lever, more cure" reads equally
  well as "any pause lets the host catch up" -- and this bug has looked late from the first sample.
  The rung that separates them waits without submitting anything: a 5 ms `g_usleep` at the same
  call site, ten times the cost of the flush it replaces and self-evidenced as engaged (`mode=sleep
  journal_depth=18`, the same depth the curing arm reports), cures **nothing**, 9 of 9. Delay is not
  the active ingredient; the flush boundary is.
- **The sync fd is not the active ingredient either.** Dropping `_cogl_context_update_sync` and
  leaving a bare `glFlush` still cures both halves, 10 of 10. So no host round-trip and no fence is
  required -- **ending the command batch is sufficient**, which is a far more specific accusation:
  zink tracks render-target-to-sample hazards per batch, and a forced flush ends the batch and emits
  the transition that a mid-batch path is missing. It also explains why `ZINK_DEBUG=sync` never
  moved this: that barriers copies, not render-target-to-sample transitions.
- Ordering the ladder weakest-first is what made it readable: the heavy readback arm cures too, but
  on its own it proves nothing about which layer owns the bug.

**Do not read the `journal` arm as void.** It reports NOBANNER on every sample, because the validity
gate proves a banner exists by measuring BODY ink -- and that arm is precisely the one that damages
the body. The gate cannot see a card whose gating element is the damaged one; the samples were
scored from the saved PNGs instead (title 432-1144 present, body 92-279 against 960-1493 intact).

## Next

The fault now has a shape and a layer: **content rendered into a freshly created offscreen is not
visible to the draw that samples it, in the same frame, unless the command batch is ended.** Not a
fence, not a wait, not a delay -- a submission boundary, and nothing weaker. The
compositor is exonerated of everything except being the thing that hits it -- the draw is submitted,
the glyphs are uploaded and complete, the actor state is right.

### The synthetic reproducer does not reproduce (`rtsample.c`)

Built to the shape the ladder ended on: a fresh texture+FBO per iteration, rendered into (a blue
clear plus a red quad over the middle half, so a failure says *which* of the two was lost), sampled
1:1 into a persistent grid in the same batch, and verified by a **single** readback after all 256
iterations -- deferred because a per-iteration readback would supply the very batch boundary being
tested for. It runs on the real stack (`virgl (zink ... MESA_KOSMICKRISP)`), and it self-tests: the
`RTS_FINISH` arm reports 256/256 correct, so the oracle can say "clean" for the right reason.

**Every arm is clean, 256/256**: baseline, `TEXDRAW` (the offscreen pass also *reads* a texture,
blended, as a glyph draw does), `REBIND` (the offscreen written by two render passes with real work
on a third target in between), `BLEND` (blended composite), and all three together.

So the fault is **not** reachable from render-to-fresh-offscreen-then-sample alone. Something in the
real case is still missing from the mock, and guessing further ingredients one at a time is the
wrong move -- the mock cannot be trusted to have failed for the right reason. Keep the vehicle: it
is a fast negative control, and it will be the regression test once the fault is understood.

### It is not only the text: the icon dies with it

The damaged card loses the header line, the **app icon** and the title together, and keeps the body
(`evidence/card-damaged-header-icon-title-all-gone.png` against
`evidence/card-intact-header-icon-title.png`). An icon is not a glyph, is not laid out by pango and
never touches the glyph cache, so whatever this is, it is not about text. It is about which *small,
freshly created* offscreens survive to be sampled. Every remaining theory has to explain the icon.

### Host arms: zink's reordering and renderpass tracking are NOT it

`ZINK_DEBUG` exposes the two host optimizations that could plausibly need a flush to come out right
-- `noreorder` ("do not reorder command streams") and `norp` ("disable renderpass
tracking/optimizations", which is where load/store ops are chosen). Set on the worker and proved
present in its environment with `ps -E`, against the instrumented build's ~93% base rate:

| host arm | damaged |
|---|---|
| control, `ZINK_DEBUG` unset | 13/14 (93%) |
| `norp` | 13/13 (100%) |
| `noreorder` | 15/15 (100%) |
| `noreorder,norp` | 14/15 (93%) |

Neither switch moves it. An earlier `noreorder,norp` run measured 7/11 (64%) and is **void, not a
lead**: `ydotoold` was dead for it, so that vehicle had no synthetic input at all while every other
run did. It was re-run with input restored and came back at 93%, matching control. A partial-looking
effect that survives only in the run with a broken vehicle is a vehicle artifact.

The invalidate/discard theory died before it cost a boot, in two greps: cogl calls
`cogl_framebuffer_discard_buffers` from exactly one place, `cogl-onscreen.c`, so a label offscreen is
never discarded; and vrend does not relay `glInvalidateFramebuffer` at all, so no guest invalidate
could reach host GL even if one were made.

### The guest's issue order is correct: the fork resolves to the host

The probe: a global counter and a `printerr` in `_cogl_framebuffer_flush_journal`, logged **after**
`_cogl_journal_flush` returns rather than before -- load-bearing, because a flush first flushes its
dependencies, so a dependency's flush is *nested inside* its parent's and logging on entry prints
the exact inversion of what GL sees. Plus a line at the dependency-registration site in
`cogl-journal.c`, since a dependency that is never recorded cannot be flushed first. Armed by the
title draw itself and self-limiting, so the window sits on the card being measured.
Log: `evidence/journal-issue-order.log`.

Both halves come back correct, on every card:

```
[LIMINA-TEXT]  draw "Critical Updates 005" fb=0xaaaaf251c520 OFFSCREEN 968x44
[LIMINA-JDEP]  fb=0xaaaaef033b50 2560x1440 depends on 0xaaaaf251c520 968x44
[LIMINA-JFLUSH] seq=121 fb=0xaaaaf251c520 OFFSCREEN 968x44   entries=18
[LIMINA-JFLUSH] seq=125 fb=0xaaaaef033b50 onscreen 2560x1440 entries=35
```

The stage **does** record the title offscreen as a dependency, and the title's 18 journalled entries
**are** issued to GL before the stage's 35 that sample them. The clean card and the damaged card
produce byte-identical orderings (`seq=1..5` on the intact post 001, `seq=121..125` on the damaged
post 005), so issue order is not the discriminator and cogl is exonerated of mis-ordering.

That was the last guest-side explanation the ladder left open, and with it goes the reading that the
journal-flush cure worked by fixing an order. **The same work is submitted, in the same order, in
both cases; the damaged card loses its content below the guest's GL stream.** The host split is now
earned by this measurement rather than inherited from the old llvmpipe run.

### What dies is what was journalled this frame -- the survivor never flushes

Sitting in the same log, unlooked-for. Across all 8 posts, the four elements that die each flush a
journal 15 times -- title `968x44` (18 entries), `Software` `134x44` (8), `Just now` `110x38` (7),
the `38x38` icon (1). The body's `568x44` offscreen **never flushes with entries anywhere in the
log**, 0 occurrences, and the body is the one element that survives. A perfect anticorrelation
between "had draws issued in this window" and "lost".

**One thread in it is not yet explained and the correlation should not be leaned on until it is:**
the body's draw *is* logged every frame, so its journal ought to hold entries at flush time and does
not. Where they go -- flushed between the draw and the window, or never journalled -- has to be
answered before this is more than an observation.

It does sharpen two things regardless. The host question is not "does the texture hold ink at frame
end" but *content rendered this frame versus content retained from earlier frames*. And it hands
`rtsample.c` the ingredient it lacks: the mock composites 256 sources that were all freshly rendered
in the same batch, and never mixes same-frame-rendered sources with retained ones the way a real
card does.

### The KK instrumentation we own tracks loads, and skips exactly this case

`kk_cmd_buffer.c` already classifies every render-pass start (`fresh` / `seen+LOAD` /
`seen+CLEAR` / `seen+DONTCARE`) and carries the `KK_LIMINA_FORCE_LOAD` arm, which was measured and
did not cure. Two gaps matter now:

- **`fresh` attachments are counted and then skipped** (`continue`), and a freshly created label
  offscreen is precisely a `fresh` attachment. The one bucket the title lives in is the one the
  detector stops looking at.
- **Nothing tracked store actions.** `kk_apply_attachment_store_ops` took `force_store` as a
  parameter, so there was no env arm for it -- and content lost at pass *end* was the shape still
  unaccounted for, since a guest-side flush moves where passes end.

Both gaps are closed (mesa `limina-kk`, `kk: arm the store side, and stop dropping fresh
attachments`), and the store arm answers:

| KK arm | what it forces | damaged |
|---|---|---|
| `KK_LIMINA_FORCE_LOAD=small` | pass START loads instead of clearing/discarding | no cure |
| `KK_LIMINA_FORCE_STORE=1` | pass END stores instead of discarding | **11/11, no cure** |

**Both ends of the render pass are now exonerated.** Nothing is being discarded that should have
been kept, at either end. Whatever loses the content is not a load or store action, which also means
it is not the `DONTCARE` counter that has been sitting nonzero in the worker log all along.

### The host texture holds the text -- and the stage composites it while it is empty

`LIMINA_VREND_INK=968x44` (virglrenderer `limina`, `vrend: read a host texture's ink at the moment a
draw samples it`) reads the texture back at the instant a draw binds it to sample, from inside
`vrend_draw_bind_samplers_shader`. Log: `evidence/host-ink-at-sample.log`.

Three details are what make it readable, and two were corrections:

- **Read twice, with a `glFinish` between.** An empty first read means either "the draws have not
  executed yet" or "the readback raced", and those are opposite conclusions.
- **Report a checksum, not just a count.** Ink alone cannot distinguish fresh content from a
  previous card's text sitting in a recycled texture.
- **Rate-limit on the wall clock, not on texture id.** Deduplicating per id looked obviously right --
  every card gets a fresh offscreen -- but the guest recycles GL texture ids hard, so the id-keyed
  version fired for the first few cards and went silent for the rest. It read like a general result
  from three samples.

The probe is a perturbation and is only readable next to the damage rate from the same run. **All 9
cards came out damaged, so it did not cure and the reading stands.**

```
sample tex=83 968x44 ink=3152 after_finish=3152 sum=ee68cd6e -> drawing into 2560x1440
sample tex=83 968x44 ink=0    after_finish=0    sum=811c9dc5 -> drawing into 2560x1440
sample tex=84 968x44 ink=3158 after_finish=3158 sum=d00580b0 -> drawing into 2560x1440
sample tex=84 968x44 ink=0    after_finish=0    sum=811c9dc5 -> drawing into 2560x1440
```

- **The content exists on the host, and it is this card's.** ~3000 inked texels of 42592 is a line
  of text, and the checksum is different for every card -- so it is freshly rendered content, not a
  recycled texture still holding the previous title.
- **The empty reads are real.** `after_finish` equals `ink` on every single line, so a texture that
  reads empty is genuinely empty and fully synchronized, not a racing readback.
- **Both kinds of sample are the stage.** Every line says `drawing into 2560x1440`. This is not some
  other consumer's texture: the visible composite is the thing sampling an empty label.

So within one card the stage composites that label texture more than once, and at some of those
composites it holds the title while at others it holds nothing -- and what reaches the screen has no
title. The render is not the failure; **the failure is that the composite that matters samples the
texture while it is empty.**

### It is the SECOND render into the same offscreen that produces nothing

Logging the render target at the draw entry point -- and reporting every *switch* to a 968x44
target, not just every change of id -- completes the sequence.
Log: `evidence/host-second-render-empty.log`.

```
RENDER TARGET tex=84 968x44                       <- the guest renders the title into 84
sample tex=84 ink=3152 after_finish=3152 ...      <- a composite sees the title
RENDER TARGET tex=84 968x44                       <- the guest renders into 84 AGAIN
sample tex=84 ink=0    after_finish=0    ...      <- the next composite sees NOTHING
```

- **Identity is clean.** The target rendered into and the texture sampled are the same GL object.
  Aliasing is out; nothing is written to one object and read from another.
- **The first render lands and the second does not.** Same target, same 18 draws -- the guest's
  draw probe logged the title twice per card all along, and the journal flushed 18 entries both
  times. On the host the first pass fills the texture and the second leaves it empty, verified
  after `glFinish`.

Two placement bugs had to be fixed to see this, and each produced a confident wrong reading first.
The render-target check began inside the sampler probe, which only runs for sampler views that are
**dirty** -- so it never fired once, which reads exactly like "the label is never rendered into a
968x44 target" and is instead a dead probe. It then deduplicated on "the id changed", which hid the
repaint of the same target: precisely the event that turned out to be the answer.

**`rtsample` still does not reproduce it.** An `RTS_TWICE` arm that renders each offscreen, samples
it, then renders the same target again and samples again comes back 256/256 clean -- so
"render the same target twice" is necessary to the real case but not sufficient on its own. The mock
is still missing something, and this is recorded as a negative rather than tuned until it breaks.

### Both render episodes carry the same draws -- and a false cure, caught by the pixels

Counting the draws vrend processes per render episode on the label target closes the "did the work
arrive" question: **both episodes report `draws=1`.** The guest's 18 journal entries batch into one
GL draw, and the second pass carries it just as the first does. The work reaches the host in both
passes; the second one writes nothing.

That reframes `ink=0` as well. The label offscreen is cleared to transparent, so an empty texture is
not "nothing happened" -- it is **cleared, and then not drawn**.

Which made `KK_LIMINA_FORCE_LOAD=small` worth re-running against the current oracle, since
suppressing the clear should leave the first pass's content in place. It measured **7/7 clean on the
header strip -- and every one of those cards had no title.** The pixels say what happened: the
header line and the icon came back, the title did not.

**This is the trap this document already warned about, walked into anyway.** The header text is
identical on every card, so any arm that preserves a reused texture's contents makes the header
reappear and scores as a cure while the element that actually varies is still missing. The mechanism
is now clear too: `FORCE_LOAD` only acts on the `seen` path, and the header and icon offscreens are
reused across cards (so their preserved content is indistinguishable from fresh) while a per-card
title offscreen is `fresh` and skipped entirely. The arm preserves stale pixels; it does not cure.

**The rig is fixed rather than the reading just corrected.** `calib.sh` now scores a TITLE strip
(`1120,100,480,28`), validated two-point against the known cure: 202 ink with
`LIMINA_TEXT_SYNC=flush`, blank on a damaged card. The title is the only element whose text changes
per post, so it is the only one that can tell a card that rendered from one showing preserved
pixels. Every arm from here is scored on it; the header stays as a secondary signal.

### The failing draw rasterises nothing

An occlusion query around the label draw, read at episode end where a readback is already known not
to cure, settles what the second pass does. `GL_SAMPLES_PASSED` is rejected by this context; the
GLES3 boolean form works.

```
episode END tex=84 draws=1 ink=3145 ... | source tex=4 512x512 ink=22680 | samples=1
episode END tex=84 draws=1 ink=0    ... | source tex=4 512x512 ink=22680 | samples=0
```

**`samples=0`.** No fragments are produced at all, so this is not blending, not a write mask and
not a store -- the geometry never covers a pixel. And everything observable about the two draws is
identical: same target, same draw count, same program, same viewport, same blend and mask state,
and the same glyph atlas with byte-identical ink. The program has **no matrix uniform**, so cogl
bakes the transform into the vertex data -- which means the geometry lives entirely in the buffer
the draw fetches.

### What is exhausted

| arm | engagement proof | result |
|---|---|---|
| `KK_LIMINA_FORCE_LOAD=small` | announces itself | header/icon return, **title does not** -- preserves stale pixels, not a cure |
| `KK_LIMINA_FORCE_STORE=1` | announces itself | no cure, 11/11 |
| `LIMINA_VREND_SCISSOR_FIX=1` | announces itself | no cure; failing draws run with a correct box |
| `LIMINA_VREND_TRANSFER_FORCE_SYNC=1` | announces itself | **no cure** -- by its own documented semantics, host transfer ordering is not what produces this |
| `ZINK_DEBUG=noreorder` / `norp` / both | `ps -E` | no cure |
| `rtsample` incl. `RTS_VBO`, `RTS_TWICE` | self-tests | 256/256 clean on every arm |

**Reading the geometry from the host is a closed avenue.** `glGetBufferSubData` on the bound
attribute buffer killed the worker with SIGABRT; copying into a scratch buffer we own with
`glCopyBufferSubData` first killed it too. Two methods, same signal 6.

**Everything that synchronises cures.** A guest-side `glFlush` at the label draw, a full readback of
the label offscreen, and -- discovered by accident -- a 512x512 `glReadPixels` of the glyph atlas
*before* the draw, which cured 4/4 and had to be moved to episode end to keep the bug alive. This is
consistent and it is also why no sync-based probe can localise further.

**One control is NOT established.** `LIMINA_HOST_GALLIUM=llvmpipe` renders 6/6 cards correctly, twice
over, but the vrend probe emitted **zero** lines in those runs -- the instrumented path never ran, so
that arm cannot be read as "the same stack minus zink". It is recorded as unresolved, not as the
layer control it was meant to be.

### Inside KosmicKrisp: encoded, bound and scissored correctly, and still covering no pixels

`KK_LIMINA_VP_LOG` reports, for render areas 64 rows or shorter, what actually reaches Metal. It
exists because reading the geometry from the GL side is impossible -- two methods, two SIGABRTs --
while this is all plain state, readable with no synchronisation, which is what made every GL probe
either fatal or curative.

- **The scissor is correct at the layer that matters.** `area 968x44 | scissor in 0,0 968x44 -> out
  0,0 968x44`. The stale GL-side box never reaches Metal, so the one state difference that looked
  compelling was a red herring in both directions: zink synthesises its own rect when the GL test is
  disabled.
- **The bindings are well formed and identical on every draw**: `vb0 range=2304 vb1 range=2292`, an
  index buffer of 384 single-byte indices, `indexed=1 draws=1`.
- **The failing draw reaches `kk_dispatch_draw`**, past the predicate path and the geometry unroll,
  both of which bail out silently and would look exactly like a draw that rasterises nothing.

So the draw is encoded, correctly bound, correctly scissored, correctly clipped -- and covers no
pixels.

### Every observable is identical between the pass that works and the pass that fails

`KK_LIMINA_VP_LOG` was extended until there was nothing left to compare. Correlated against the
vrend episode log in the shared worker log (`evidence/kk-both-passes-identical.log`):

| dimension | working pass | failing pass |
|---|---|---|
| scissor reaching Metal | `968x44 -> 0,0 968x44` | same |
| viewport reaching Metal | `0,44 968x-44` | same |
| vertex / index bindings | `vb0 2304, vb1 2292, ib 384 x 1B, indexed` | same |
| **vertex data itself** | `-51.2,14.5 -50.4,… idx 0 1 2` | **byte-identical** |
| glyph atlas contents | `512x512 ink=23592` | **byte-identical** |
| colour attachment | `0xa1ac85900` | **same Metal texture** |
| reaches `kk_dispatch_draw` | yes | **yes** |
| result | `ink=3152 samples=1` | `ink=0 samples=0` |

The geometry is read through a debug-only GPU-address-to-CPU-pointer registry recorded at bo
creation. Metal buffers are CPU-visible, so that read needs **no synchronisation** -- which is the
whole point: both GL-side attempts to read the same bytes aborted the worker, and every GL read that
did work cured the bug instead of measuring it.

### Where this stands

**The root cause is not established.** What is established is a precise, reproducible signature and a
long list of things it is not.

The "recycled vertex buffer" hypothesis is **refuted**: the vertex data is byte-identical at encode
in both passes.

## The two passes are not the same draw: the failing one runs a different shader

The table above is **not** the whole state, and the conclusion once drawn from it -- "two identical
Metal draws, different results" -- was wrong. It listed every dimension that had been *read*, and
read no shader. The pipeline object was named as one of three candidates "past where fprintf can
go"; it was in fact one `fprintf` away.

Logging the bound pipeline and its provenance at the dispatch, over a run with two damaged cards:

| | working render | failing render |
|---|---|---|
| vertex shader | `0xcbbdb9340` | `0xcbed00e00` |
| fragment shader | `0xcbbdb9500` | `0xcbed00fc0` |
| Metal render pipeline | `0xcbbf48000` | `0xcbed11c00` |
| sample count | 1 | 1 |
| colour format | 37 | 37 |
| pipeline bound on the drawing encoder | yes | yes |

**The repeat render binds a different shader pair, and the correlation with damage is exact**: in
that run the one label attachment whose second render kept `0xcbbf48000` came out clean, and the two
that switched to `0xcbed11c00` are the two fully damaged cards. Evidence:
`evidence/failing-pass-uses-a-different-shader.log`.

### The second pass is a different cogl pipeline, not a second attempt at the same one

Dumping the NIR that goes *into* `nir_to_msl`, keyed by the same `kk_shader` pointer as the MSL,
together with the `vk_vertex_input_state` each was compiled against
(`KK_LIMINA_SHADER_DUMP=<dir>` writes both):

| | working pass | failing pass |
|---|---|---|
| attr 0 | `106` R32G32B32_SFLOAT — **vec3 position** | `103` R32G32_SFLOAT — **vec2 position** |
| attr 1 | `37` R8G8B8A8_UNORM — packed colour | `109` R32G32B32A32_SFLOAT — float4 colour |
| attr 2 | `103` R32G32_SFLOAT | `103` R32G32_SFLOAT |
| binding strides | 32 / 32 / 32 | **16 / 0 / 16** |
| vertex fetch in NIR | `32x3 %76 = load_constant_agx` | `32x2 %76 = load_constant_agx` |

The input NIR differs by 491 lines, so **the divergence is not KK's code generation** -- KK is handed
different shaders compiled against different vertex layouts. These are two different cogl pipelines,
and the baked topology class is a *consequence* of that, not the cause.

**This reframes the fault.** It was never one draw behaving two ways. A label offscreen is rendered
by one pipeline (vec3 position, packed colour, stride 32) and then rendered *again* by a different
one (vec2 position, float4 colour, stride 16, plus a stride-0 binding, i.e. a constant attribute) --
and that second pass produces no ink. Since a pass start clears, the second pass **wipes the good
content** rather than merely failing to add to it.

That retroactively explains `KK_LIMINA_FORCE_LOAD`, recorded above as "preserves stale pixels, not a
cure". It is not a stale-pixel artefact: forcing LOAD stops the second pass clearing, so the
*correct* content from the first pass survives. The arm was measuring the real mechanism and was
mis-read at the time.

### Both pipelines named from cogl's source, and what the failing one is

The two vertex layouts identify their call sites exactly, with no guest instrumentation needed:

- **The working pipeline is the cogl journal** (`cogl/cogl/cogl-journal.c`): `cogl_position_in` as
  2-or-3 floats ("3 when doing software transforms") plus `cogl_color_in` as **4 unsigned bytes**,
  interleaved at a layer-dependent stride. That is the batched-rectangle path -- the card
  background.
- **The failing pipeline is the text itself** (`clutter/clutter/pango/clutter-pango-display-list.c`):
  `CoglVertexP2T2 { float x, y, s, t }`, i.e. vec2 position at offset 0 and vec2 texcoord at
  offset 8, stride **16**, with colour supplied by the pipeline rather than per-vertex -- which is
  the stride-0 binding. Drawn as `COGL_VERTICES_MODE_TRIANGLES` with cached rectangle indices.

So the symptom is not "a second pass wipes the text". **The failing draw *is* the glyph draw**, and
it rasterises nothing -- which is precisely the reported symptom, background intact and text gone.
The paragraph above claiming the second pass overwrites good content is superseded by this.

### The load-bearing detail: the glyph VBO is cached and re-used

```c
if (node->d.texture.primitive == NULL)
  { ...allocate the buffer, cogl_buffer_map(..., MAP_HINT_DISCARD), fill the quads... }
cogl_primitive_draw (node->d.texture.primitive, fb, pipeline);
```

The source comment states the intent: *"if the text doesn't change from frame to frame the VBO can
be re-used avoiding the repeated cost of validating the data and mapping it into the GPU."* The
journal rebuilds its buffer every frame; the glyph path does not.

That asymmetry lines up with every earlier observation:

- **It rides a repaint, not first render** -- the first draw builds the buffer, later draws re-use it.
- **Every synchronisation cures it** -- a flush or readback around the draw is exactly what would
  make a missed or late buffer upload land in time.
- **Software-2D is clean** -- it bypasses virgl entirely, so no buffer transfer is involved.

**The cached glyph VBO is NOT the fault.** `LIMINA_TEXT_NOCACHE=1` (lever in the guest's
`clutter-pango-display-list.c`, kept as `mutter-text-nocache.patch`; unrefs the primitive so it and
its VBO are rebuilt on every draw) leaves the damage untouched: **11 of 11 valid samples damaged**,
against **8 of 10** for stock in the immediately preceding session, both pixel-confirmed. The arm
self-evidences in the journal (`clutter glyph primitive cache DISABLED`).

That kills the hypothesis it was built for -- that a *re-used* VBO's contents fail to reach the host
across the virgl/vrend buffer-map path. With the cache off there is no re-use: every draw gets a
freshly created buffer, the same shape as the cogl journal, which never loses content on this path.
Fresh buffers lose the text just as reliably, so cached-versus-fresh is not the discriminator and
the asymmetry against the journal lies somewhere else.

The arm also used to **SIGSEGV the host worker** within a few posts, which is why it was long
recorded as unusable. That crash was ours and unrelated -- see below -- and with it fixed the arm
runs clean to completion. Its cost is real but survivable: it churns a GL buffer object per glyph
run and zink logs `> 100 copy boxes detected` throughout.

### The worker SIGSEGV on this path was our own instrumentation, not a concurrency fault

`evidence/vrend-sync-vs-gpu-worker-segv.ips` was read as evidence of **two threads inside the same
zink/KosmicKrisp context at once**:

    thread 19 "gpu worker" (faulting, SIGSEGV at 0x11e01d000 -- a page boundary)
      Worker::process_queue -> create_fence -> vrend_renderer_create_fence
        -> _mesa_fence_sync -> tc_flush -> _tc_sync -> tc_batch_execute
          -> tc_call_draw_single -> zink_draw -> kk_draw

    thread 27 "vrend-sync"
      zink_fence_finish -> zink_screen_timeline_wait -> kk_timeline_wait

Both halves of that observation are real: creating a fence on the gpu-worker thread does force a
threaded-context sync that executes deferred draws on that thread, and vrend's sync thread is
independently inside zink. Neither is what faulted.

The fault was a dangling read in **our own poly-heap instrumentation**. It kept the CPU view of the
bump pointer in a global, set from whichever `kk_device` initialised its heap last, and dereferenced
it on every draw. One process hosts two KK devices -- host zink-on-KK serving vrend's GL, and guest
venus/vkr -- so when either is torn down its heap mapping goes away and the next draw by the
survivor reads freed memory. That is why the fault address is always page-aligned. Fixed by hanging
the pointer off the owning `kk_device`.

`limina-test::venus_replay` reproduces it deterministically, because it tears the venus device down
mid-test while host GL keeps drawing: clean at the manifest-pinned mesa rev, SIGSEGV at the
instrumentation tip, clean again with the fix. The older `.ips` above symbolises the frame as
`kk_draw.cold.1` rather than `kk_heap` (`kk_heap` is `static`, so it inlines), so it is the same
fault in all but proof.

**The rule worth keeping:** always-on instrumentation is live code on every draw and earns the same
lifetime discipline as the driver around it. A pointer into device-owned memory belongs on the
device, never in a global -- this process has two of every Vulkan object that looks like a
singleton.

One way of testing this by removing the concurrency remains unusable, and should not be retried as
stated:

- `VIRGL_DISABLE_MT=1` clears `VIRGL_RENDERER_THREAD_SYNC` (we do set it, so the lever is live).
  The guest boots and `org.gnome.Shell` reports `active` with load ~2, but **nothing is ever
  presented** -- the window stays on the last boot-console frame. Removing the sync thread while
  `ASYNC_FENCE_CB` is still enabled wedges the present path. A usable version has to drop both.

(`LIMINA_TEXT_NOCACHE=1` was the other, and it is usable now: the SIGSEGV that made it look
unmeasurable was our own instrumentation, and with that fixed the arm runs to completion. Measured
above -- it does not cure.)

Note the trap in both: each *looks* like a clean result if you only score cards. A wedged compositor
scores every post as NO BANNER, and a crashed worker scores a frozen surface as clean.

Two supports for this hypothesis have since been withdrawn, and it now rests only on the
cached-vs-rebuilt asymmetry between the two paths:

- **The `nan` in the read-back vertex data is a probe artifact, not corruption.** The `geom` probe
  prints four consecutive floats; under the journal layout those are `vec3 position` followed by a
  **packed RGBA8 colour**, and a colour reinterpreted as a float is meaningless by construction. A
  NaN there is expected and says nothing.
- **"The Metal bytes were byte-identical between the two passes"** is equally void in the other
  direction: the two passes use different buffers with different layouts, and the probe read both
  under one assumed layout, so neither the match nor the NaN carries information. The probe needs a
  per-pipeline layout before any geometry claim can be made from it.

So the question is no longer "why does the same draw behave differently" -- it never was the same
draw, nor even the same pipeline. It is now:

1. **Why does a second render of the same label use a different program at all?** That decision is
   made above KosmicKrisp, in zink or in the guest, and it is reachable by ordinary logging on both
   sides.
2. **Why does that program rasterise nothing?** `samples=0` with a sample count and colour format
   identical to the working pipeline points at what the vertex stage computes, not at pass state.

Note what made this findable: the shader identity costs nothing to read and needed no fence, while
the whole preceding effort went into dimensions that were merely *easy to think of*. The lesson
generalises -- **"identical in every dimension" is only ever a claim about the dimensions read**,
and it should be written as a list of what was checked, never as a universal.

## The Metal GPU capture: the fault will not be observed this way

A triggered device-scope Metal capture was built to see what the GPU *did* rather than what was
encoded (`KK_LIMINA_CAPTURE`, `spikes/notification-text-corruption/metal-capture.sh`). It works,
and it closes the approach -- not by showing the fault, but by proving it cannot be reached here.

**Apple's capture layer segfaults on this command stream whenever the window spans the failing
pass: 2 of 2.** It dies inside `GPUToolsCapture`, called straight from our commit:

    GTMTLSMCommandEncoder_processTraceFunc
    GTResourceTrackerProcessFunction
    -[CaptureMTL4CommandQueue _addRequestsToDownloadQueueForCommandBuffers:count:atIndex:]
    -[CaptureMTL4CommandQueue _commitCommandBuffers:count:atIndex:]
    -[CaptureMTL4CommandQueue commit:count:options:]
    mtl_command_queue_commit            <- ours; a plain [q commit:cmds count:count options:opt]
    kk_queue_submit

`EXC_BAD_ACCESS ... at 0x8`, full report in `evidence/gputoolscapture-segv-on-commit.ips`. A window
closed at the commit that carries the *first* render survives and writes a complete trace; one held
open across the repeat render does not.

**The Xcode-attached destination is closed too, for an unrelated reason: attaching a debugger to a
running worker SIGKILLs it.** Twice, and the second time with `com.apple.security.get-task-allow`
in the signature -- which a debugger does require, but which turns out not to be enough. A SIGKILL
writes no crash report and the supervisor logs no panic, so the VM just disappears mid-session with
nothing to read; recognising that signature is worth more than the attempt was. The untried variant
is attaching *before* the VM is created, on the theory that it is live `hv_vm_*` state that cannot
have its threads suspended. `LIMINA_SIGN_DEBUGGABLE=1` (`crates/limina-vmm/sign.sh`) exists for
whoever tries it -- and note `xtask run` re-signs on every boot, so signing the worker debuggable by
hand is silently undone seconds later.

**Whether capture also *cures* the fault is undecided, and deliberately left so.** The one capture
that completed -- a first-render window, worker alive at scoring -- gave a clean card, which at a
~7% clean base rate is p≈0.07: suggestive, not a finding. The failing-pass shape cannot answer it
at all, because the crash preempts the verdict: the worker dies inside that pass's commit, so the
composite never presents and the probe reads a frozen surface still showing an earlier, good
composite. **A run whose worker is dead at scoring time yields no clean/damaged verdict** -- two
such runs were briefly recorded here as "clean" before that was caught, which is this rig's own
"treat invariance as a broken oracle" trap in its purest form: three trials, ink identical to the
byte, explained away as text determinism. `metal-capture.sh` now refuses to score a dead worker.

Re-measuring the cure question buys nothing: if capture cures, the trace holds a working pass; if
it does not, the tooling crashes before writing one. Either way the next tool is the same.

One complete trace exists, of a **working** label pass, at
`traces/WORKING-PASS-cap-85549-1/limina-vmm.gputrace` (357 MB, gitignored; its encoder is labelled
`LIMINA 968x44 pass #1`, so it is searchable in Xcode's navigator). It shows nothing the encode-side
table above does not already state, and there is no failing-pass trace to diff it against.

**The remaining tool is a `limina-kk` bisect.** The signature is sharp enough that a good/bad
boundary would name the change, and unlike every probe tried so far a bisect does not touch the
timing of the run it measures.

## The host-side mimic: the failing pipeline reproduced, the fault not

`glyphmimic.c` builds the failing episode out of every property measured above, at once, and runs
it with no compositor and no toolkit -- in the guest on virgl, and natively on the host on
zink-on-KosmicKrisp. The point of the host leg is Metal tracing: Apple's capture layer segfaults
on the VM's command stream (below), so a host process that reproduces is the only route to a
capture.

Built maximal rather than incremental, because `rtsample.c` established that walking one
ingredient at a time returns clean on every arm. One 968x44 offscreen per episode, rendered by two
passes with the two different measured pipelines, composited by a stage in between and after:

| | pass 1 -- cogl journal | pass 2 -- clutter display list |
| --- | --- | --- |
| attribute formats | `106` / `37` / `103` | `103` / `109` / `103` |
| binding strides | 32 / 32 / 32 | **16 / 0 / 16** |
| geometry | 18 quads | 64 quads, 256 verts, 384 uint8 indices |

The stride-0 binding is produced the way cogl produces it: a vertex attribute whose **array is
disabled**, its value supplied by `glVertexAttrib4f`. Mesa lowers a disabled array's current value
to a zero-stride vertex buffer. A stride-0 `glVertexAttribPointer` is a different thing (GL stride
0 means tightly packed) and a uniform is a different pipeline; either substitution builds a
vehicle that cannot reproduce.

**Both gates pass, so the numbers below are worth something.** The pipeline gate:
`KK_LIMINA_SHADER_DUMP` shows the mimic compiling to exactly the two tables above -- the fingerprint
is reproduced at the level KK sees, not merely in the source. The oracle gate: `GM_NODRAW`, which
omits the glyph draw, scores `text-lost=90` with `samples=0` on all 90 -- the real case's exact
signature -- so a clean verdict is a real observation and not a detector that never fires.

**Every host arm is clean: 3600 episodes, zero losses.**

The arm names in the two tables below (`GM_PAD`, `GM_ATLASUP`, `GM_ONCE`, `GM_NOCACHE`,
`GM_REUPLOAD`, `GM_POOL`) are **retired** and no longer exist in `glyphmimic.c`. They were
additive guesses at what to put around the draw; the faithful rebuild subsumes all of them, and
its arms are subtractive instead. The results stand as evidence -- none of these provoked the
fault -- but do not try to run them.

| arm | host (zink-on-KK), 90 episodes |
| --- | --- |
| baseline (x3) | 90/90 clean |
| `GM_PAD` 1 / 4 / 16 / 64 / 256 -- filler draws between the passes | 90/90 clean |
| `GM_ATLASUP` -- atlas written between composite and glyph pass | 90/90 clean |
| `GM_ATLASUP` + `GM_PAD` | 90/90 clean |
| `GM_PRESENT`, `GM_U16`, `GM_NOCACHE`, `GM_ONCE`, `GM_NOSTRIDE0` | 90/90 clean |
| `GM_FINISH`, `GM_FLUSH` | 90/90 clean |
| 40 repeats of `GM_ATLASUP GM_PAD=8` | 0 non-clean of 40 (3600 episodes) |

**The guest leg is clean too.** Same source, built in the guest against its own EGL/GLES so the
stream reaching vrend is guest mesa's, renderer reported as `virgl (zink Vulkan 1.4(Apple M1 Max
(MESA_KOSMICKRISP)))` -- i.e. the full guest-virgl -> vrend -> zink -> KK path the real case uses.
The positive control was re-proven on this platform first (`text-lost=90`, `samples=0` on all 90),
because the host proof does not transfer to a different readback path.

| arm | guest (virgl), 90 episodes |
| --- | --- |
| baseline | 90/90 clean |
| `GM_PAD` 8 / 64 | 90/90 clean |
| `GM_ATLASUP`, `GM_ATLASUP`+`GM_PAD` | 90/90 clean |
| `GM_PRESENT`, `GM_U16`, `GM_NOCACHE`, `GM_ONCE`, `GM_NOSTRIDE0` | 90/90 clean |
| `GM_FINISH`, `GM_FLUSH` | 90/90 clean |
| 40 repeats of `GM_ATLASUP GM_PAD=8` | 0 non-clean of 40 |

**Both legs clean, so the reading is the third one: the fingerprint is incomplete.** Not "vrend's
emission is the trigger" -- that branch required the guest to fail while the host stayed clean, and
it did not. Every property we knew how to name about the failing draw is now reproduced, verified at
the pipeline level, on both routes into the convicted component, and none of it provokes the fault.
What provokes it is therefore something we have not yet named, and no further guessed arm on this
vehicle can find it.

That is the point to stop guessing and measure. The next step is not a new ingredient: it is to
instrument **vrend**, which we own, to record the actual GL call stream it emits at a failing
episode, and replay that stream on the host. The existing `LIMINA_VREND_*` levers are all
behavioural toggles; none of them records what vrend emits, so the dump has to be added. Until then
the vehicle's value is as a fast, gated negative control and as the eventual regression test.

**This does not exonerate zink or KK, and must not be read that way.** The locus is not in
question -- the host-implementation split above convicted zink/KosmicKrisp with everything from the
guest driver down through vrend held constant. What this vehicle probes is the **trigger**: which
input provokes the fault. A clean host run says the GL *this file writes by hand* does not provoke
it, which is a statement about the input, not about the implementation.

That distinction is what makes the guest leg informative rather than redundant. The same source
reaches zink/KK by two different routes, and in the real case the GL is not hand-written at all --
vrend emits it from the guest's virgl commands, with its own state setting, buffer orphaning and
bind pattern. So `guest reproduces + host clean` localises the trigger to vrend's emission, and
`both clean` says the fingerprint is incomplete on either route. Until one leg fails this is an
unproven mimic.

Two gaps are already known and are the first things to close if the guest leg is also clean:

- **Incidence was session-unstable in the real case** -- 8/10 in one session and 0/32 in another on
  the same build, with the card's identity controlling it. That is consistent with dependence on
  accumulated process state a fresh 90-episode process may never reach.
- **The real host side runs two KosmicKrisp devices in one process** -- venus/vkr for the guest and
  zink-on-KK serving vrend's GL. A single-device mimic structurally cannot reproduce a fault that
  lives in the interaction between them. That is the next hypothesis if both legs come back clean,
  and it is not reachable by adding more single-process arms. The poly-heap SIGSEGV above is a
  standing reminder that the two-device shape is real and has already bitten once.

Run the legs with `mimic-host.sh` (which carries the host KK/zink env; a bare run aborts in GPU
init) and `mimic-build.sh` in the guest. Both gates are documented at the top of `mimic-host.sh`
and must be re-run per platform -- the oracle was proven on zink-on-KK, and virgl is a different
readback path.

## The vrend command tracer: what vrend is actually asked to do

Both mimic legs came back clean, so the fingerprint is incomplete and the way forward is to stop
naming ingredients and record the real thing. `LIMINA_VREND_TRACE=<MB>` (virglrenderer `limina`,
`src/vrend/vrend_trace.[ch]`) arms a preallocated ring that captures the whole stream; a dump is
requested with `echo x > /tmp/limina-vrend-trace.fifo` and decoded by
`vrend-trace-decode.py`.

**It buffers in memory and writes only on request, and that is the load-bearing design choice.**
A bare `glFlush` cures this fault, so it sits at a submission boundary; a tracer emitting a line
per command would move the boundary it is trying to observe. The hot path does an
allocation-free, syscall-free append.

Hooked in three places, because the command stream alone does not contain the interesting work:

| hook | why it is not enough to hook the decode loop |
| --- | --- |
| decode loop, full payload per command | the baseline; payloads kept whole so the stream stays replayable |
| `vrend_renderer_transfer_iov` | transfers never enter the command stream -- and this is the glyph-atlas upload path, i.e. the write-then-sample shape under suspicion |
| `vrend_renderer_create_fence` + per-fence retire | libkrun fences through the **ctx0** path; hooking the decode layer's fence entry recorded **zero** fences and read as "this session takes no fences" |

Draws carry the bound target's size unconditionally, so the 968x44 label stays findable after its
create has aged out of the window.

A first capture of the live desktop, as a smoke test of the instrument:

```
records: CMD 13197, DRAW_FB 1453, FENCE 398, RETIRE 398, TRANSFER 160, SUBMIT 119
draw targets: 4102x70 (panel), 4102x230, 2560x1440 (stage), 204x44, 250x250, ...
```

**Two bugs the instrument had, and how they were caught -- both are the reason to distrust a
tracer until it is gated.** Records do not all arrive on one thread: fences retire on vrend's
poll thread, and the unlocked first version produced a trace with **one sequence number claimed
by two records**. Nothing about the trace looked wrong; only counting distinct seqs against the
record count exposed it. The decoder now runs that check on every load and says so, because a
trace that silently invents or tears records would corrupt every conclusion drawn from it. The
second: a dump requested while the guest was idle never happened, because only the render thread
serviced the request at a submit -- indistinguishable from a broken tracer. The FIFO thread now
dumps directly.

`evicted` in the dump header is the count of records that aged out. It is not a curiosity: the
ring is a window, and a nonzero value means the capture no longer reaches back to the episode of
interest. Size the ring for the session and fire the dump promptly after damage is seen.

## The first trace of a damaged session: the glyph draw's vertex buffer is uploaded 0.08 ms before it

Captured with the tracer above during a session damaged **9/9** (`calib.sh`, title ink 0 and header
ink 0 on every valid post, body ink ~845 proving a banner was really on screen). The window spans
875 s with **0 evicted**, so the whole session is in it, and the integrity gate passes.

**The card is not one offscreen, it is many.** Per posted notification the guest renders a fixed
set of small targets and then composites them into the stage:

```
568x44   <- the GLYPH-shaped draw: 3 bindings, strides (16, 16, 0)
968x44   journal shape, strides (32, 32, 32) at offsets 0 / 12 / 16
110x38   134x44   38x38   82x82   98x98      then 2560x1440 (the stage), many draws
```

Each of these is rendered **twice** per post, and the whole set repeats.

**Which draw is the text is now measured, not inferred.** Searching the trace for the constant
(zero-stride) colour attribute finds `(16, 16, 0)` exactly **20 times in 10 posts** -- twice per
post, and nowhere else in the session. Every one of them targets **568x44**. The 968x44 draws use
the journal layout. Earlier work here treated 968x44 as the failing title draw; at least on this
vehicle and card, the glyph-shaped draw is the 568x44 one, and it is worth re-reading the older
sections with that in mind.

**The finding: the glyph draw's vertex buffer arrives by transfer, immediately before the draw.**
Every glyph draw is preceded by a `TRANSFER` of exactly **2432 dwords** into the very resource it
then binds as its vertex buffer, and in 9 of 10 posts both passes reuse the *same* resource, which
is re-uploaded each time.

| | upload -> draw (median) | range | fences between |
| --- | --- | --- | --- |
| 1st pass | 0.22 ms | 0.18 - 2.70 | 0 |
| 2nd pass | **0.08 ms** | 0.07 - 0.31 | 0 |

The second pass -- the one already established to produce nothing -- draws from a vertex buffer
uploaded roughly **three times closer** to the draw, with no fence in between on either.

**What this does and does not establish.** It is the first *observable difference* between the pass
that lands and the pass that fails: the section above concluded "every observable is identical",
and that conclusion was reached without any visibility into transfers, which do not appear in the
command stream at all. It also matches the host probe's finding that the vertex buffer the GPU
fetches is all-zero -- a buffer whose upload has not landed looks exactly like that. But it is a
**correlation over ten episodes**, not a demonstrated race, and both intervals are small.

**The obvious next arm is a weak test, and that is worth saying before someone runs it.**
`LIMINA_VREND_TRANSFER_FORCE_SYNC=1` would very likely turn the damage off -- but *every*
synchronisation tried on this bug has, so a cure there discriminates nothing. What would
discriminate is comparing the bytes the transfer delivered against the geometry the draw actually
fetched, which needs the transfer payload recorded (the tracer currently keeps the box, not the
data) and a matching read at the draw. That is the next instrument, not the next arm.

## Tracing the mimic against the real thing: the measured gap list

Both streams captured with the same instrument, gnome-shell's during a 9/9-damaged session and
the mimic's from the same host, then separated by **virgl context id** (different processes get
different contexts -- separating them by sequence window instead gives wrong answers, and did
once here).

| property | gnome-shell, damaged | matched by the mimic? |
| --- | --- | --- |
| glyph draw target | 568x44 | yes |
| card composition | 568x44, 968x44, 110x38, 134x44, 38x38 siblings, one draw each per frame, composited into 2560x1440 | yes |
| frames per card | exactly 2, 13-36 ms apart, then abandoned | yes |
| depth attachment | D24S8 (`S8_UINT_Z24_UNORM`), fresh surface per frame | yes |
| colour attachment | fresh surface per frame over a persistent texture | yes |
| constant colour attribute | stride 0, separate long-lived buffer, offset +16 per frame | yes, unbuilt -- mesa lowers it there |
| index buffer | long-lived, uint8, offset 0, never re-uploaded | yes |
| draw | 228 indices, TRIANGLES, indexed, 1 instance | yes |
| upload size | 2432 bytes = exactly the draw's 152 vertices | yes |
| upload -> draw | median 0.18 ms | 0.66 ms |
| submit batches between upload and draw | 0 on 36 of 36 | 0 on 59% |

**The last two rows are the only ones still open, and neither is load-bearing.** In the real case
the vertex upload and the draw that consumes it are *always* in the same virgl command batch, and
a submission boundary is the one thing already known to cure this fault -- so a vehicle that
inserts the cure between the two operations under test cannot reproduce, whatever else it gets
right. That was worth chasing, and it was chased to the end: an arm was built in which **every
one of 200 draws had zero submit batches between its upload and itself**, the shell's exact
invariant. It is clean. The sharpest structural candidate this trace can name is matched and
exonerated.

Reaching that invariant at all requires a **per-frame flush**. Without one, virgl accumulates many
frames into a single batch and emits all of their transfers at its head, hundreds of milliseconds
from the draws that read them. Two other things must stay out of the window between upload and
draw or they split it themselves: `glCheckFramebufferStatus` (a round trip -- check the FBO once,
not per frame) and any readback.

A trap worth naming from these measurements: the ring outlives a process and **virgl context ids
are reused**, so a later run's records sit in the same ctx as an earlier one's. Analysing "ctx 8"
as though it were one run silently mixes arms; window by time or by sequence range within the
context, and never confuse `glBufferSubData` (writes in place, as the shell does) with
`glBufferData` (orphans and mints a fresh resource -- a third behaviour that is nobody's).

Two claims from earlier sections need reading in this light. "Every observable is identical
between the pass that works and the pass that fails" was reached with no visibility into
transfers, which never enter the command stream; the passes differ in upload-to-draw distance
(0.08 ms failing vs 0.22 ms landing). That difference is probably **not** causal -- a 5 ms sleep
in the same place was already measured to cure nothing -- so latency is not the active ingredient.

**What this branch establishes, and where it stops.** The trigger is not any single property of
the glyph draw. Every property the trace could name was reproduced, one at a time and then all at
once, and none of them provoked it.

## The faithful mimic: the stream matched field-for-field, and still nothing breaks

The gap list above was closed by extracting the failing draw's exact state from the trace rather
than inferring it, and rebuilding the mimic's episode around it. Four ingredients had been wrong
or missing, and two of them were invisible until the decoder printed them:

- **A D24S8 depth-stencil attachment on every offscreen** (`SET_FRAMEBUFFER_STATE` carries a
  nonzero zsurf whose surface is `VIRGL_FORMAT_S8_UINT_Z24_UNORM`). Metal has no 24-bit depth, so
  KosmicKrisp emulates this format. "D24S8 emulation" is one of the five exonerations from the run
  of pixel-identical A/B results -- i.e. it had never actually been under test. The mimic had no
  depth buffer at all.
- **A fresh surface object per paint over persistent textures.** Both attachment surfaces are
  newly created for each frame and wrap the same two resources. In GL terms: a new FBO every
  frame, the textures kept.
- **A card is a set of sibling offscreens** -- 568x44 title, 968x44, 110x38, 134x44, 38x38 -- each
  drawn *once* per frame and composited into the 2560x1440 stage, not one label drawn twice.
- **A card lives exactly two frames.** Ten cards, twenty glyph draws, two per colour resource,
  13-36 ms apart, then the resources are abandoned.

Two earlier readings were wrong and are corrected here. The failing draw is **228 indices = 38
quads = 152 vertices**, so uint8 indices never approach their 255 ceiling: that ceiling is not a
property of this bug, it was an artifact of a 64-quad choice made before the trace existed. And a
virgl buffer transfer's extent is in **bytes**, not dwords -- confirmed by the mimic reporting back
exactly the byte count it uploaded -- so the shell's 2432 is 152 vertices at stride 16, *exactly*
the draw's data with no slack, not a buffer four times the size of its draw.

With all of it assembled the two streams are indistinguishable at the draw-state level: same
target, fresh colour and depth surfaces over persistent resources, the stride-0 constant attribute
fed from a separate long-lived buffer at an offset advancing 16 bytes per frame, a long-lived
uint8 index buffer at offset 0, 228 indices, the same GL framebuffer across both frames, and a
2432-byte upload. The one ingredient that never needed building was that separate-resource
constant attribute: mesa's current-value lowering already produced it.

**Verdict: no arm ever loses the text** -- twelve arms and forty repetitions, on the host and in
the guest. (`GM_COMPOSITES=0` scores BLANK rather than clean: with nothing composited there is
nothing to read back. That is the oracle being removed, not a cure.) while the same session lost the title on **16 of 16 real cards** posted immediately
before and immediately after the run. That pairing is what makes this negative worth something --
incidence is session-unstable, so a clean mimic in a quiet session proves nothing, while a clean
mimic beside a 16/16-damaging real card is the strongest result this vehicle can produce.

The subtractive arms (`GM_NODEPTH`, `GM_FRAMES`, `GM_WIDE`, `GM_COMPOSITES`, `GM_GAP_MS`,
`GM_NOSTRIDE0`, `GM_U16`, `GM_FINISH`, `GM_FLUSH`, `GM_PRESENT`) exist for the case where the
faithful arm *does* reproduce: each removes one measured ingredient, so the reproduction can be
minimised by finding which removal cures it. Building the other direction -- adding one property
at a time to a stripped mimic -- never provoked anything and is not worth repeating.

One residual difference is measured and is *not* the blocker: the shell holds zero submit
boundaries between an upload and the draw consuming it on 36 of 36 draws, while the mimic reaches
that on 59% (median distance 0.66 ms against the shell's 0.18). The exoneration is *internal to
the faithful arm* and that is what makes it count: **881 of its 1485 draws held the shell's exact
zero-boundary invariant, and every one of them was clean**. So the invariant is cleared in
combination with every other measured ingredient, not in isolation -- an earlier arm that matched
it on a mimic with no depth buffer, no siblings and no fresh FBOs would have proved nothing, by
this spike's own rule. Reaching it at all needs a
per-frame flush: without one, virgl accumulates many frames into a single batch and emits all
their transfers at its head, hundreds of milliseconds from the draws.

**This mimicking branch is closed.** Two exits remain, both named before this round and neither
reachable by building a guest program that draws like the shell:

1. **Replay the captured stream through vrend on the host.** The tracer already records full
   command payloads for this purpose; what it does not yet record is transfer *contents*.
2. **The two-device hypothesis.** The worker hosts two KosmicKrisp devices -- venus/vkr and
   zink-on-KK -- and a single-process mimic cannot reach a fault in their interaction.

A side observation from the faithful arm, unrelated to the text but worth having: the fresh-FBO
and depth-texture churn makes KosmicKrisp's allocator pool grow without bound
(`[LIMINA-ALLOC-POOL] class 0 grew to 301 allocators (budget 4 MiB) -- in-flight depth is
outrunning completion`). It does not affect the verdict here.

## The stream replay: the fault reproduces on the host, with no VM

The captured vrend command stream, replayed through libvirglrenderer on the host with no guest and
no compositor, loses the notification header exactly as the real session does. `vrend-replay.c`
takes a trace, recreates every resource on the control path, feeds the recorded bytes into their
backing stores, submits the batches, and reads the offscreens back.

What one sweep reports, deterministic across runs:

| offscreen | contents (read from the pixels) | card 1 | cards 2-12 |
|---|---|---|---|
| 968x44 | title, "Critical Updates 001" | inked | **blank** |
| 110x38 | timestamp, "Just now" | inked | **blank** |
| 134x44 | header element | inked | **blank** |
| 38x38 | icon | inked | **blank** |
| 568x44 | body, "install critical updates as soon as possible" | 23 px | **4668 px, every card** |

That is the reported fault: the card keeps its background and body and loses the header row, the
icon and the title. Eleven damaged cards, against a live session `bannerprobe` scored 11/11
damaging immediately before the dump.

**Name the offscreens from pixels, not from geometry.** A note in this tree had 968x44 and 568x44
swapped, which inverts the verdict — the same run reads as "the title renders and the body is lost".
`REPLAY_DUMP_W=568 REPLAY_DUMP_DIR=<dir>` plus `rgba2png.py` puts the actual text on screen.

**Why this is not a replay artefact.** Card 1 and cards 2-12 are structurally identical — same
sizes, same two draws per offscreen, same D24S8 depth sibling — and only the later ones lose their
text. Any missing replay input takes card 1 with it, and a gap in what the trace captured before it
armed would hit the *earliest* cards hardest; the differential runs the other way. The depth
attachment does not discriminate either: every offscreen in both classes has one, which finally puts
a measurement under the long-standing D24S8-emulation suspicion instead of another exoneration that
never had it under test.

Controls, run before reading any of it: `--nodraw` leaves all 58 card offscreens blank (the five
that keep ink are upload-fed icons, which draw nothing), and all 675 copy-payload correlations are
verified rather than assumed — 0 unmatched.

**The vehicle is deterministic.** Six sweeps in a row: 214 verdicts, 0 submit errors, identical
values throughout.

It was not, and the cause is worth keeping because it produced convincing wrong answers rather than
crashes. `backing_add` held the replayer's backing stores in a `realloc`'d array of structs and
handed vrend `&b->iov`. vrend stores that `struct iovec *` and dereferences it for the resource's
whole life, so **every previously attached resource dangled the moment the array outgrew its
capacity**. Whether that mattered depended on whether realloc happened to move the block, which is
why the same trace would sometimes sail through and sometimes report `src iov_len=0` on one copy
transfer. Hold backings by POINTER; a backing's iovec must never move.

The failure downstream of that is the part to recognise again: **one rejected command poisons the
context permanently, and everything after it fails in silence.** `vrend_report_context_error` sets
`ctx->in_error`, `vrend_hw_switch_context` refuses a context in error, and
`vrend_decode_ctx_submit_cmd` then returns EINVAL with no message of its own. So a single bad
COPY_TRANSFER3D turned into 391 silently failed submits and 115 failed readbacks — and the process
still **exited 0**, having printed 105 verdicts instead of 214. A short verdict list is the only
symptom. Check the count and the submit-error total on every run; a missing verdict means the run
never got there, not that the offscreen was blank.

### The lever sweep: exactly one lever moves it, and it is a mask

With the replay as the vehicle, the spike's own KK/zink A/B levers become testable one run each.
Twelve 968x44 title offscreens, card 1 first; BASELINE is run as an arm, because a sweep whose
baseline does not reproduce the known signature is measuring nothing:

| arm | titles |
|---|---|
| BASELINE (gate) | `I...........` |
| `KK_LIMINA_FORCE_LOAD=1` | **`IIIIIIIIIIII`** |
| `KK_LIMINA_FORCE_LOAD=small` | `I...........` |
| `KK_LIMINA_FORCE_TOPO_UNSPEC=1` | (hangs, >150 s) |
| FORCE_STORE, HEAP_NORESET, NO_PROMOTE, SERIALIZE, BARRIER, ZINK_NO_FANS, NOLISTRESTART, NOROBUST | `I...........` each |

Exactly one lever moves it, and it moves it all the way. The reading first drawn from that -- that
the clear is the defect -- is **wrong**, and the section below establishes why with the draws
counted: the clear is a legitimate per-repaint clear, and `FORCE_LOAD` is a mask that resurrects the
*previous* repaint's copy. Read this table as "one lever reaches the fault", never as "the fault is
the load action".

KK counts this itself — `LIMINA_KK_STATS=1` over the replay:

    pass starts: fresh=78 seen+LOAD=383 seen+CLEAR=118 seen+DONTCARE=10 (fresh small=62)
                 | reload_hazard=128 (small=30)

128 pass starts discard a target that had already been drawn -- a count of *opportunities* for the
mask to bite, not of defects. Only 30 are "small", which is why
`=small` cannot cure a 968-wide title: it forces LOAD only on attachments <= 512 px, so it never
touches this one. That arm is not a control for this fault — do not read its null as evidence.

**The cure is verified against a reference, not against an ink count.** `FORCE_LOAD=1` is a
diagnostic and not a fix, and its own comment says why: with nothing cleared, previous frames pile
up and stale ink reads as a healthy header. That is real and visible here — cards 9-11 come back at
roughly double (6734/6588/6763 against llvmpipe's 3399/3264/3145). But cards 1-8 and 12 match the
llvmpipe reference EXACTLY, pixel count for pixel count (3264 3350 3362 3348 3364 3396 3293 3397 …
3231), and card 12's dump reads "Critical Updates 012" — its own number, not an earlier card's. So
the cure is genuine on 9 of 12 and the doubling is a separate, expected artefact of the lever.

**Trap, again, and it nearly produced a clean table of nothing.** macOS has no `timeout` binary.
The first sweep wrapped every arm in it, so every arm silently no-opped and returned an identical
result — including the baseline that was already known to work. Uniformity across arms is the
signature of a differential not reaching the system under test; run the baseline as an arm and read
nothing until it passes.

### The host-implementation split, reproduced inside the replay

The same trace, the same binary and the same commands, with only the host GL implementation
swapped underneath vrend. Both arms run `LIMINA_VREND_IOSURFACE=0`, so the scanout path is held
constant too and the driver is the single variable. Twelve title offscreens, card 1 first:

| host GL | 968x44 title offscreens |
|---|---|
| zink → KosmicKrisp → Metal | `I...........` |
| llvmpipe | `IIIIIIIIIIII` |

Three runs of each, identical every time. Under llvmpipe every title renders (3145-3399 inked
pixels, varying as the banner numbers do); under zink-on-KK eleven of twelve are exactly zero.

  MESA_PREFIX=/Volumes/mesa-cs/zink-llvmpipe-prefix LIMINA_HOST_GALLIUM=llvmpipe \
    LIMINA_VREND_IOSURFACE=0 ./replay-host.sh <trace> --sweep --sweep-w 968

llvmpipe needs `LIMINA_VREND_IOSURFACE=0` or it asserts in `init_scene_texture` on a map failure —
the same assert the live split hit, for the same reason.

This retires the live split's stated caveat, that llvmpipe's cleanliness might be timing
suppression because it is slower. There is no live session here and no compositor to race: the
replay submits a fixed recorded stream at its own pace in both arms. The zink-on-KK result is also
perfectly deterministic — eleven blank, one inked, to the pixel, on every run — which is not how a
timing race behaves.

### What made the replay lie before it worked

Four defects, each of which produced a confident blank verdict:

- **The iov must reach vrend, and passing it to create prevents that.**
  `virgl_renderer_resource_create` stores the iov on the *virgl* resource and stops — its signature
  marks the parameter `UNUSED` — while vrend's resource, the one every transfer reads, is fed only
  by `virgl_renderer_resource_attach_iov`. The two are mutually exclusive: `attach_iov` returns
  `EINVAL` when an iov is already set. Create with NULL, then attach.
- **`COPY_TRANSFER3D` payloads belong to the SOURCE.** vrend captures the bytes inside
  `transfer_write_iov`, which the copy path reaches as `transfer_write_iov(dst_res, src_res->iov, …)`,
  so the record is keyed by dst while the offset and the bytes belong to src. Seeding dst put 675 of
  1617 uploads where nothing reads them and left the real source at zeros. No recapture is needed to
  correlate them: command records are written *after* their dispatch, so the next CMD in the context
  is the copy that produced the payload.
- **Score at the resource's UNREF, never by suppressing it.** virgl handles are REUSED, so a
  survivor makes a later create collide and everything downstream of it stops rendering.
- **"0 submit errors" is not a clean replay.** It counts only `submit_cmd`'s return; per-command
  dispatch failures inside a batch never reach it. The first run that passed that gate had 168
  dropped draws.

And one in vrend itself: `vrend_renderer_transfer_iov` reports `ILLEGAL_RESOURCE` both for "the
context does not know this handle" and for "it does, but there is no backing iov". Reading one for
the other costs a session chasing resource attachment for what is a backing bug. `LIMINA_ATTACH_TRACE`
now separates them, and also prints which branch `vrend_renderer_attach_res_ctx` takes.

### The mechanism, with the draws counted: the repeat draw is a different pipeline and inks nothing

Counting draws per render pass -- with every draw path logged and every encoder identified at
creation -- resolves the whole sequence. Each 968x44 title offscreen, in encode order, with `C` a
pass that begins CLEAR and `L` one that begins LOAD, and the letters inside naming the bound
VS/FS pair:

    C[-] L[AB] C[-] L[CD]          (8 of the title textures; 12 titles share 8, textures are reused)
    C[-] L[AB] C[-] L[CD] C[-] L[EFABEF]

Three facts fall straight out, and together they are the fault:

- **Every CLEAR pass draws nothing.** 24 of 24, exactly.
- **The repeat pass binds a different shader pair.** `A/B` draws the title in pass 2; pass 4 redraws
  with `C/D`. This is the live session's "the failing render binds a different shader pair", now
  deterministic and in a program with no compositor.
- **KosmicKrisp drops nothing.** `kk_draw` entries == `mtl_draw_*` encodes == 4465, zero bails on
  the predicate or unroll paths. Every draw that enters KK reaches Metal.

So the sequence per title is: pass 2 draws the correct title with `A/B`; pass 3 clears it away;
pass 4 redraws with `C/D` and **produces no ink**; the readback is blank. `FORCE_LOAD` cures by
stopping pass 3 clearing, so `A/B`'s correct output survives -- which is why it matches the llvmpipe
reference pixel-for-pixel rather than looking like stale ink. **The clear is legitimate per-repaint
behaviour. The defect is that the pass-4 draw rasterises nothing.**

`C/D` is *not* the culprit, and the section below shows why: one title's pass-4 draw inks, using the
same `C/D` pipeline as the eleven that fail. The two-pipeline structure is real and explains which
draw is at risk; it does not explain which instance of it fails.

llvmpipe closes the argument from the other side: it inks all twelve from the *same trace*, so the
stream genuinely contains ink-producing draws after that final clear. vrend is common to both arms.
The draw exists, KK encodes it, and only under zink-on-KK does it land empty.

This supersedes both earlier framings in this file. "The failing draw rasterises nothing" was right
about the draw and wrong to call the clear incidental; "content loss at pass start is IN" was right
that the clear participates and wrong to call it the defect. Neither is the whole statement; the
layered one above is.

**Two probe-coverage traps, both of which produced confident invariance.**

- The draw probe covered `mtl_draw_primitives` only. On this stack that is 208 of 4465 draws -- zink
  issues **indexed-indirect** draws, so the glyph path was entirely invisible and every title pass
  read as "no draws". The render-pass log then looked identical across working and failing cards,
  which reads exactly like a differential not reaching the system under test. It was a hole in the
  instrument. *"Identical in every dimension" is only ever a claim about the dimensions read* --
  and that includes the dimension you believe you are already reading.
- Encoder pointers **recycle** across command buffers. Binding `enc -> pass` on first sight and never
  rebinding silently attributes later draws to a dead pass; it invented a 21-draw pass that does not
  exist and made the per-pass counts look card-dependent when they are uniform. A zero-unmapped-draw
  control does not catch this -- it catches drops, not misattribution. `[LIMINA-KK-RPENC]` now prints
  the encoder at creation, immediately after its descriptor: parse the pairing, never infer it.

### The vertex layouts, reproduced -- and why the pipeline is NOT the discriminator

`KK_LIMINA_SHADER_DUMP` over the replay reproduces the live session's table exactly, from the
deterministic vehicle:

| | working (pass 2) | failing (pass 4) |
|---|---|---|
| attr 0 | `106` R32G32B32_SFLOAT vec3 position | `103` R32G32_SFLOAT vec2 position |
| attr 1 | `37` R8G8B8A8_UNORM packed colour | `109` R32G32B32A32_SFLOAT float4 colour |
| attr 2 | `103` R32G32_SFLOAT | `103` R32G32_SFLOAT |
| binding strides | 32 / 32 / 32 | **16 / 0 / 16** |

Working is the cogl journal; failing is `CoglVertexP2T2` from the clutter/pango display list, its
colour supplied by the pipeline rather than per-vertex -- which is the stride-0 binding.

**A lead was drawn from this and is RETRACTED; the retraction is the useful part.**
`KK_LIMINA_VP_LOG` shows all 24 title draws binding `vb1 == vb0 + 12`, ranges 2304/2292 -- offset 12
being where the packed colour sits in the journal's 32-byte vertex. Read as "the pass-4 draw is
compiled for stride 16 but bound with the journal's stride-32 shape", that is a mechanism which
rasterises nothing while reporting no error. It does not survive contact with the rest of the data:
**every** title pass-4 draw shares those bindings, and one title inks. A binding shape common to the
working draw and the failing ones cannot be what separates them.

That failure is worth more than the lead was, because it names the real constraint:

- All title pass-4 draws bind the **same** VS/FS pointers (`C/D`, one pair across every title).
- All bind the **same** vertex buffers (`+12`, identical ranges).
- All carry the **same** scissor `(0,0 968x44)` and the **same** viewport
  `(0.0,44.0 968.0x-44.0)` -- full target, correctly flipped.
- Exactly one of them inks.

So the draw that works is indistinguishable from the eleven that do not **in every dimension read so
far**, and those dimensions cover the pipeline object and the geometry-coverage state entirely.
The defect is therefore **per-draw data, not per-draw pipeline state**: the vertex *contents*, or
the glyph atlas the fragment stage samples. A draw that rasterises correctly while sampling an
unwritten atlas region inks nothing and reports nothing.

Two dimensions are now closed rather than open, which is the point of recording them: viewport and
scissor are uniform, and robustness is out on its own arm (`LIMINA_KK_NOROBUST` leaves the sweep at
`I...........`), so a stride-0 colour binding collapsing to a zero fetch range is not it either.

One caveat on identity: the replay cannot yet map a virgl resource to a Metal texture, so which of
these eight textures is card 1 is not established. The argument above does not need it -- it rests
on all eight being identical in the dimensions read while exactly one inks -- but the mapping is
owed before any per-card claim, and vrend's `set_framebuffer_state` shares the replay's stderr,
which is the cheap way to get it.

### ROOT CAUSE: zink does not rebind the pipeline between the body draw and the title draw

Nothing is missing from the recorded stream. Every glyph-pipeline draw has its own upload a median
0.162 ms earlier with **zero** submit batches in between, and every title draw samples the **same**
sampler view -- the working card and the failing ones alike. That is what llvmpipe already implied:
inking all twelve from this trace is only possible if the content is in it.

Resource-to-card mapping, read from the framebuffer colour surface at each draw and confirmed
against the verdict lines: `res 293` is card 1's title, `res 295` its body; `res 353`/`res 355` are
card 2's; and so on up by ~38 per card.

**Each card is painted twice**, and each paint is a body draw (cogl/pango glyph pipeline, strides
16/0/16 in KK's binding order) immediately followed by a title draw (cogl journal pipeline, strides
32/32/32). Keying every draw on the pipeline object actually in effect, and on whether zink issued a
bind between the two draws of a pair:

| body draw runs under | bind issued between the two draws | title draw runs under | count |
|---|---|---|---|
| 16/0/16 (`0xbb8974000`) | **yes, to 32/32/32** | 32/32/32 | x12 |
| 16/0/16 (`0xbb89e8e00`) | **none** | 16/0/16 | x11 |
| 32/32/32 | none | 32/32/32 | x1 |

Row 1 is every card's **first** paint: zink binds the title pipeline and the title renders. Row 2 is
the **repaint** of cards 2-12: zink issues no bind at all, so the title draw executes under the
body's still-bound pipeline. Row 3 is card 1's repaint, where the inherited pipeline happens already
to be the right one.

A vertex stage compiled for the body fetches vec2 positions at stride 16 out of the title's stride-32
buffer. It rasterises nothing -- no error, no warning. The pass legitimately began with a CLEAR, so
the correct content from the first paint is already gone, and the offscreen reads back empty.
`FORCE_LOAD` cures by suppressing that clear, restoring the first paint's output, which is why it
matches llvmpipe pixel-for-pixel.

**The fault is symmetric, and card 1 carries both of its signs.** Row 3 is the same missed rebind
with the roles reversed: card 1's *body* repaint runs under the title's 32/32/32 pipeline. Scoring
every body confirms it -- eleven bodies ink a dead-uniform 4668 pixels, and card 1's inks 2313:

| resource | card | ink pixels |
|---|---|---|
| 295 | 1 | **2313** |
| 355, 394, 431, 469, 508, 545, 581, 616, 653, 688, 725 | 2-12 | 4668 each |

The pixels settle what the count alone could not. Card 2's body is clean text; card 1's is sheared,
colour-smeared garbage -- the same string and the same glyph count, so "card 1's body just says
something different" is dead:

- `evidence/replay-body-card2-clean-res355.png` -- "Install critical updates as soon as possible"
- `evidence/replay-body-card1-sheared-res295.png` -- the same draw through the wrong vertex layout

So card 1 is the only card whose title survives **and** the only card whose body is destroyed. One
fault, both signs, on the one card where the stale binding runs the other way.

**The defect is above KosmicKrisp.** KK receives no bind and faithfully keeps the pipeline it holds;
the check that decides this is a grep for a `stage=0` bind between the two draws of a failing pair,
and there is none. Both pipeline variants exist and are individually correct, so the pipeline cache
is not implicated either -- the skipped step is the *bind decision*. Because KK does not advertise
`VK_EXT_vertex_input_dynamic_state`, zink bakes attribute formats and offsets into the pipeline and
leaves only the stride dynamic, so a missed rebind silently swaps the entire vertex layout instead of
tripping validation.

The guest supplies the pattern that defeats the dirty tracking: cogl alternates these two vertex
layouts per card, A-B-A-B across submits. That is also the most likely reason the field-for-field
mimic never reproduced the fault -- it matched the state, not the alternation cadence.

Two rules this section earns:

- **Ordering is a sound join across a seam; texture identity is not.** Both sides execute the same
  stream in order, so the Nth KK draw into a title target is the Nth title draw in the trace, and
  the two sequences interleave identically (48 draws, `BTBTBT...`). An earlier grouping by Metal
  texture pointer was scrambled by texture reuse and produced a confident wrong answer.
- **Log the object in effect, not the events.** Counting binds between draws splits 12/12 and says
  nothing; keying on the pipeline actually in effect splits 12/11/1 and says everything.

This also retires an identification that stood since the live session: **the failing title draw is
not the clutter/pango glyph pipeline.** That pipeline draws only the 568x44 body. The titles are cogl
journal draws at stride 32; what executes for them is the body's pipeline, which is the fault above.

The shader-dump aliasing that would have explained all of this away was tested and does not exist:
the dump path carries a compile counter, and no shader is compiled twice.

### THE FIX: force the pipeline lookup when vertex elements change and vertex input is static

`zink_draw.cpp` decides whether to look the pipeline up at all -- and so whether
`CmdBindPipeline` is reached -- from a fixed set of flags: `gfx_pipeline_state.dirty`, the
renderpass state, `gfx_dirty`, `dirty_gfx_stages`, the primitive mode, and batch changes.
A vertex-element rebind sets **none** of them. It sets `ctx->vertex_state_changed`, which on
this path is consumed by nothing: it exists for `CmdSetVertexInputEXT`, which only the
`ZINK_DYNAMIC_VERTEX_INPUT*` paths reach. KosmicKrisp lands on `ZINK_DYNAMIC_STATE3`.

Logging the whole gate rather than its verdict shows it directly -- 24 title draws, split
exactly in half, with the vertex state changed at every single one:

```
gate=1  dirty=1 rp=1 gfx_dirty=0 stages=0x0 prim=0 batch=0 vtx_changed=1   x12   first paint
gate=0  dirty=0 rp=0 gfx_dirty=0 stages=0x0 prim=0 batch=0 vtx_changed=1   x12   repaint
```

The first paint of each card is rescued by unrelated dirt (`dirty`/`rp`), not by anything
noticing the layout change. On the repaint nothing is dirty, the lookup is skipped, and the
draw inherits its predecessor's compiled vertex layout.

The fix adds the missing term, compile-time gated so the dynamic paths pay nothing:

```c
const bool vi_is_static = DYNAMIC_STATE != ZINK_DYNAMIC_VERTEX_INPUT &&
                          DYNAMIC_STATE != ZINK_DYNAMIC_VERTEX_INPUT2;
... || (vi_is_static && ctx->vertex_state_changed) ? update_gfx_pipeline<...>(...) : false;
```

Placed at the gate rather than in `zink_bind_vertex_elements_state`, because three separate
sites set `vertex_state_changed` (elements bind, enabled-mask change, and `zink_context.c:1447`)
and the gate covers all of them without one being missed later.

Measured on the recorded stream, replayed with no VM and no compositor:

| | titles inking | card 1 body ink | other bodies |
|---|---|---|---|
| before | **1** of 12 | 2313 | 4668 each |
| after | **12** of 12 | 4668 | 4668 each |

Pixel-verified, not counted: `evidence/replay-title-fixed-res353.png` is the card that was
blank, now reading "Critical Updates 002" -- the same string llvmpipe produced from this trace.
Card 1's body is clean text again.

Upstream fork commit `b836c280506` on `limina-kk`. It is upstreamable as-is: the defect is
generic zink, not a KosmicKrisp workaround, and it hits every driver without
`EXT_vertex_input_dynamic_state` given an application that alternates two vertex layouts.

**Adding `EXT_vertex_input_dynamic_state` to KosmicKrisp is not an alternative to this fix.**
Metal compiles the vertex descriptor into the pipeline state object, so KK could only implement
the extension by caching pipeline variants keyed on vertex input -- what zink already does, moved
one layer down, buying no capability. It would also mask the bug rather than fix it. Worth
considering separately as a pipeline-permutation reduction; worth checking against MTL4 first.

## Next

The fault reproduces in a single-process host program with no VM, no compositor and no guest,
deterministically, and the host-implementation split runs inside it. Every question below is now
answerable by editing and re-running one binary.

1. **A git bisect is still NOT available, and llvmpipe is not a substitute for one.** It needs a
   known-good KK *revision*, and there is still no evidence this path ever rendered these cards
   correctly. llvmpipe is a different driver, not an earlier KK — it makes an excellent
   differential and a useless bisect endpoint. Do not start one without first establishing a good
   end.

   What the replay does change is the **cost of looking for one**: testing a candidate revision
   used to mean a boot and a live damaging session, and now it is one replay run. The cheapest
   probe worth taking is whether the fault survives with our own `limina-kk` commits reverted to
   their upstream base — one build, and it answers whether we introduced it. If any revision comes
   back clean, a real bisect opens up; until one does, the work is code-path narrowing, not history
   search.
2. **Find why a needed attachment is begun with CLEAR/DONTCARE.** The sweep has narrowed this from
   "somewhere in zink/KK" to one mechanism with a counter attached: `reload_hazard` in
   `kk_cmd_buffer.c`, 128 of them per replay. The question is now why the load action encoded for
   these passes discards content the next draw still needs — whose loadOp that is (zink's, or KK's
   own pass merging), and what makes card 1 differ.
3. **Narrow to the draw if that is not enough.** Two differentials are in hand, both
   about code paths rather than history: across drivers (llvmpipe renders, KK does not, same
   stream) and within one KK run (card 1 renders, cards 2-12 do not). Card 12's command stream is card 1's modulo handles -- same CLEAR, same
   sampler views, bit-identical viewport and constant-buffer values, same `BIND_OBJECT 18`, same
   108-index draw at offsets 0/12/16, and title vertex buffers that are both 2304-byte resources
   taking a single 2304-byte upload. One renders and one does not, so instrument KK on that draw
   and compare the two arms directly.
3. **The damage is not accumulated rasterisation.** `--draws-from 50000` drops every draw before
   card 12 while keeping every resource, transfer and state command; card 12 still loses its title
   and still renders its body. Whatever carries it is not the earlier cards' drawing -- which makes
   per-draw state inside KK, not wear, the thing to look at.

## Traps this rig exists to avoid

Each produced a confident, wrong reading.

- **A shader dump taken twice is a PARTIAL dump the second time.** `KK_LIMINA_SHADER_DUMP` fires on
  compile, so a re-run serves the pipelines from zink's on-disk shader cache and writes only the
  tables it happened to recompile -- silently. The first `glyphmimic` gate run emitted three vertex
  shaders; the next emitted one, and the glyph pipeline's table, the one the gate exists to check,
  was simply absent. Read that way it says "the failing pass is gone". Always pass
  `MESA_SHADER_CACHE_DISABLE=true` with the dump lever, and count the tables you expect.

- **A vehicle that never fails may be a vehicle that cannot fail.** Every `glyphmimic` arm returning
  90/90 clean is only evidence because `GM_NODRAW` -- the same code with the glyph draw omitted --
  returns `text-lost=90` with `samples=0` on all 90. Without that positive control, an ink detector
  that always fires and a genuine cure print the identical verdict. Prove the oracle can say the bad
  thing, per platform, before believing the good thing.

- **The worker log's `scanout 0 -> IOSurfaces` line goes stale, and nothing says so.** A gdm restart
  mints a fresh scanout pool without emitting a new line, so the newest line in the log still names
  the *previous* session's surfaces. Scoring against them read 20/20 `NO_CARD` -- printed as "0
  damaged", which is the same string a cure produces. `iosscan` is the oracle: the live desktop
  surfaces carry `nz` in the tens of thousands, a stale boot-console frame carries ~1000, and
  `dumpone.sh <id>` settles it by eye in one command. Take the ids from a census, never from the log.

- **Verify an env change inside the target process, and verify the *renderer*, not just the env.**
  `/etc/environment.d/` reaches an ssh session but **not** gnome-shell (the systemd *user* manager
  persists across logins, so restarting gdm leaves the old environment). `glxinfo` over ssh then
  reports `llvmpipe` for **its own process** while gnome-shell stays on virgl — both "arms" are the
  same arm, which is exactly why the control matched. Read
  `tr '\0' '\n' < /proc/<shell-pid>/environ`, reboot rather than restart gdm, and confirm the
  renderer actually changed.
- **A lever whose engagement cannot be observed is worse than no lever.** virglrenderer's own log
  defaults to WARNING *and* its output does not reach the limina worker log at all, so the absence
  of a `virgl_info` line proves nothing. `LIMINA_VREND_TRANSFER_FORCE_SYNC` announces itself on
  stderr for this reason.
- **A single leftover CRITICAL notification pins the banner slot forever.** GNOME never
  auto-dismisses `-u critical`, so every later notification queues behind it and the probe measures
  one stale card indefinitely — with entirely plausible, entirely constant ink. Flush the queue
  first, never send critical. **Flush a wide id range**: ids climb through a long session, and a
  too-low range silently leaves a resident card (e.g. GNOME's own "Support GNOME") owning the slot,
  so the probe measures *that* card instead of the posted one.
- **Liveness must be measured against a pre-send baseline, not sampled in the moment.** By probe
  time the banner is already static, so nothing differs over a short window and the probe falls back
  to whichever stale surface still holds an old card.
- **The scanout is a rotating pool AND the pool is re-created over a VM's life** (183 → 255 → 260 in
  one session). An id from a log line goes stale silently and every trial then reports *identical*
  ink. Treat invariance as a broken oracle until proven otherwise.
- **`IOSurfaceGetSeed` is not a liveness oracle here.** It advances for CPU lock-based writes, not
  for the GPU writing the scanout blob. Diff actual content.
- **`LIMINA_WINDOW_CAPTURE` is the wrong oracle for this guest path.** It fires once per 120
  *presents*, and mutter here flushes the **same** framebuffer rather than swapping, so the counter
  barely advances while the screen updates fine.
- **GNOME withholds notification *banners* while its idle monitor says the user is away** — they go
  straight to the tray, nothing repaints, and an unattended loop measures a frozen screen while
  reporting healthy numbers. Defeat idle and *verify* it (`org.gnome.Mutter.IdleMonitor.GetIdletime`).
  A host cursor warp does not reset it; a forwarded keystroke does.
- **The overview silently turns damaged samples into clean ones.** The header strip is calibrated
  against a banner on the plain desktop; in the Activities overview the banner sits lower and that
  strip lands on the shell's search entry, whose ink scores as a present header. One Escape at
  startup is not enough -- a run that re-enters the overview partway through becomes a stream of
  false negatives, and one such run came back at 30% against a 29-77% baseline and briefly read as a
  lead. `calib.sh` now leaves the overview before every post. **Confirm a suspicious arm against the
  saved PNGs before believing it**: it was a full-frame capture, showing wallpaper and top bar, that
  distinguished "the overview is up" from "the card lost its header".
- **A constant notification string measures nothing after the first card.** cogl renders the glyphs
  once and later cards reuse the cached texture. Vary the text every trial.
- **Parse tool output by label, never by field position.** Adding two fields to the probe's output
  silently shifted every column and reported healthy cards as damaged.
- **`pkill -f <pattern>` over ssh matches its own shell's command line** and kills the rest of the
  remote command. Bracket the pattern (`notify-loo[p]`).
- **An arm that breaks rendering entirely exits 0 and looks like a completed run.** The
  `VIRGL_DISABLE_MT` measurement scored 10/10 `NO BANNER` and returned exit code 0: the compositor
  was alive (`org.gnome.Shell` `active`, load ~2) but never presented, so every post was correctly
  discarded and nothing anywhere said "this arm is broken". The validity gate is what saved it --
  without a gate this reads as 100% damage, and with only a title-strip score it reads as clean.
  **Read the gate's discard count, not just the pass/fail line**: an all-discarded run is a broken
  rig, never a measurement.
- **The guest DRM plane's `fb=` id staying constant does not mean the compositor is idle** — on this
  path mutter updates the same framebuffer in place.
