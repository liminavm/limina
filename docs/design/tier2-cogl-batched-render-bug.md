# Tier-2 seated-desktop render defect: cogl batched-quad path (OPEN)

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
| **L** | **journal 2→4 vertex expansion + layout** | `upload_vertices` logs 2 diagonal corners/quad, expands to 4 (v0,v1,v2,v3) in a specific interleaved color+pos+tex layout | replicate the exact expansion + interleave in a mapped-buffer vehicle | untested |
| **C** | **a LARGE per-frame MAPPED multi-quad buffer fetched incoherently by the host GPU** (and/or the indexed draw reading from it) | `upload_vertices` (cogl-journal.c:1162) **maps** a pooled `DYNAMIC` `CoglAttributeBuffer` every frame (`_cogl_buffer_map_range_for_fill_or_fallback`) and writes the 2→4 expanded verts — host-visible mapped-buffer = **#28 territory**. My vehicles use `glBufferData(STATIC_DRAW)` and never map per frame. | map-per-frame vehicle: `glMapBufferRange` a DYNAMIC buffer, write MANY expanded quads, draw indexed, loop frames | **next** |
| **P** | **CPU software-transform fill path** | the verts are CPU-transformed via `transform_points` during the per-frame fill | `COGL_DEBUG=disable-software-transform` (`/tmp/dock-swxform2x.png`, env exact-confirmed) | ❌ **DISPROVEN** — dock still broken; GPU-side transform doesn't help |

### ⚠️ Corrected reasoning — what `disable-batching` actually does
An earlier draft claimed the cause is "the **tail vertex v3** of each quad goes stale ⇒ tri2 drops."
**That mechanism is NOT established** — it assumed the fan path reuses the same buffer, which it does
not. `disable-batching` has exactly **one** effect site (cogl-journal.c:1635): at the end of
`_cogl_journal_log_quad` it calls `_cogl_journal_flush` **after every quad**, so each flush has
`n_entries == 1` → `upload_vertices` **still maps a buffer** (just for one quad) and the draw becomes
`TRIANGLE_FAN`. So **disable-batching does not bypass the mapped-buffer path** — if "mapping is broken"
were the whole story, disable-batching (which also maps) would break too, and it's clean.

### Standout suspect: C (refined)
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

### Decisive next test for C
**map-per-frame vehicle** — `glMapBufferRange` a `DYNAMIC` `GL_ARRAY_BUFFER`, write **many**
cogl-expanded quads (v0..v3 order), draw indexed `TRIANGLES`, loop several frames. It is the one combo
no vehicle hit (mapped + many quads + indexed), isolating exactly the static→mapped variable.
- Reproduces ⇒ mapped-multi-quad coherency confirmed (C).
- Stays clean ⇒ the bug is the indexed **draw** reading a mapped source, not the data → then instrument
  the real fetch with MoltenVK `[LIMINA-VTX]`.

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
