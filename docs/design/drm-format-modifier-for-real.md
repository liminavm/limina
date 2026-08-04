# Implementing `VK_EXT_image_drm_format_modifier` for real (virgl/KK)

Status: **design, pre-code — RE-OPENED** (2026-08-04). Task #19. Written after the modifier
pressure test (`spikes/modifier-necessity/RESULTS.md`), which dropped the two kernel patches and
left the Vulkan axis standing. The §BLOCKED verdict below is **superseded** — its findings were
real but mis-weighed; see §UNBLOCKED at the end.

## What this is supposed to buy

The ledger's stated durable fix for three carried patches:

| patch | what it does | collapses? |
|---|---|---|
| mesa 0010 | force-advertises the modifier ext in venus + answers it in-guest | **half of it** — see below |
| mesa 0015 | venus WSI present fix; residual is modifier-shaped | partially |
| virgl 0005 | normalizes `DRM_FORMAT_MODIFIER` tiling → `OPTIMAL` for KK | yes, if KK accepts modifier tiling |

## Findings that change the plan

### 1. mesa 0010 is two patches wearing one hat, and only one of them is about modifiers

Read the diff, not the title. It does two independent things:

- **(a) External-memory handle type for a renderer that has `KHR_external_memory_fd` but no
  dma-buf.** Advertises dma-buf *to the guest* while using opaque-fd *to the renderer*. This is the
  half the ledger's "without it: `caps.dmabuf=0` → gbm dumb buffer → gnome-shell SIGSEGV" note is
  about. **Absent from upstream tip** (`vn_physical_device.c` still only has the dma-buf branch).
  Load-bearing, and not modifier-related at all.
- **(b) The modifier fiction.** Moves `EXT_image_drm_format_modifier` into venus's *native*
  extension table, answers `vkGetImageDrmFormatModifierPropertiesEXT` locally with
  `DRM_FORMAT_MOD_LINEAR`, and overrides `vkGetImageSubresourceLayout` with a computed linear
  layout.

### 1a. (a) is NOT stale — but it collapses by the same lever as (b)

Tested 2026-08-04 (the hypothesis was that (a) predates dma-buf working and is now dead code).
It is not. On venus tip, `vn_physical_device_init_external_memory` sets `renderer_handle_type`
**only** under `renderer_extensions.EXT_external_memory_dma_buf` (line 1042), and vkr on macOS
never advertises that extension — it *injects* `KHR_external_memory_fd` instead
(`vkr_physical_device.c:329`, because KK has no native fd support and vkr emulates it over Metal
shared memory). So without 0010(a), `renderer_handle_type` stays 0, the gate at line 1132 fails,
and venus advertises neither fd nor dma-buf to the guest — the documented `caps.dmabuf=0` → gbm
dumb buffer → gnome-shell SIGSEGV path. **(a) is live and load-bearing.**

The real asymmetry is one layer down, in vkr: it already injects one emulated extension and not
the other. If vkr also advertised `EXT_external_memory_dma_buf` and accepted that handle type —
translating to opaque-fd host-side, which is exactly what 0010(a)'s own comment says currently
happens *in the guest* (`vn_device_memory_fix_alloc_info`) — then upstream's branch at line 1042
fires and **0010(a) deletes itself**.

So both halves of 0010 reduce to one principle:

> **Make the renderer advertise honestly, instead of patching the guest driver to compensate.**

(a) → vkr advertises + accepts dma-buf (emulated, as it already does for fd).
(b) → vkr/KK advertises the modifier extension, and upstream's passthrough gate does the rest.

Both land in virglrenderer/KK, which we own and are already upstreaming into — a better place for
the delta than Mesa's guest driver.

**Terminology, because it is easy to garble:** "(a)" and "(b)" always mean the two halves of
*mesa 0010*, i.e. guest-side venus patches. The vkr dma-buf advertisement is the **replacement**
for (a), not (a) itself. So (a) gets **deleted**, and what becomes upstreamable is the new
virglrenderer patch. An earlier draft of this doc said "(a) is upstreamable on its own merits" —
that was written while (a) still looked load-bearing, and it no longer holds: fixing this renderer-
side means upstream venus never needs a branch for a case only non-Linux renderers reach.

**Unconfirmed:** the vkr injection carries `TODO: Remove after mesa!40478 has had sufficient distro
uptake`. A search of mesa tip found a related *"venus: relax SYNC_FD semaphore export requirement"*
commit but nothing identifiable as !40478 — do not treat that TODO as ripe without checking.

### 1b. PROVEN (2026-08-04): the (a) half works, and 0010(a) is now dead code

Implemented and measured the same day. vkr's macOS injection block now advertises
`VK_EXT_external_memory_dma_buf` alongside the `VK_KHR_external_memory_fd` it already injected
(`spikes/modifier-necessity/virgl-inject-dmabuf-ext.patch`). Nothing else changed — no guest mesa
rebuild, 0010 still fully applied.

The oracle is direct rather than inferred: venus stamps `renderer_handle_type` into
`VkExternalMemoryImageCreateInfo::handleTypes`, so a probe at vkr's image-create read it back:

```
[LIMINA-VKR-HT] image external handleTypes=0x200 (DMA_BUF)
```

`0x200` is `DMA_BUF`. Before the change it would be `0x2` (`OPAQUE_FD`). Since 0010(a) is an
`else if` *after* upstream's dma-buf branch, upstream's branch firing means **0010(a) no longer
executes** — proven without rebuilding guest mesa.

Session verified on the enhanced.test image: venus live in the seated session (`Virtio-GPU Venus`),
gnome-shell up with **0** segfaults in the boot journal, guest sees
`VK_EXT_external_memory_dma_buf`, no `degrading to software-2D` / `ComponentError` in the worker,
and the desktop **human-confirmed correct** (the health checks alone would not have caught a
shear-class fault, per the same morning's lesson).

That the change was this small is itself evidence for the "advertise honestly" reading: vkr's memory
paths already accepted both handle types interchangeably (the export strip takes
`OPAQUE_FD|DMA_BUF`; imports arrive via the handle-agnostic `VkImportMemoryResourceInfoMESA`). Only
the advertisement was missing.

**Remaining to actually retire 0010(a):** promote the vkr change from spike patch to a real
`patches/virglrenderer/` entry (minus the probe `fprintf`), delete the (a) hunks from
`patches/mesa/0010`, rebuild the guest mesa RPM, refresh the enhanced images, and re-validate. The
ledger row for 0010 should split into (a) and (b) at the same time.

### 2. Upstream venus is strict passthrough, and that is the whole lever

`EXT_image_drm_format_modifier` is in upstream's **passthrough** table
(`vn_physical_device_get_passthrough_extensions`, tip line ~1337), and passthrough entries are
intersected with renderer support at `vn_physical_device.c:1409-1410`:

```c
} else if (passthrough.extensions[i] &&
           physical_dev->renderer_extensions.extensions[i]) {
```

`native_extensions` — where 0010(b) inserts — bypasses that gate. So:

> **If KK advertises the extension, stock upstream venus advertises it to the guest by itself.**
> 0010(b) then has nothing left to do and deletes itself, using upstream code rather than a
> replacement patch of ours.

That is a materially better outcome than "shrink the patch", and it is the actual argument for
doing this work.

### 3. KK already has `EXT_external_memory_metal`; it has no modifier support at all

Verified on mesa tip: `kk_physical_device.c` advertises `EXT_external_memory_metal = true`
upstream. Our contribution there was the `MTLTEXTURE` *handle type* inside it, not the extension.
`EXT_image_drm_format_modifier` appears nowhere in `src/kosmickrisp` on tip — this is a from-scratch
feature, not a gap-fill.

### 4. 0010's comments describe a world that does not exist

The patch says *"KK now supports VK_EXT_image_drm_format_modifier and LINEAR COLOR_ATTACHMENT
directly"*. KK does not, on tip or in our tree. Whatever happens to the patch, the comment is
actively misleading and should not be trusted by the next reader.

## The design question, and why it is not obvious

0010's own rationale is a real architectural position, not just a hack:

> *"In a VM, DRM modifiers are metadata for the guest DRM stack — the host renderer doesn't need
> the extension."*
> *"Guest memory mapping goes through blob resources, so the actual host layout is irrelevant."*

Today's MTLTEXTURE work is **evidence for** that position: the host layout is now genuinely
non-linear (an IOSurface-backed `MTLTexture`), the guest is still told `LINEAR`, and everything
renders correctly. The decoupling holds.

So "implement it for real" must answer: *if the host layout is deliberately opaque to the guest,
what does a host-side modifier actually mean?*

**Proposed answer.** A DRM modifier is an *opaque token identifying a layout* — that is its entire
job. KK can honestly expose exactly two:

- `DRM_FORMAT_MOD_LINEAR` — for images KK genuinely lays out linearly.
- one vendor/opaque token meaning "Metal's private tiling for this format" — which is a true
  statement about a layout KK controls and can reproduce.

This is not the same fiction relocated. The current fiction claims *linear* for something that is
not linear; the proposal names an opaque layout opaquely, which is what modifiers are for. The
guest keeps getting a predictable stride for DRM framebuffer bookkeeping via the linear modifier
when it needs one, and a real token when it does not.

## Measure before coding

Assumptions to falsify first — the modifier axis has already burned us once by building on
inferred premises:

1. **Does the guest ever use the claimed linear stride for actual pixel access**, or only for DRM
   framebuffer bookkeeping? 0010's comment asserts the latter; today's MTLTEXTURE result is
   consistent with it but does not prove it. Instrument `vn_GetImageSubresourceLayout` callers.
2. **How many modifiers does the guest WSI actually negotiate?** 0010 hardcodes one. If one is
   genuinely enough, the two-token design above is over-built.
3. **Does virgl 0005's normalization become unnecessary, or merely wrong?** KK accepting modifier
   tiling is necessary but maybe not sufficient — vkr still strips the modifier structs for other
   reasons (the `EXTERNAL_MEMORY_IMAGE_CREATE_INFO` unlink is a separate concern).
4. **What does mesa 0015's residual actually depend on?** The ledger calls it modifier-shaped; that
   has not been tested by removing 0010(b) and watching what breaks.

Recommended order: (2) then (1) — both are cheap counter/probe work on the existing rig — before
any KK code.

### MEASURED (2026-08-04): item 2 — the guest negotiates exactly ONE modifier, LINEAR

Probe in `vkr_image.c` logging the modifier structs that ride in on image create
(`[LIMINA-VKRMODLIST]`), seated F44 enhanced session:

```
LIST count=1: 0x0 (fmt=44 2560x1440 usage=0x80097)      ×3
```

`0x0` is `DRM_FORMAT_MOD_LINEAR`. Three per session, all allocation-side (`LIST` form), all
scanout-sized `B8G8R8A8_UNORM`. No `EXPLICIT` imports and no other consumers appeared.

**The result is partly circular and must not be over-read.** mesa 0010(b) is *what makes* the guest
ask for one modifier — it answers the extension inside the guest and hardcodes LINEAR ("we only
support LINEAR which has 1 plane"). So this measures *what the guest asks for under our own patch*,
not what a stock guest would negotiate against a renderer that advertised a real list. What it does
establish, and this is the useful part:

- The live traffic is **tiny** — 3 images per session, one format, one usage pattern. Whatever we
  implement has a very small surface to satisfy.
- Nothing in the current stack wants a *tiled* modifier. The two-token design proposed above
  (LINEAR + an opaque Metal-tiling token) is **over-built for observed demand**.

**Design revision: start with a single token.** KK advertises `VK_EXT_image_drm_format_modifier`
offering exactly `DRM_FORMAT_MOD_LINEAR`, for the formats where it genuinely produces a linear
layout, and reports subresource layouts truthfully. That is honest, matches observed traffic
exactly, and is enough for upstream venus's passthrough gate to light up — which is the whole point,
since that is what deletes 0010(b). A tiled token can be added later if a real consumer appears.

**Consequence worth stating plainly:** with MTLTEXTURE import already solving the *layout* problem
(the host no longer needs the image to be linear to share it), implementing the modifier extension
buys **patch deletion, not capability** — 0010(b) and virgl 0005. That is still worth doing, but it
should be scoped and justified as cleanup, not as a feature.

## BLOCKED (2026-08-04): the single-LINEAR-token design is not implementable as written

> **Superseded the same day — see §UNBLOCKED below.** The two findings stand as observations;
> the weights and the recommendation drawn from them do not.

Two findings from the KK source, both concrete, that invalidate the "start with a single token"
revision above. Recorded before writing any code.

### 1. Metal cannot render to a linear texture — and the one measured consumer needs exactly that

`kk_image_layout.c:226` (our kk 0002) deliberately lays DRM-modifier images out **tiled** whenever
they carry `COLOR_ATTACHMENT` or `DEPTH_STENCIL_ATTACHMENT` usage, because a linear Metal texture is
not a valid render target: `mtl_new_render_command_encoder_with_descriptor` returns nil and
`kk_encoder.c` asserts on it. Its own comment states the rationale — "in a VM the guest's DRM
modifier is host-meaningless … lay such attachment images out tiled".

Now decode the traffic actually measured in §"MEASURED item 2": `usage=0x80097` =
`TRANSFER_SRC | TRANSFER_DST | SAMPLED | **COLOR_ATTACHMENT** | INPUT_ATTACHMENT`.

So the *only* modifier consumer we have ever observed asks for `DRM_FORMAT_MOD_LINEAR` **with
COLOR_ATTACHMENT usage** — the precise combination KK cannot honour linearly. Advertising LINEAR
would therefore be a *new* fiction replacing the old one: we would promise a layout we then
silently refuse to produce, which is exactly what this whole exercise set out to stop.

This inverts the earlier revision. "One honest LINEAR token" is not available; if anything is
advertised it has to be the **opaque Metal-tiling token** from the original two-token proposal —
because tiled is what KK genuinely produces for these images.

### 2. The common Vulkan runtime compiles modifier support out on macOS

`vk_image.h:79-90` guards `drm_format_mod` — the field a driver is supposed to fill in — behind
`#if DETECT_OS_LINUX || DETECT_OS_BSD`. On Darwin the field does not exist, so KK cannot use the
common runtime's modifier plumbing at all without either widening that guard upstream or carrying a
private field. That is a real upstreamability cost for a feature whose entire justification is
patch *deletion*.

### What this means for the task

The stated payoff was deleting mesa 0010(b) and virgl 0005 — cleanup, not capability (already
established above, since MTLTEXTURE import solved the layout problem). Against that we would now be
taking on: a from-scratch extension implementation, an upstream runtime guard change or a private
field, and an advertised token that is honest only if it is the opaque tiled one — which in turn
means the guest stops receiving the LINEAR answer that mesa 0010(b) currently hands it, so 0010(b)
does not simply delete itself; its consumers have to be re-tested.

**Recommendation: do not implement this now.** The cost/benefit has inverted relative to the plan.
Better options, in order:

1. **Leave 0010(b) + virgl 0005 in place** and revisit only if a real consumer needs modifiers.
2. If we do want it, implement the **opaque Metal-tiling token**, not LINEAR — and budget for the
   `vk_image.h` guard and re-testing every 0010(b) consumer.

## UNBLOCKED (2026-08-04): the BLOCKED verdict misread both the costs and the payoff

Re-examined with the MTLTEXTURE spike results in hand (`spikes/modifier-necessity/RESULTS.md`
§"Follow-on work" onward). Both blocking findings are real observations that were given the wrong
weight, and the recommendation they produced — do nothing, or the opaque token — points backwards.

### The "new fiction" reading is wrong: in the world we are moving to, LINEAR is the truth

- **The one measured consumer (LINEAR + COLOR_ATTACHMENT) is served today, correctly, by exactly
  that combination.** vkr's shipping path *forces* `VK_IMAGE_TILING_LINEAR` onto scanout-capable
  external images — COLOR_ATTACHMENT usage included — and KK renders into them every session:
  months of production, vkmark ~1520 in the forced-LINEAR arm, zero nil encoders. Whatever Metal's
  general rule on linear render targets, this configuration demonstrably renders.
  `kk_image_layout.c`'s own comment scopes the nil-encoder hazard to the buffer-backed *private*
  layout path and explicitly exempts the linear IOSurface-backed scanout ("renderable via
  newBufferWithBytesNoCopy … must keep its linear layout"). §BLOCKED finding 1 generalized the
  carve-out's comment into "Metal cannot render to a linear texture, period" — the very file
  refutes the generalization three lines up.
- **Under the MTLTEXTURE import (the architecture we are moving to), the shared image's storage is
  an IOSurface — physically linear at the surface's `rowBytes`.** The GTK4 shear was two *linear*
  aliases disagreeing about stride (a per-row skew — tiling disagreement would have produced block
  garbage, not a shear). A LINEAR modifier plus a truthfully-reported pitch is an exact
  description of those bytes. The fiction is gone, not relocated.
- **The usage=0x80097 measurement cannot be discounted as circular and simultaneously be
  load-bearing.** §MEASURED already flagged that the observed traffic is what our own 0010(b)
  manufactures; §BLOCKED then used that same traffic's usage bits as exogenous proof that LINEAR
  cannot be honoured. With KK advertising a real (truthful) list, the guest re-negotiates against
  it through upstream code.
- kk 0002's tiled carve-out stays right for modifier images that are *not* externally shared (no
  external-memory struct → no IOSurface backing). The honesty boundary is: advertise LINEAR for
  the IOSurface-shareable color formats, where a linear layout is genuinely what the guest-visible
  bytes are. **Checkpoint before code:** re-test the kk 0002 origin class (linear render encoder
  on a non-IOSurface-backed modifier image) on today's KK — the incident predates the vkr KK
  winsys work, and its result decides whether the carve-out stays scoped or can narrow to
  depth/stencil.
  **→ RAN, same evening: does not reproduce.** 3 modifier attachments went through KK genuinely
  linear (carve-out gated off), gnome-shell composited into them all session + a full vkmark
  suite, zero nil encoders/asserts. The carve-out can narrow; keep depth/stencil out of the
  modifier tables instead. Full write-up in `spikes/modifier-necessity/RESULTS.md`
  §"kk 0002 origin-class checkpoint".

### The opaque-token pivot points backwards

The modifier's guest-side consumers are gbm, KMS `ADDFB2`, and compositor policy. **All of them
consume LINEAR unpatched** (stock virtio-gpu KMS handles linear; the enhanced kernel advertises it
in `IN_FORMATS`; arm E showed compositor behaviour given LINEAR). **None of them can consume a
vendor Metal-tiling token** without new guest kernel `IN_FORMATS` advertisement plus compositor
acceptance — i.e. the opaque token *reintroduces the guest-side patch class this whole effort
exists to delete*, the same class the pressure test dropped that morning (linux 0002/0003).

The "re-test every 0010(b) consumer" cost §BLOCKED priced in belonged entirely to that pivot. With
LINEAR, the guest receives exactly the answer 0010(b) hands it today; what changes is the answer's
*source* — and the host-answered subresource layout is *more* correct than 0010(b)'s guest-computed
`w*bpp`: the two agree only while the IOSurface happens not to realign the pitch, which is
precisely the stride-disagreement class that sheared every GTK4 window. Deleting 0010(b) closes a
latent bug class; it does not open one.

### The vk_image.h guard is a modest carry, not a blocker

`drm_format_mod` has ~3 uses in `vk_image.c` (init to `MOD_INVALID`, the query reply). Widening
`DETECT_OS_LINUX || DETECT_OS_BSD` is a small commit on the `limina-kk` branch we already carry,
and it travels *with* the KK modifier feature when upstreamed — a normal constituent of "KK gains
modifier support", not a standalone tax. If upstream balks, a KK-private field is the fallback.

### The payoff was priced as generic "cleanup"; it is the publishing-critical currency

"Patch deletion, not capability" weighed all patches equally. They are not equal:

- **mesa 0010(b) is a guest mesa patch** — the expensive kind: RPM rebuild, delivery into guests,
  enhanced-image refresh, stale-image hazards, and the guest series length is what gates the
  limina-guest fork migration (task #19 → #24) on the road to publishing. The host-side deltas
  that replace it ship inside the app, in forks we already maintain and are upstreaming into.
- **With 0010 fully retired ((a) is already proven dead), stock upstream venus works against our
  renderer unpatched** — the passthrough gate lights up from the renderer's honest advertisement.
  That is stock-tier capability for every future distro mesa, per the two-tier tenet, and it is
  the same "advertise honestly instead of patching the guest" principle §1a/§1b proved with a
  three-line change the same day.
- Completeness item §BLOCKED missed: 0010(b) also natively advertises
  **`EXT_queue_family_foreign`** (constant-only, no entry points). Retiring 0010(b) must cover it
  — vkr injects it the way it injects dma-buf, or KK advertises it. Trivial either way, but it has
  to be on the list or 0010(b) cannot fully delete.

### Revised design (supersedes both the two-token proposal and the §MEASURED single-token revision)

1. **KK** (`limina-kk`): implement `VK_EXT_image_drm_format_modifier` advertising **LINEAR only**,
   for the IOSurface-shareable color formats, with truthful subresource layouts. Widen the
   `vk_image.h` guard on the branch.
2. **Pitch authority — one rule.** KK's linear `rowPitch` rule and vkr's IOSurface allocation must
   agree by construction (IOSurface-aligned rowBytes); EXPLICIT imports carry the exporter's pitch
   and KK validates it. This is what makes the reported layout *true* rather than coincident.
3. **vkr**: stop rewriting modifier tiling for KK (the virgl-0005-equivalent in-branch code dies);
   MTLTEXTURE import becomes the default (its own reviewable change, per the spike's remaining
   list); inject `EXT_queue_family_foreign` alongside the dma-buf advertisement.
4. **Guest mesa**: delete 0010 entirely ((a) hunks are dead code, (b) hunks are replaced by
   upstream passthrough); rebuild the RPM; refresh the enhanced images; re-validate — including
   the mesa 0015 residual test (§Measure item 4), which 0010(b)'s deletion finally unblocks.
5. **Acceptance**: the magenta/IOSurface oracle plus a shear-sensitive eyeball (grim screenshots
   of GTK4 windows at odd widths), a stock-image boot, and the gnome-shell-rs rig for pacing.

## Non-goals

- Reviving the two dropped kernel patches (linux 0002/0003). Those are punted to the "additional
  hardware planes" release and nothing here depends on them.
- Retiring the forced LINEAR. That is the MTLTEXTURE gate's job (separate, and already measured
  better on the compositor rig); it is not blocked on this work.
