# Tier-2 seated-desktop render defect: cogl batched-quad path (OPEN)

> ⚠️ **REFRAME 2026-06-09 (read first — supersedes the "RASTERIZATION/STATE" framing below).**
> Two corrections invalidated several conclusions in the body of this doc:
> 1. **The scene is UNSTABLE when the bug reproduces** (larger regions paint→decay→only damage repaints) —
>    NOT just under `disable-texturing`. So single-frame full-scene scanout grabs are unreliable. (The
>    dash/dock-icon "bottom-left-triangle-only" pattern IS stable and trustworthy; window-thumbnail/large-area
>    reads are not.)
> 2. The cogl pixel-knobs gave no clean answer: `wireframe` no-op, `disable-texturing` destabilizes,
>    `disable-blending` ambiguous. **RETRACTED: "tri2 covered", "tri2 not covered", "GPU gets PERFECT data →
>    post-raster rasterization-STATE kill."** Those rested on host-side reads + a (false) stable-scene
>    assumption.
>
> **The ONE solid causal fact:** `disable-batching` is the only intervention that has ever changed the
> outcome → the bug lives in the batched **indexed `TRIANGLES`** draw over cogl's **shared rectangle index
> buffer**; the per-quad **fan** path (no index buffer) is immune. **SOLID eliminations (instrument,
> frame-independent):** depth, stencil, clip, cull, uint8, first-vertex offset. **OPEN:** the data/coherency
> line at the GPU level (host-side reads don't prove GPU-side), and whether tri2 is even rasterized.
> **NEXT:** instrument the working `disable-batching` path and diff the Metal-level draw vs the broken batched
> path. Authoritative running state: memory `limina-tier2-venus`.

Status: **root localized, not yet fixed.** 2026-06-09.
Context: [[tier2-coexist-gpu]], [[tier2-host-visible-coherency]]. Prereqs: bug A (#31/#28) fixed, #30
zero-copy present working — the seated GNOME desktop renders on venus (zink→venus→MoltenVK→Metal,
custom 16 KiB-page guest kernel, Fedora 43 / GNOME 49 / mutter 49.5).

## TL;DR

The seated desktop renders, but **windows, the top panel, and small icons render broken** — each
textured quad shows only its **bottom-left triangle**; the top-right triangle is missing on a clean
TL→BR diagonal, and the texture is *correct* in the half that draws. The **background/wallpaper is
never broken**.

**Decisive finding:** `COGL_DEBUG=disable-batching` in gnome-shell's environment makes the whole
desktop render correctly (dock, panel, window chrome all whole). So the bug lives in **cogl's batched
journal flush** — the `batch_len > 1` path that draws many quads as one **indexed `GL_TRIANGLES`**
call. With batching off, every quad is drawn individually as a non-indexed `TRIANGLE_FAN`, and is fine.

This is a *diagnostic*, not the fix (batching exists for performance, and a few residual artifacts
remain even with it off). The real fix must target the indexed batched draw through our stack.

## What was observed (verified, not assumed)

- User report: "massive tearing all over + most icons/widgets split in 2 triangles, only the
  bottom-left rendered." Files window broken, top panel broken. **Background always fully rendered.**
- Symptom geometry: the **second** triangle of each quad drops. In cogl's vertex order
  TL,BL,BR,TR with index pattern `{0,1,2, 0,2,3}` per quad: tri1 `{TL,BL,BR}` (bottom-left) survives,
  tri2 `{TL,BR,TR}` (top-right) is absent. Clean diagonal; surviving half's texels are correct.
- Size-dependent: small icons consistently broken, large surfaces fine. (See uint8 threshold below.)

### ⚠️ A retracted claim, kept as a lesson
Earlier this investigation claimed "bug 1 (tearing) is fixed" after pixel-verifying that the
**wallpaper** rendered clean. That was wrong: the wallpaper was *never* broken, so a clean wallpaper
proves nothing. Verify the pixels that actually exhibit the defect (dock/panel/window), not a proxy
region. (CLAUDE.md: "pixel-verify; proxies lie" — the proxy here was the wrong region.)

## The decisive experiment

Set `COGL_DEBUG=disable-batching` for gnome-shell (via `~/.config/environment.d/`, then a **fresh
first-login** — restarting gdm alone does not re-import environment.d; a clean worker boot on the same
disk does). **Confirm the var is actually in `/proc/$(pidof gnome-shell)/environ` before trusting the
capture.** Then read the venus scanout IOSurface host-side (`spikes/venus-draw-probe/iosdump.swift`).

Result: dock icons, top-panel text + status icons, and window/workspace thumbnails **all render whole**.
Captured frame `/tmp/ios-39.png` (overview) — clean across the board.

## Where the bug lives (code)

`third_party/mutter/cogl/cogl/cogl-journal.c`, `_cogl_journal_flush_modelview_and_entries`:

```c
state->current_vertex += (4 * batch_len);   // 4 verts per quad in the VBO (l.392)

if (batch_len > 1) {                                            // MANY quads batched
    CoglVerticesMode mode = COGL_VERTICES_MODE_TRIANGLES;
    int first_vertex = state->current_vertex * 6 / 4;          // NON-ZERO index offset
    _cogl_framebuffer_draw_indexed_attributes(framebuffer, pipeline, mode,
        first_vertex, batch_len * 6, state->indices,           // shared rectangle index buffer
        attributes, state->attributes->len, draw_flags);       // <-- BREAKS on venus
} else {                                                        // SINGLE quad (e.g. background)
    _cogl_framebuffer_draw_attributes(framebuffer, pipeline,
        COGL_VERTICES_MODE_TRIANGLE_FAN, state->current_vertex, 4,
        attributes, state->attributes->len, draw_flags);       // <-- always fine
}
```

The shared index buffer is **uint8 when ≤ 64 rectangles**, else uint16
(`cogl-indices.c:130` — `n_indices <= 256/4*6` i.e. ≤ 384 indices = ≤ 64 quads;
`cogl_context_get_rectangle_indices`, pattern `{0,1,2, 0,2,3}` per quad). Dock/panel/icon batches are
all ≤ 64 rects ⟹ **uint8** indices — which is why "small icons" specifically break.

The journal stores **2 diagonal corners per quad** in `_cogl_journal_log_quad` (positions at `v` and
`v+stride`); the 4 corners are reconstructed and uploaded to a pooled `CoglAttributeBuffer` at flush.

### Why window/panel/icons hit it but the background doesn't
- **Background** (`meta-background-content.c` `paint_clipped_rectangle`): one
  `clutter_paint_node_add_texture_rectangle` per region rect → typically a **single quad** →
  `batch_len == 1` → `TRIANGLE_FAN` branch → fine.
- **Window content** (`meta-shaped-texture.c` `do_paint_content` /
  `paint_clipped_rectangle_node`): emits a `clutter_paint_node_add_multitexture_rectangle` **per damage
  / opaque / blended region rect**; the shell batches many such small quads → `batch_len > 1` → the
  indexed branch. (Note: the `coords[8]` hardcode in `paint_clipped_rectangle_node` is *not* a bug —
  it's upstream code that works on real GPUs; for single-plane textures the journal reads layer count
  from the pipeline and ignores the spare coords.)

## Ruled out (all pixel-clean vehicles, `spikes/venus-draw-probe/`)

Every standalone vehicle that draws quads **per-rect** is clean, which is exactly why they all passed
before — none exercised the batched indexed flush:
- `primtest.c` — per-rect `TRIANGLE_FAN` / `TRIANGLE_STRIP` / `TRIANGLES`, ± back-face cull, ± non-zero
  first-vertex (untextured).
- `texfan.c` — per-rect **textured** fan, per-rect indexed list, **and `MODE=batch`** (one
  `glDrawElements`, **uint16**, cogl's exact vertex order TL,BL,BR,TR + `{0,1,2,0,2,3}`, offset 0).
- `quads.c` / `fbotex.c` — indexed triangle list, FBO + texture.
- `vkcoh-*.c` — host-visible coherency probes (fully coherent).

The only vehicle that reproduced *any* corruption is `u8test.c` (uint8 indexed grid). **But** the
venus uint8 extension is currently **disabled** (fork commit `bf65849`, `vkr_common.c`
`KHR/EXT_index_type_uint8 = false`) and, per the prior record, `u8test` renders clean on that build —
yet the desktop still breaks. **⇒ re-verifying `u8test` on the current build is the linchpin
experiment** (see theory U below): if it's clean, the desktop bug is *not* the uint8 path.

## Theories — the deltas between the broken batched draw and the clean vehicles

The batched draw `draw_indexed_attributes(TRIANGLES, first_vertex=current_vertex*6/4, batch_len*6,
shared_indices)` differs from the clean `texfan MODE=batch` (uint16, offset 0, indexed TRIANGLES) in
exactly these axes. Each has a discriminator and a prediction.

| # | Theory | Mechanism | Discriminator | Result |
|---|--------|-----------|---------------|--------|
| **U** | **uint8 indices** (zink layer) | cogl makes a GL uint8 index buffer; zink converts uint8→uint16 (venus uint8 disabled); a bad conversion would hit ≤64-rect batches = small icons | `u8test U8=1` on this build (`/tmp/ios-41.png`) | ❌ **DISPROVEN** — perfect 8×8 grid, uint8 is clean |
| **O** | **non-zero `first_vertex` offset** | `current_vertex*6/4` starts the indexed draw partway in; offset base may be mishandled | `texfan MODE=batch IDX8=1 OFFSET=1` — group A offset 0, group B non-zero (`/tmp/ios-75.png`) | ❌ **DISPROVEN** — both groups clean, even uint8+offset |
| **L** | **journal 2→4 vertex expansion + layout** | `upload_vertices` logs 2 diagonal corners/quad, expands to 4 (v0,v1,v2,v3) in a specific interleaved color+pos+tex layout | the instrument dumps the expanded verts the GPU fetches (`[LIMINA-Q]`) | ❌ **DISPROVEN** — expansion is exact (v0..v3 correct positions+texcoords); see "INSTRUMENTED THE REAL DRAW" |
| **C** | **a LARGE per-frame MAPPED multi-quad buffer fetched incoherently by the host GPU** (and/or the indexed draw reading from it) | `upload_vertices` (cogl-journal.c:1162) **maps** a pooled `DYNAMIC` `CoglAttributeBuffer` every frame (`_cogl_buffer_map_range_for_fill_or_fallback`) and writes the 2→4 expanded verts — host-visible mapped-buffer = **#28 territory**. My vehicles use `glBufferData(STATIC_DRAW)` and never map per frame. | `mapquad MAP=1`, then the instrument's dump of the bytes the GPU actually fetches | ❌ **DISPROVEN** — `mapquad` clean (weakened it); the instrument then proved the GPU fetches PERFECT data, killing it outright. See "INSTRUMENTED THE REAL DRAW" |
| **P** | **CPU software-transform fill path** | the verts are CPU-transformed via `transform_points` during the per-frame fill | `COGL_DEBUG=disable-software-transform` (`/tmp/dock-swxform2x.png`, env exact-confirmed) | ❌ **DISPROVEN** — dock still broken; GPU-side transform doesn't help |

### ⚠️ Corrected reasoning — what `disable-batching` actually does
An earlier draft claimed the cause is "the **tail vertex v3** of each quad goes stale ⇒ tri2 drops."
**That mechanism is NOT established** — it assumed the fan path reuses the same buffer, which it does
not. `disable-batching` has exactly **one** effect site (cogl-journal.c:1635): at the end of
`_cogl_journal_log_quad` it calls `_cogl_journal_flush` **after every quad**, so each flush has
`n_entries == 1` → `upload_vertices` **still maps a buffer** (just for one quad) and the draw becomes
`TRIANGLE_FAN`. So **disable-batching does not bypass the mapped-buffer path** — if "mapping is broken"
were the whole story, disable-batching (which also maps) would break too, and it's clean.

### (superseded — C is DISPROVEN; see "INSTRUMENTED THE REAL DRAW" below) Earlier reasoning that made C the standout
⚠️ This section made C the leading suspect; the instrument later **killed it** (the GPU fetches perfect
data). Kept for the reasoning trail only — do NOT treat C as live.

U, O, and the software-transform fill (P) are disproven. The three data points:

| case | buffer | quads | draw | result |
|------|--------|-------|------|--------|
| real batched | **mapped** | **many** | indexed | **BROKEN** |
| `disable-batching` | mapped | one | fan | clean |
| `texfan MODE=batch` | **static** | many | indexed | clean |

No single existing test isolates one variable, but together they corner it: vs `texfan` the delta is
**static→mapped** (many-quads + indexed held constant); vs `disable-batching` the deltas are
**many-quads + indexed** (mapping held constant — both map). So the bug needs **a large per-frame
*mapped* multi-quad buffer** (and/or the indexed draw reading from it); a *tiny mapped* buffer is fine
(disable-batching), a *big static* buffer is fine (texfan). This still fits "small icons in big batches
break, big single-quad background doesn't." **Likely a residual of #28** on the per-frame-mapped journal
buffer (not the one-shot blobs #28's fix covered).

### map-per-frame vehicle result: CLEAN (C weakened) — `spikes/venus-draw-probe/mapquad.c`
`mapquad MAP=1` (glMapBufferRange WRITE|INVALIDATE a `DYNAMIC` buffer, 64 quads, indexed `TRIANGLES`, 60
frames with swaps) renders a **perfect grid** (`/tmp/ios-69.png`). So the mapped-multi-quad path does
**not** reproduce the desktop bug.
- ⚠️**Confound:** this mesa-zink EGL/GBM exposes **no ES3 config**, so the context fell back to **ES2**.
  `glMapBufferRange` still resolved and succeeded (`map_fail=0`), and zink backs every GL buffer with a
  host-visible VkBuffer, so it very likely *did* exercise the coherency path — but a mesa staging/shadow
  fallback can't be 100% ruled out. So C is weakened, not cleanly killed.

## ⭐ INSTRUMENTED THE REAL DRAW — it's RASTERIZATION/STATE, not data (2026-06-09)
Built the instrumented MoltenVK (`spikes/venus-draw-probe/mvk-instrument.patch`, `rebuild-mvk.sh`,
`VK_ICD_FILENAMES`), booted the seated desktop with it (`boot-seated-mvkinst.sh`,
`LIMINA_IDX_DUMP=1 LIMINA_VTX_DUMP=1 LIMINA_VTX4=1`), and **verified the bug reproduces under the instrument**
(dock icons still bottom-left triangles — premise checked, the capture is of the genuinely broken render).
Filtered the firehose to cogl's rectangle batches via the index signature `idx0-5: 0 1 2 0 2 3`.

For every broken cogl quad, the data Metal receives is **PERFECT**:
- **Indices** (`[LIMINA-IDX]`): `prim=3` (triangle), `idxType=0` (uint16), `idxCount = 6·nquads` (both
  triangles submitted), `maxIdx = 4·nquads−1`, `restart=0`, `idx0-5 = 0 1 2 0 2 3`.
- **Positions** (`[LIMINA-Q]`, position binding): every quad a flawless rectangle,
  e.g. `(-46.38,25.76) (-46.38,-27.42) (-41.16,-27.42) (-41.16,25.76)` = TL,BL,BR,TR with **`v3`=TR exact**.
- **Texcoords** (`[LIMINA-Q]`, texcoord binding): `(0,0)(0,1)(1,1)(1,0)` = TL,BL,BR,TR, **`v3`=`(1,0)` exact**
  (atlas sub-`0.5` ranges also well-formed).

⇒ **The GPU is handed geometrically and texturally correct data for BOTH triangles, yet tri2 produces
no visible output. This kills the entire data/coherency line — C, stale-v3, index, texcoord all dead.**
It is a **rasterization / pipeline-STATE** defect. Winding is identical for both triangles (both CCW,
same signed area), so it is **not** simple back-face culling either.

### What the disable-batching interaction tells us — a screen-space clip is FALSIFIED
Grounding deduction (the right question: how does `disable-batching` interact with a post-raster kill?).
Batched (broken) and `disable-batching` (clean) submit the **identical two triangles at identical pixels** —
same v0..v3 (both run the same `upload_vertices` 2→4 expansion), same coverage, same winding. The only
differences are **non-geometric**: indexed-`TRIANGLES` vs `TRIANGLE_FAN`, batch size / first-index offset,
and the per-batch shared state setup. Two consequences:

1. **Any kill keyed to *where tri2 lands* on screen** (scissor, stencil, depth, a position-based `discard`,
   a clip plane on v3) **would fire in the fan path too** — tri2 occupies the same pixels there. It doesn't.
   So the killer is **not** screen-position-keyed. (Retract the earlier "almost certainly a clip keyed to
   the geometry" framing.)
2. **The defect's pattern makes a screen-space clip impossible anyway:** it drops the **NE half of every
   quad in quad-local coordinates**, wherever that quad sits. No single screen-space clip region is "the
   lower-left triangle of every quad at once" (a scissor is one rectangle; nothing writes a per-quad
   diagonal stencil mask). So the kill is **structural — the 2nd sub-triangle of each quad** — keyed to the
   indexed multi-quad draw shape, not to screen geometry.

But `texfan` already does indexed multi-quad `TRIANGLES` (with offset, uint8) **cleanly** — so the draw
*structure* alone isn't sufficient. The real batch must also carry **per-batch bound STATE that `texfan`
doesn't replicate**: the real multitexture/blend pipeline, the cogl clip stack active on that batch, or the
exact interleaved color+pos+padded-texcoord vertex layout. Batching is what binds that state once across
many quads; `disable-batching` re-establishes it per quad and the defect vanishes.

**Live mechanism:** *indexed multi-quad draw* × *the real shell's per-batch pipeline/state* → the structural
2nd sub-triangle executes to zero coverage on Metal, despite perfect MoltenVK command-level data.

**Decisive next experiment (differential, not "find the clip"):** capture the **same icon** under batched
vs `disable-batching` and **diff the Metal render-pipeline descriptor** each draw binds — depth/stencil
descriptor, blend, color-write mask, two-sided/front-face, provoking-vertex. That bound state is the one
thing that provably differs between clean and broken and that `[LIMINA-RAST]` hasn't dumped. Secondarily, grow
`texfan` toward the real batch (real vertex layout → multitexture pipeline → push a clip) until it breaks,
which names the culprit state directly. (`[LIMINA-RAST]` already added cull/front/poly/raster/viewport/scissor
to the `[LIMINA-IDX]` site; one residual curiosity is a negative-height `vp0` on some batches, harmless with
cull off but worth confirming it isn't the discriminator.)

### Depth/stencil differential RESULT — depth, stencil, and clip are ELIMINATED (2026-06-09)
Added `[LIMINA-DS]` (`liminaDumpDepthStencil`, gated `LIMINA_DS_DUMP`) to **both** draw sites — the indexed
(batched, broken) and the non-indexed (fan) — dumping the Metal depth/stencil descriptor + stencil ref +
provoking-vertex. Booted the seated desktop (instrumented MoltenVK, no net), 1219 `[LIMINA-DS]` lines over a
seated session that **reproduces the bug** (scanout IOSurface 69/72). Result — **uniform and fully benign on
every single draw**:
- **depth `cmp=7` (Always), `write=0`** → depth test is a no-op.
- **`stencilTest=0`** on all 1219 draws, all stencil ops Keep, masks `0xffffffff`, front+back identical →
  **no stencil / no clip anywhere in the session** (12 draws carry a stencil `ref=1/1` but with the test off
  it's inert; no draw ever has `stencilTest=1`, so cogl is not stencil-clipping at all here).
- `provoke=0` uniform.

⇒ **Depth-test, stencil, and clip are conclusively OUT** — and this is conclusive *from the batched run
alone*: you can't have less depth/stencil than "off," so a `disable-batching` run cannot reveal a
depth/stencil difference. Combined with the prior `[LIMINA-RAST]` (`cull=NONE`, rasterization on, scissor
covers the region), **none of Metal's fixed-function fragment tests are dropping tri2.** The "cogl clip-stack
on the batch" branch of the live mechanism is dead.

**What that forces.** With cull / depth / stencil / scissor / rasterizer-discard all benign AND the submitted
geometry correct, a *covered* fragment can only be dropped by the **fragment shader (`discard`), color-write
mask, or blend** — but those are per-pipeline and would hit tri1 identically, so they can't selectively kill
tri2. The one remaining selective explanation is that **tri2 is never actually covered** — i.e. its
coverage/geometry is degenerate *at GPU execution* even though the host-side instrument reads v0..v3
correctly at bind time (the `[LIMINA-Q]` reads are the host CPU view; for a Shared-storage buffer host==GPU, so
confirm the storage mode — if Shared, tri2 *is* covered and we're back to a genuine Metal rasterization quirk
on the batched indexed draw; if not Shared, a stale GPU-side copy of a tri2-only vertex like v3 is back in
play). **Next experiment = COVERAGE test: is tri2 rasterized at all?** `COGL_DEBUG=wireframe` (needs the
environment.d inject + fresh login; capture the dock/window): missing-triangle edges ABSENT ⇒ geometrically
not covered (degenerate/collapsed at execution); both triangles' edges PRESENT with only one filled ⇒ covered
but its fragments are dropped. That single bit splits the last two hypotheses.

## (superseded) Earlier dead-end: vehicles exhausted
Every standalone vehicle is clean: static indexed, mapped indexed, uint8, non-zero offset, textured fan,
FBO, coherency probes. U/O/P disproven, C weakened. Yet the real batched indexed draw breaks and
`disable-batching` (which keeps clipping/pipeline but forces per-quad fan) fixes it — so the defect is
tied to the **batched indexed `TRIANGLES` draw with the real shell's full draw STATE**, which the
vehicles don't replicate. Remaining real-vs-vehicle deltas on that draw: the **multitexture/color/blend
pipeline**, the **clip stack** active during the draw, and the **exact journal vertex layout** (packed
color + pos + padded per-`n_layers` stride). Per CLAUDE.md ("instrument the stack you own"), the
decisive move is now to **instrument the real draw** rather than build more vehicles:

- **MoltenVK `[LIMINA-IDX]`/`[LIMINA-VTX]`** (`spikes/venus-draw-probe/mvk-instrument.patch`,
  `rebuild-mvk.sh`, boot via `VK_ICD_FILENAMES`): for a real broken icon's indexed draw, dump the draw
  state (prim, index count, index type, base) + the **index and vertex bytes the GPU actually fetches**.
  - verts/indices correct but tri2 absent ⇒ it's the **draw/rasterization** (state), not data.
  - tri2's vertices stale/zero ⇒ it's **data/coherency** after all (and we learn which bytes).
  The filtering challenge: select the small textured indexed-TRIANGLES draws among thousands per frame.
- Cheap knobs still worth a pass (no rebuild): `COGL_DEBUG=disable-software-clip`, `disable-atlas`,
  `disable-blending`, `disable-texturing`, `COGL_DEBUG=wireframe` (geometry-vs-texture confirmation).
  But note `disable-batching` already fixing it points at the batched indexed draw over clip/atlas.

Geometry/wireframe cross-check (theory T2 from the bisection): `COGL_DEBUG=wireframe` / `rectangles`
would show whether the missing triangle is geometrically absent (wireframe shows one triangle) vs
present-but-mistextured (full quad, wrong texels). The "texture correct in the surviving half + clean
diagonal" already argues **geometric** (one triangle not rasterized), but wireframe confirms cheaply.

## Open items
- **Residual artifacts** remain even with `disable-batching` (user-observed). So the batched path is the
  *main* culprit but possibly not the only one — characterize what's left after a fix.
- `disable-batching` is a workaround with a real perf cost; the fix must address the indexed draw.
- Fix layer, once root is pinned: zink uint8 conversion (theory U), or venus/MoltenVK indexed-draw
  base-offset handling (theory O), or coherency on the index/vertex blob (theory C). Keep it minimal
  and upstreamable (mechanism in the dependency).

## Methodology knobs (the bisection menu)

Runtime, no rebuild — set in gnome-shell env (`environment.d`, fresh first-login, **verify in
`/proc/<pid>/environ`**). Full inventory mapped from mutter 49.5 source:

- **Batching / journal:** `COGL_DEBUG=disable-batching` (decisive here), `disable-software-transform`,
  `batching`/`journal` (trace).
- **Clipping:** `COGL_DEBUG=disable-software-clip`, `clipping`/`stencilling` (trace).
- **Culling / damage:** `CLUTTER_PAINT=disable-culling`, `disable-clipped-redraws`, `redraws`,
  `damage-region`.
- **Atlas / texture:** `COGL_DEBUG=disable-atlas`, `disable-shared-atlas`, `disable-texturing`.
- **Blending:** `COGL_DEBUG=disable-blending`.
- **Visualize geometry:** `COGL_DEBUG=wireframe` / `rectangles`.
- ⚠️ environment.d rejects a file with an **empty assignment** (`CLUTTER_PAINT=`) — write only
  non-empty vars or the whole file is skipped (cost ~real time here).

Harness: `/tmp/cogltest.sh "<COGL_DEBUG>" "<CLUTTER_PAINT>" <tag>` (writes env, reseats) +
`/tmp/gatedcap.sh <tag> <ENV_TOKEN>` (waits for venus frames, **gates capture on the env token being
present in the shell**, dumps + crops the dock canary). Keep these — they enforce premise-verification.
