# venus-draw-probe — is bug A (#31) all draws, or depth-specific?

`tri.c`: minimal GBM+EGL+GLES2+KMS probe. **No depth buffer.** Blue clear + one big red 2D
triangle (NDC, passthrough vertex shader, constant-red fragment shader), `drmModeSetCrtc`, hold.
Scans out as a venus blob → global IOSurface → read host-side with `iosdump`.

Build (in guest): `gcc tri.c -o tri -ldrm -lgbm -lEGL -lGLESv2 -I/usr/include/libdrm`
Run (gdm stopped so card0 master is free; patched zink env from [[limina-fedora-access]]):
`LD_LIBRARY_PATH=/opt/mesa-zink/lib64 ... GALLIUM_DRIVER=zink VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json VN_PERF=no_*_feedback ./tri`
then on host `iosdump <printed scanout blob id>`.

## Result (2026-06-08, dev-enh image, zink→venus→MoltenVK on M1 Max)

Program succeeded with **no errors**: `glGetError=0`, shaders compiled, `link=1`,
`eglSwapBuffers=1`, `addfb=0`, `setcrtc=0`, scanout = venus IOSurface 86.

`iosdump 86` → **`uniform=true`, every pixel `(0,0,255)` = solid BLUE. The red triangle is ABSENT.**

## Conclusion

A 2D, **no-depth**, single-triangle draw produces **zero fragments** while the clear lands
perfectly. So bug A is **ALL draws, not depth-specific** — the `VK_EXT_depth_clip_enable` warning
is a red herring for the cause. Rendering runs through zink→venus→MoltenVK; clears work, every
draw emits nothing, silently (no GL/EGL error). The blue clear was also visually confirmed on the
limina window.

## ROOT CAUSE — VERIFIED (2026-06-08): bug A IS #28 (host-visible coherency)

Built an instrumented host MoltenVK (`third_party/MoltenVK-src` @ v1.4.1; fprintf at the draw site
[`MVKCmdDraw.mm`] and the vertex-bind loop [`MVKCommandEncoderState.mm bindVertexBuffersTemplate`] →
`[LIMINA-DRAW]`/`[LIMINA-VTX]`), loaded into the worker via
`VK_ICD_FILENAMES=/tmp/mvk-instrumented/MoltenVK_icd.json` (ad-hoc codesigned dylib). Ran `tri`:

- `[LIMINA-DRAW] prim=3 firstVtx=0 vtxCount=6 instCount=1 cull=0 poly=0 rastDisabled=0 sc0=(0,0,1280x800)`
  → the Metal draw STATE is perfect (triangle, 6 verts, 1 instance, no cull, rasterization on, full
  scissor). Negative-height viewport `vp0=(0,800,1280x-800)` is the normal zink Y-flip, harmless with
  cull=none. ⇒ kills the points / rasterizer-discard / depth_clip theories.
- `[LIMINA-VTX] mtlidx=30 off=0 stride=8 len=1048576 storage=0(Shared) firstNonzeroByte=-1 v: 0 0 0 ...`
  → **the vertex buffer the GPU actually fetches is ENTIRELY ZERO** (scanned all 1 MB, not one nonzero
  byte). `storage=Shared` = the real memory the GPU reads. Not an offset bug.

⇒ every vertex = (0,0) → degenerate zero-area triangles → ZERO fragments → only the clear shows. The
guest wrote the quad into a host-visible buffer; the host MoltenVK sees zeros. **The guest's writes to
host-visible memory are not visible to the host GPU = host-visible blob coherency (#28).** Clears land
because they need no guest-written data. **#31 (bug A) and #28 are the SAME bug — fix #28 → render works.**

Rebuild/redeploy the instrument: `/tmp/rebuild-mvk.sh`; boot with it: `/tmp/boot-mvkinst.sh` (multi-user).
(Don't use `glReadPixels` as an oracle — #28 black readback; read the IOSurface scanout instead.
`occ.c` = an occlusion-query attempt; UNUSABLE here — this mesa-zink EGL/GBM gives no ES3 context.)

## #32 ROOT CAUSE — VERIFIED & FIXED (2026-06-09): broken stencil clipping, not damage accounting

Symptom: after the seated desktop settles, regions (search bar half, ~1/3 of the overview center)
decay to the background color; spreads with mouse activity. Mechanism chain, each link verified:

1. Ages are CORRECT (`buffer-age-test.c`: 0,0,0 then steady 3) and back-buffer content PERSISTS
   (`buffer-age-content-test.c`, incl. SCANOUT + SwapBuffersWithDamage) — the old "age lie /
   content discard" theory is DEAD.
2. Culling verdicts are CORRECT (instrumented `cull_actor` / `setup_clip_frustum`,
   `mutter-cull-instrument.patch`, env `LIMINA_CULL_LOG`): StEntry's eye-space box is constant and
   it's culled OUT only when the clip frusta legitimately exclude it.
3. The real break: when a clipped redraw has a **multi-rect** region, cogl refines the scissor
   (extents-only) with the **stencil buffer** (`cogl-clip-stack-gl.c`). On this stack the window
   framebuffer has **NO stencil**: the EGL config advertises depth=24/stencil=8 but
   `GL_STENCIL_BITS=0` and the fb is INCOMPLETE (`stencil-test.c`; venus exposes D32S8/S8 fine —
   zink/mesa-st substitution gap, Apple has no D24S8). All stencil ops are silent no-ops ⇒
   painting escapes the per-rect clip ⇒ background floods over correctly-culled widgets inside
   the scissor extents. Consistent with every observation: disable-clipped-redraws stable,
   disable-culling stable (single full-stage rect ⇒ no multi-rect refinement), decay lands on
   culled-but-inside-extents widgets.

FIX (`mutter-stencil-clip-fix.patch`, in-guest mutter 49.5 rebuild, user-verified stable):
- new `cogl_framebuffer_get_stencil_bits()` (cogl-framebuffer.{c,h}); NB the underlying driver
  query (`cogl_gl_framebuffer_back_query_bits`) early-returns WITHOUT writing `*bits` when
  `COGL_PRIVATE_FEATURE_QUERY_FRAMEBUFFER_BITS` is absent (it is, on zink's GL 2.1 compat
  context) — first attempt read uninitialized stack (stencil_bits=21845=0x5555) and never
  engaged; zero-init the struct so a failed query truthfully reads stencil=0.
- `meta-stage-impl.c`: before regenerating the redraw clip, if the region is multi-rect and
  `stencil_bits<=0`, degrade the region to its bounding extents (single rect ⇒ scissor-only
  path, which works). Cost: larger repaint area on those frames. `[LIMINA-CLIP]` log under
  `LIMINA_CULL_LOG` shows `-> DEGRADE-TO-EXTENTS` vs `-> stencil path`.
- `cogl-clip-stack-gl.c`: one-time g_warning if stencil clipping is attempted on a
  stencil-less fb (`has_stencil_buffer()` guard at both call sites).

DEEP FIX still owed (guest mesa/zink): an EGL config advertising stencil=8 must not yield an
incomplete fb — zink should substitute D32_SFLOAT_S8_UINT when D24S8 is missing. The mutter-side
degrade is correct robustness regardless (upstream candidate together with the garbage-read fix).
Guest libs live in the volatile clone (/usr/lib64/mutter-17/*, backups /root/mutter17-backup/) —
re-apply after re-clone; bake into .dev-enh.raw eventually.

## #32 deep-fix session 2 (2026-06-09 night): host exonerated for the stencil break; D24S8 emu added; u8 re-validated

Three independent verdicts from this session:

1. **The "no stencil attachment" premise from session 1 was a broken-oracle artifact** —
   see the previous section's correction. With the oracle fixed, the REAL guest-side
   failure shape is: window fb reports stencil=8/depth=24 COMPLETE, GL_OUT_OF_MEMORY
   ("Resizing framebuffer", renderbuffer AllocStorage fails), and zink begins rendering
   with **NO depth/stencil attachment at all** ([LIMINA-BEGREND] depthAtt=0x0
   stencilAtt=0x0 while [LIMINA-DSCOMP] shows the dynamic stencilTestEnabled=1 arriving
   fine). MoltenVK behaves per spec — the bug is GUEST-side, in the zink/KOPPER GBM
   window path (`kopper_allocate_textures` — note: this stack runs kopper, NOT plain
   dri2). zink-on-lavapipe (sw kopper path) works; zink-on-venus (hw kopper path)
   fails. gdb suggests the DS texture is created from a garbage
   stvis.depth_stencil_format, but guest mesa needs fprintf instrumentation (container
   rebuild) to pin it — NEXT STEP. A host-side workaround for THIS bug is impossible:
   the guest never sends stencil work to the host.

2. **Host D24S8 emulation added anyway (MoltenVK fork)**: setFormatProperties now lets
   D24_UNORM_S8_UINT advertise its substitute's (Depth32Float_Stencil8) caps on Apple
   GPUs, and both getImageFormatProperties gates accept substitutable formats
   (MVKDevice.mm). Verified end-to-end through venus: fmtprops optimal=0xde01, IFP2=0,
   create/alloc/bind OK (vkds.c). Kill switch LIMINA_NO_D24S8_EMU. It did NOT fix the
   stencil break (proving D24S8-absence wasn't the discriminator) but removes a whole
   class of missing-D24S8 fallback paths for stock guests. In mvk-instrument.patch.

3. **uint8-index hide RE-VALIDATED as necessary** (user was skeptical): reverted
   bf65849, rebuilt virgl, rebooted, ran u8test U8=1 → grid STILL corrupt (clustered
   stretched-triangle smear, evidence/u8-reenabled-still-corrupt-2026-06-09.png) on
   MoltenVK 1.4.1 with all current fixes. The hide stays, now gated __APPLE__ (fork
   commit 636848b). It is already a HOST-side patch (vkr extension table). Proper fix
   = MoltenVK's uint8->uint16 conversion compute pass (u8test.c is the repro).

Ops note: MoltenVK External deps (SPIRV-Cross etc.) link PREBUILT xcframeworks from
External/build; after editing External sources, rebuild via
`xcodebuild -project ExternalDependencies.xcodeproj -scheme ExternalDependencies-macOS ...`
BEFORE rebuild-mvk.sh, or the edit silently doesn't ship (cost one boot cycle).

## u8 ROOT-CAUSED (2026-06-10): MoltenVK's uint8→uint16 conversion has TWO bugs; sentinel mapping is the cogl killer

Following up the 2026-06-09 "hide re-validated" verdict with the actual mechanism. Vehicle:
`u8test.c` (U8TEST_N grid; N=8 → 256 verts, max index 255 = cogl's full-batch shape), u8
extension temporarily re-enabled in vkr, pixels read host-side via `iosdump`. New probe
`[LIMINA-U8CONV]` (env `LIMINA_U8_DUMP`) logs every conversion's `numIndices/tmpOffset/outBase`.

**Bug 1 — temp-buffer offset mismatch (REAL, latent, fixed).** `MVKCmdBindIndexBuffer::encode`
converts into a *pooled suballocation* (`getTempMTLBuffer`) but bound the compute kernel's
output at offset **0** (remainder path: `indicesConverted*2`) while the draw reads at
`uint16Buf->_offset` (MVKCmdDraw.mm). `[LIMINA-U8CONV]` showed live offsets cycling
0/1024/2048/…/9216 on a desktop boot — every nonzero one read stale pool bytes as indices.
Why it wasn't THE smoking gun: pool reuse means the stale bytes are usually a PREVIOUS
identical conversion's output, so corruption is intermittent and shape-preserving; u8test's
single visible frame happened to get tmpOffset=0 and was corrupt anyway. Fix: write at
`outBase = uint16Buf->_offset` (+ remainder), kill switch `LIMINA_U8FIX_OFF`. Verified
engaging (outBase==tmpOffset in log).

**Bug 2 — 0xFF→0xFFFF restart-sentinel mapping (THE deterministic cause).** The
`convertUint8Indices` kernel promotes legal index value 255 to Metal's restart sentinel
0xFFFF; Metal honors restart ALWAYS (cannot be disabled via public API), so any
restart-disabled draw whose u8 buffer legitimately contains 255 corrupts. cogl's full
64-rect batches are 256 verts with last index exactly 255 → the broken-shell symptom.
Upstream knows: a `convertUint8IndicesRaw` kernel exists but is only selected under
`MVK_USE_METAL_PRIVATE_API` (compiled OUT by default). A/B/C matrix (offset fix ON):

| run | max index | kernel    | pixels |
|-----|-----------|-----------|--------|
| N=8 | 255       | sentinel  | CORRUPT (diagonal smear, pixel-identical across boots — deterministic) |
| N=7 | 195       | sentinel  | clean  |
| N=5 |  99       | sentinel  | clean  |
| N=8 | 255       | **Raw**   | **CLEAN — all 64 quads complete** |

Fork fix: default to the Raw kernel (env `LIMINA_U8_SENTINEL=1` restores upstream behavior for
A/B). Raw is correct for every restart-disabled draw (incl. all list topologies — Vulkan only
allows restart on strips/fans); it would break only u8+restart-enabled apps, which the proper
upstream fix must handle by choosing the kernel at DRAW time (where restart state is known)
or converting u8→u32 (255 can never collide with the u32 sentinel).

Evidence: `evidence/u8-sentinel-and-offset-broken-2026-06-10.png` (corrupt N=8),
`evidence/u8-rawfix-clean-2026-06-10.png` (clean N=8). Both fixes in `mvk-instrument.patch`.

**Policy: the vkr u8 hide STAYS** (virgl fork 210c3f6 documents why): zink's CPU u8→u16
fallback is effectively free, while MoltenVK's conversion costs a render-pass split (full
tile store/reload on TBDR) per index-buffer bind, and stock MoltenVK builds in the wild
carry both bugs. Upstream-PR candidates: the offset fix (one-liner, uncontroversial) and the
sentinel fix (needs the draw-time restart-aware design).

## MVK_USE_METAL_PRIVATE_API enabled (2026-06-10): logicOp & friends light up, desktop clean — now the build default

Follow-on from the u8 root-cause: the Raw conversion kernel upstream gates behind
`MVK_USE_METAL_PRIVATE_API` is only one of a family of zink-targeted features. Built with
`make macos MAKEARGS="MVK_USE_METAL_PRIVATE_API=1"` (now the rebuild-mvk.sh default;
`LIMINA_MVK_PRIVATE_API=0` opts out, runtime lever `MVK_CONFIG_USE_METAL_PRIVATE_API=0`).
All encoder-level uses are respondsToSelector-guarded → silent degrade, not crash, if Apple
moves the private selectors. Verified on the seated desktop boot:

- **gnome-shell's zink warning lost `feats.features.logicOp`** — only
  `have_EXT_custom_border_color` remains ("Some incorrect rendering…" line, journalctl).
- **vulkaninfo in-guest (through venus):** logicOp, wideLines, legacyDithering,
  nonSeamlessCubeMap, primitiveTopologyListRestart, provokingVertexLast — all true.
- **Desktop canary clean**: overview seat, top bar, workspace thumbnails, dock 7/7 icons
  complete (evidence/private-api-desktop-clean-2026-06-10.png).
- Worker log clean (only the expected D24S8→D32S8 substitution notices from the emu path).

What this buys for GL correctness: native glLogicOp (XOR rubber-banding, legacy X11),
GL-default provoking-vertex-LAST flat shading, glLineWidth>1, GL_DITHER, pre-GL3.2 cubemap
seams, and honest restart control (the sentinel problem class disappears when restart is
genuinely toggleable). Caveat for future macOS bumps: failure mode of a removed private
selector is silent wrong rendering — re-run this section's three checks after OS updates.

## The "residual damage" day (2026-06-10): a half-installed fix, not a regression — and the path trap

A full day of A/B exonerations (private API on/off, D24S8 emu on/off, sampler fix reverted,
clipped redraws on/off, stock vs fixed mutter) all produced PIXEL-IDENTICAL "damage" (search
bar missing, workspace strip stuck off-center at idle seat). Every single one was testing an
UNMITIGATED stack, because the morning bake of the #32 mutter fix into dev-enh.raw installed
`libmutter-17.so.0.0.0` into `/usr/lib64/mutter-17/` — but **gnome-shell loads it from
`/usr/lib64/` directly** (unlike every other libmutter*.so). The lib containing
meta-stage-impl.c — the degrade-to-extents mitigation itself — was never loaded; only the
cogl zero-init was live (its one-time "no stencil buffer" warning firing made the install
look half-alive). The user's ground truth cut through it: at the 01:32 verification NO damage
was reproducible idle OR active, so it had to be a delta in the install, and the transcript
of the verified moment (line 2660) showed yesterday's install replaced the FULL lib set
including `/usr/lib64/libmutter-17.so.0.0.0`.

Completing the install (full set: cogl+clutter+mtk into mutter-17/, libmutter-17 at its real
path) immediately produced the first complete overview of the day — search bar, centered
previews, dock (evidence/overview-complete-after-full-install-2026-06-10.png) — and the user
could no longer reproduce the instability. The correct procedure is now encoded in
`install-mutter-fix.sh`.

Lessons, earned twice over:
- **A "verified engaging" sub-oracle is not a verified fix.** The cogl warning proved one
  PIECE was loaded; the load-bearing piece was elsewhere. Verify the artifact the fix lives
  in (file mtime/size at the path the process actually maps — /proc/PID/maps).
- **When every A/B comes back identical, stop toggling and re-check the baseline.** Identical
  results across N supposedly-different configs means the differential isn't reaching the
  system under test.
- **The user's episodic memory is an oracle.** "I could not reproduce it at any point" +
  transcript archaeology (the exact install commands at the verified moment) beat five boot
  cycles of host-side bisection.
- The exonerations remain valid in one direction: none of the overnight host changes CAUSE
  visible desktop damage on the unmitigated stack. All restored (private API default build).

## #32 DEEP BUG ROOT-CAUSED & FIXED HOST-SIDE (2026-06-10): MoltenVK DS + HOST_TRANSFER ⇒ memoryTypeBits=0

The "zink/kopper begins rendering with NO DS attachment" defect — the real bug under all the
seated-desktop stencil instability, mitigated until now by the mutter degrade-to-extents patch
— is a **MoltenVK contradiction**, visible in plain source, reachable by any zink guest:

1. `MVKImage::getMemoryRequirements` (MVKImage.mm:920) restricts every image of a
   DS-attachment-capable format to **private memory types only** (`_isDepthStencilAttachment`
   is format-level: set when the format's features include DEPTH_STENCIL_ATTACHMENT,
   MVKImage.mm:1224).
2. Same function (MVKImage.mm:929): if usage includes `VK_IMAGE_USAGE_HOST_TRANSFER_BIT`
   (0x400000), it **strips the private types** (host copies need CPU-visible storage).
3. DS attachment + HOST_TRANSFER ⇒ private ∖ private = **memoryTypeBits = 0** — spec-invalid
   (≥1 type is mandatory) and unallocatable.

zink adds HOST_TRANSFER to ANY image whose format advertises
`VK_FORMAT_FEATURE_2_HOST_IMAGE_TRANSFER_BIT` (zink_resource.c:476 at mesa pin 3515c52) —
and MoltenVK advertises it on every readable format including all DS formats
(kMVKVkFormatFeatureFlagsTexRead, MVKPixelFormats.mm:1516). venus relays
VK_EXT_host_image_copy with hostImageCopy=true, so the guest's window-system depth/stencil
buffer is created with usage 0x400027, requirements come back memTypeBits=0x0, allocation
fails ⇒ GL_OUT_OF_MEMORY ⇒ cogl gets no stencil buffer ⇒ multi-rect clips no-op ⇒ #32.
**Upstream zink KNOWS** — zink_resource.c:387 carries "MoltenVK cannot allocate a depth
buffer with VK_IMAGE_USAGE_HOST_TRANSFER_BIT_EXT" — but the workaround is inside
`#if defined(MVK_VERSION)`, compiled out on a Linux guest where the MoltenVK sits invisibly
behind venus. Observed live: stencil-test on the seated guest printed the exact
`vn_GetImageMemoryRequirements2 size=346112 memTypeBits=0x0` for fmt=129 via the image-less
maintenance4 query (no vkCreateImage — which is also why creation-time substitution never
saved it).

**Fix (MoltenVK fork, in mvk-instrument.patch):** stop advertising
`VK_FORMAT_FEATURE_2_HOST_IMAGE_TRANSFER_BIT` on DS-attachment-capable formats (end of
`MVKPixelFormats::setFormatProperties`). One change, two layers healed: zink stops adding the
usage (it checks format feats), and the existing usage/feature gate in
getImageFormatProperties (MVKDevice.mm:1537) now honestly rejects stray attempts, engaging
zink's designed fall-back-without-HOST_TRANSFER negotiation (emit_hic_variants). Kill switch
`LIMINA_HICFIX_OFF`.

**Verified (same guest, same mesa, same binary test):**
| config | DS memTypeBits | verdict |
|---|---|---|
| pre-fix (prior boot) | 0x0 | STENCIL BROKEN, glErr=0x505 |
| fix ON, D24S8 emu ON (default) | 0x1 | **STENCIL WORKS**, glErr=0 |
| fix ON, D24S8 emu OFF | 0x1 (via D32S8) | **STENCIL WORKS** — format-independent |

And the load-bearing first: **gnome-shell now boots with a REAL stencil buffer** — zero
"Stencil buffer not available" warnings, zero [LIMINA-CLIP] DEGRADE lines in the journal, on
both fixed boots. The mutter mitigation never engages; it stays as defense-in-depth.
The D24S8 emu is NOT needed for stencil (it remains for stock-guest native-D24S8 paths).

Upstream PR candidates from this: (a) MoltenVK — either don't advertise host-image-transfer
on DS formats or make the memory-requirements contradiction impossible; (b) mesa/zink — the
MVK_VERSION compile-time guard misses MoltenVK-behind-venus; detect at runtime (driver
props) or recover when memoryTypeBits==0.

### Stock-mutter validation (2026-06-10): the mutter patch is NOT needed with the host fix
Booted a clone of dev-enh.raw, `dnf reinstall mutter` (all four libs verified stock at the
paths gnome-shell maps, via /proc/PID/maps + mtimes/sizes), gdm restart. Journal: ZERO stencil
lines (stock cogl prints "Stencil buffer not available" when stencil is missing — it didn't).
USER VERIFIED: no instability/damage reproducible on completely stock mutter 49.5. The whole
mutter patch (cogl zero-init + degrade-to-extents) is dormant with a working stencil buffer;
it stays baked in dev-enh.raw as harmless defense-in-depth, but stock guests are correct
purely host-side — the two-tier shape we want. (Golden image still has the patched mutter;
the stock validation was clone-only.)

## Firefox WebGL on venus (2026-06-10): root causes mapped

User report: some WebGL pages work (webglsamples blob), others fail (aquarium) with
FEATURE_FAILURE_EGL_CREATE. Three distinct findings:

1. **The aquarium failure = WebGL2.** aquarium-config.js ships `enableVR: true` ⇒ it requests
   `getContext('webgl2')`. WebGL2 needs a GLES 3.0 context; **zink-on-venus-on-MoltenVK exposes
   only ES 2.0** (eglinfo: "OpenGL ES profile version: OpenGL ES 2.0"). The ES3 eglCreateContext
   fails IN-GUEST (zero host traffic — the instrumented worker showed no new venus contexts) ⇒
   FEATURE_FAILURE_EGL_CREATE; tdl's webglcontextcreationerror handler nukes the page before its
   webgl1 fallback can save it. Why no ES3: zink requires `VK_EXT_transform_feedback` +
   `VK_EXT_conditional_rendering` for GL/ES 3.0 (docs/drivers/zink.rst) — MoltenVK implements
   neither (Metal has no XFB; ANGLE-on-Metal emulates it with compute). **WebGL1 works fine**
   (blob renders 60fps, repeatedly, multiple contexts).
   - `MESA_GLES_VERSION_OVERRIDE=3.0` (with or without GL override): firefox SIGSEGV — the
     missing features are load-bearing; forcing the version is not a workaround.
   - The real fix is a MAJOR workstream: implement XFB (+ conditional rendering) in our MoltenVK
     fork (upstream precedent: ANGLE Metal). Tracked as the WebGL2/ES3 wall.

2. **MSAA backbuffer FBO incomplete (silent fallback, no user-visible breakage).** Firefox's
   antialias backbuffer (MozFramebuffer: RGBA8 color RB + DEPTH24_STENCIL8 RB, samples=4 via
   GL_EXT_multisampled_render_to_texture) gets FRAMEBUFFER_INCOMPLETE_ATTACHMENT; Firefox falls
   back to non-MSAA and pages render (un-antialiased). Two sub-findings:
   - Found+fixed a REAL MoltenVK emu gap on the way: getImageFormatProperties computed
     supportsMSAA from the bare VkFormat caps ⇒ substituted formats (D24S8) reported
     sampleCounts=1; fixed by resolving through getMTLPixelFormat (substitute fallback) —
     in mvk-instrument.patch.
   - But the Firefox incompleteness REMAINS after that fix, and `msaa-test.c` (this dir)
     reproducing Firefox's exact FBO shape (incl. RGB8 color) is COMPLETE on the same boot.
     The delta is something in Firefox's GL context (robustness attribs?). OPEN THREAD —
     deprioritized because the fallback makes it cosmetic (no AA) only.

3. **virgl (init=0x0) CTX_CREATE failures are by design, not a bug.** The new libkrun
   context-lifecycle logging (libkrun patch 0016) showed guest processes occasionally creating
   capset-0 (virgl GL) contexts — those fail ComponentError(22) on our venus-only host and the
   guest's subsequent ATTACH/DETACH spam dmesg with 0x1200 (the errors that looked alarming at
   the start). gnome-shell does one such probe at session start and falls back to venus
   cleanly. Mesa picks virgl when the zink env override is absent from a process's
   environment — worth remembering when a guest app "has no GL": check it sees the zink env.
   (Launch GUI apps for tests via `systemd-run --user` so environment.d applies.)

Oracle kept: msaa-test.c. Worker-side observability kept: libkrun 0016 (CTX_CREATE/DESTROY at
info with guest process name; all GPU error responses at warn with precise rutabaga error).

## KosmicKrisp A/B round 1 (2026-06-10): builds, runs venus headless, ONE winsys blocker

KosmicKrisp (mesa-native Vulkan-on-Metal, mesa main 178a3d7) evaluated as a MoltenVK
alternative under virglrenderer/venus. Build: trivial — brew deps (llvm 22.1.6, libclc,
spirv-llvm-translator, spirv-tools), mesa checkout on a case-sensitive sparse volume
(third_party/mesa-cs.sparseimage — host APFS trap), venv for python deps, the documented
meson line (docs/drivers/kosmickrisp.rst), zero patches. Boot vehicle:
spikes/venus-draw-probe/boot-seated-kk.sh (VK_ICD_FILENAMES, same mechanism as the
instrumented MoltenVK).

| check | MoltenVK (ref, today) | KosmicKrisp |
|---|---|---|
| host vulkaninfo | 1.2 | **1.3.353** |
| venus-relayed apiVersion (guest) | 1.2 | **1.3.353** |
| render server init | clean | clean (no software-2D degrade) |
| msaa-test (headless FBOs) | COMPLETE | **COMPLETE** |
| guest gains through venus | — | **+EXT_conditional_rendering** (ES3 gate 1/2), +EXT_robustness2 |
| stencil-test (GBM winsys) | STENCIL WORKS | **SIGABRT** at window-surface creation |
| gnome-shell seat | clean | **crash-loop (SIGABRT)** |

THE blocker (single, crisp — found instantly by libkrun-0016 logging + KK's own mesa-style
warnings): `kk_image.c:421: unsupported VkExternalMemoryHandleTypeFlagBits: OPAQUE_FD
(VK_ERROR_FORMAT_NOT_SUPPORTED)` ×27 ⇒ ResourceCreateBlob/CmdSubmit3d ComponentError(-1) ⇒
every guest winsys image (GBM window surface, scanout blob) fails ⇒ venus CHECK abort in the
client. vkr requests OPAQUE_FD for blob export; our MoltenVK path solved this same contract
with the limina/macos-blob-map-ptr virglrenderer work; KK rejects the query outright (it offers
EXT_external_memory_metal / external_memory_host instead). Fix lives in OUR virgl fork (extend
the macOS external-memory translation to KK's handle types) and/or a small KK patch — a known,
bounded integration problem, squarely in our experience.

Notes: EXT_transform_feedback still absent in KK (properties scaffolded in kk_physical_device.c,
no implementation — the ES3 prize needs KK work either way, but mesa-side with in-tree
emulation precedents). KK advertises EXT_index_type_uint8 (Metal lacks u8 indices ⇒ they
convert internally — our vkr __APPLE__ u8 hide currently masks it; re-evaluate on KK when
winsys works). KK has built-in Metal capture (MESA_KK_GPU_CAPTURE=1) — better debug ergonomics
than our instrumented MoltenVK. Strategy stays side-by-side: MoltenVK default, KK behind
boot-seated-kk.sh until the blob blocker falls.

## KosmicKrisp rounds 2–12 (2026-06-10): zero-copy scanout WORKS; one rasterization bug open

The OPAQUE_FD blocker fell in six iterative rounds, all in OUR virgl fork (commit 70c9f0c on
the limina branch) plus two one-line KK patches (kk-patches/kk-fixes.patch):

1. **IFP2 synthesize** (vkr_physical_device.c): strip OPAQUE_FD/DMA_BUF external-image-format
   queries before KK sees them, synthesize EXPORTABLE|IMPORTABLE on the way back — zink's
   modifier probing now succeeds on a driver with zero fd handle types.
2. **System-MTLDevice fallback** (vkr_device.c): KK has no VK_EXT_metal_objects, so
   vkr_metal_get_device returned NULL and every blob carrier alloc failed; fall back to
   MTLCreateSystemDefaultDevice (single GPU on Apple Silicon).
3. **Forced-LINEAR scanout images** (vkr_image.c): KK can't adopt an IOSurface into a VkImage
   (no VkImportMetalIOSurfaceInfoEXT), but it builds LINEAR images' MTLTextures from the bound
   memory's MTLBuffer — so force scanout images LINEAR, strip external/modifier structs, and
   drop INPUT_ATTACHMENT usage (KK promotes those to 2DArray layout; Metal buffer-backed
   linear textures must be type 2D — also patched the KK bridge to demote, kk-fixes.patch).
4. **Pitch-matched IOSurface** (vkr_image.c post-create): allocate the global IOSurface with
   kIOSurfaceBytesPerRow = the driver's real rowPitch (vkGetImageSubresourceLayout).
5. **Forced dedicated allocation** (vkr_image.c GetImageMemoryRequirements2): zink only chains
   VkMemoryDedicatedAllocateInfo when the driver asks; without it the memory path can't find
   the image (the host-pointer import silently never fired — found via VIRGL_LOG_LEVEL=info,
   which boot-seated-kk.sh now sets; vkr_log is INFO and the default level is WARNING).
6. **Host-pointer import** (vkr_device_memory.c): the dedicated scanout allocation imports the
   IOSurface base address via VK_EXT_external_memory_host (+ KK NULL-deref assert fix,
   kk-fixes.patch); KK's linear texture is then literally backed by IOSurface bytes.

**VERIFIED:** gnome-shell seats on KK, SET_SCANOUT_BLOB resolves zero-copy IOSurfaces, and
iosdump shows the full activities overview (wallpaper, complete dock, top bar) in the scanout
surface. The original goal — KK as a venus backend with zero-copy present — is proven.

**OPEN BUG (the one thing left):** draws into a render pass whose color attachment is a
buffer-backed LINEAR texture AND that has a depth-stencil attachment produce ZERO fragments;
clears land fine. GBM-window stencil-test = red (fails); same ops surfaceless-FBO = green
(passes, stencil-fbo-test.c, new oracle); desktop linear passes (no DS) draw fine. The
elimination ledger, each independently verified:
- NOT stencil semantics (GL_ALWAYS and stencil-disabled variants also drop the draw).
- NOT Metal capability (linear-rt-test.swift, new host oracle: draw lands in all 4 cases —
  plain/bytesNoCopy-over-IOSurface × with/without DS).
- NOT vertex data/address: KK is bindless; the quad floats verified CPU-side at the exact
  GPU address in the root descriptor (probe in kk_cmd_draw.c), clamp/range sane.
- NOT viewport/scissor/cull/winding/PSO-vs-FBO-run (full encoder state traces identical
  modulo y-flip, which is benign).
- NOT texture views (all newTextureView calls on the linear parent succeed, incl 2DArray).
- NOT pipeline-rebind/colormask staleness (two-program GBM variant still fails).
- NOT GPU faults (command buffers complete with no error).
A targeted single-pass Metal capture of the failing pass is in /tmp/kk-pass.gputrace
(LIMINA_KK_CAPTURE=<width> + METAL_CAPTURE_ENABLED=1, probe in mtl_encoder.m / kk-probes.patch)
— needs Xcode-replay eyeballs, the #31 method.

Latent upstream KK bugs found en route (not load-bearing for us today):
- kk_bind_buffer_memory drops info->memoryOffset for non-heap (imported) memories
  (kk_buffer.c:163) — any VkBuffer bound at nonzero offset into imported memory reads wrong.
- KK bakes color write mask into the fragment shader (nir_lower_blend) and ignores
  vkCmdSetColorWriteMaskEXT — fine for pipelines (mask static, in the hash via state->cb),
  non-conformant if VK_EXT_shader_object is ever used against it.

## KosmicKrisp round 13 (2026-06-10): the "open bug" was a GUEST ZINK REGRESSION — KK track is GREEN

**Verdict: not a KK bug, not a host bug, not our winsys plumbing.** The linear+DS draw-drop
only ever reproduced with **stock Fedora zink (mesa 25.3.6)**; our `/opt/mesa-zink`
(26.1.0-devel) passes everything on KK. Same VM boot, same host stack, minutes apart:

- `LD_LIBRARY_PATH=/opt/mesa-zink/lib64 … /tmp/st` → **STENCIL WORKS** (left green/right red)
- bare `MESA_LOADER_DRIVER_OVERRIDE=zink /tmp/st` (= system mesa 25.3.6) → STENCIL BROKEN

**Root cause (upstream mesa):** zink 25.3's draw-time pipeline-update gate
(`zink_draw.cpp:729`) is missing `ctx->gfx_pipeline_state.dirty` — a blend-CSO-only change
(glColorMask) sets *only* that flag, so the draw after the change keeps the previous
pipeline. Introduced by `32b4412d` ("zink: update gfx pipeline less frequently", 2025-09-11),
fixed by `5a5a87ec` ("zink: use gfx_pipeline_state.dirty as a pipeline update condition",
2026-01-20, MR 39381) whose commit message describes this failure verbatim. No `Fixes:` tag ⇒
no 25.3-stable backport; Fedora 43 stays broken until mesa 26.x.

**Why it looked like a KK/linear+DS bug:** the trigger isn't the render target at all — it's
the *feature level*. With full-ds3 (MoltenVK + private API), colormask is dynamic state and
the buggy gate is bypassed; KK advertises only 5 EDS3 features (no colorWriteMask) ⇒ zink
takes the static-blend path ⇒ the missing dirty check fires. The stencil-test's
mask-off→mask-on toggle between draws is exactly the minimal trigger: draw1's wm=0 pipeline
stays bound for draw2 (host evidence: vkr now dumps per-pipeline dynamic-state + static
write masks at CreateGraphicsPipelines, virgl fork e2e2702 — failing run shows
Create(wm=0)+one bind+both draws, then Create(wm=f) bound too late in the next batch;
passing run shows both creates up front and a bind per draw). The KK replay probes agree:
one `mtl_render_set_pipeline_state`, two draws, DSS swap in between.

**Two broken oracles burned this round** (recorded so we stop repaying them):
- `vulkaninfo | grep -c VK_EXT_shader_object` counted **GPU1 = llvmpipe**; the venus device
  never advertised it (venus-protocol doesn't know the extension — an entire "hide
  shader_object in vkr" fix was a no-op aimed at the wrong GPU). Scope vulkaninfo greps
  per-GPU.
- The whole failing battery ran bare `MESA_LOADER_DRIVER_OVERRIDE=zink`, i.e. **system**
  zink — not the canonical `/opt/mesa-zink` env from the top of this file. Oracle
  invocations must pin the guest GL stack explicitly; "zink" is not one thing in the guest.
- (From round 12, same family: a two-program differential must vary *shader source* — mesa
  dedupes identical-source programs, which invalidated the first colormask refutation. The
  ledger line "NOT pipeline-rebind/colormask staleness" above was wrong for exactly this
  reason.)

**Final KK battery (our zink, canonical env): GBM stencil WORKS, FBO stencil WORKS, MSAA
unchanged** (pre-existing cosmetic INCOMPLETE_ATTACHMENT thread, same as MVK path). The
/tmp/kk-pass.gputrace Xcode-replay ask is obsolete — root-caused without it.

**Tier implications:** enhanced tier (our baked mesa) is fully green on KK. Stock tier
(Fedora 43 mesa 25.3.6) hits the zink regression on KK *and would on any
no-dynamic-colormask driver*; it works on MoltenVK only because private-API full-ds3 routes
around the bug. Options if/when KK becomes the default backend: ping Fedora for the
one-line `5a5a87ec` backport, or implement EDS3 colorWriteMask in KK (we own it).

## Round 14 (2026-06-10): WebGL2 on KK — VK_EXT_transform_feedback + the cross-context import saga

**Goal:** open the ES3/WebGL2 gate on the KK stack and run the webglsamples aquarium in
Firefox. **Outcome: DONE, pixel-verified** — `evidence/aquarium-webgl2-kk-window-buffer.png`
(Firefox's own window IOSurface, full fish school) and `evidence/aquarium-webgl2-kk-desktop.png`
(the composited seated desktop showing the Firefox window) — after one feature
implementation and THREE stacked crash root-causes.

### 1. VK_EXT_transform_feedback implemented in KosmicKrisp (`kk-patches/kk-xfb.patch`)

zink requires XFB (+ conditional rendering, emulated) for GL 3.0/ES 3.0. Design:
- Capture is lowered into the **vertex shader as `store_global` writes**
  (`kk_nir_lower_xfb.c`): per io_xfb-annotated store_output, inside an `active_mask` bit
  test, compute `slot = (instance_id − first_instance) × verts_per_instance + vertex_id`
  and store to `xfb_base[buffer] + slot×stride + offset` (all in the root descriptor,
  pre-folded host-side per draw: bound base + append offset − firstVertex×stride).
- **CPU counter shadow** (device-level ring keyed on counter mtl_buffer+offset): sound
  because KK defers all vkCmd* into vk_cmd_queue and replays sequentially at submit; zink
  only consumes counters via Begin-resume and CmdDrawIndirectByteCountEXT, both served
  from the shadow.
- Queries: TRANSFORM_FEEDBACK_STREAM (written,needed) + PRIMITIVES_GENERATED accumulated
  CPU-side, but results MUST be written via KK's GPU-ordered imm-write list
  (`kk_cmd_write`) — plain CPU stores get clobbered when an earlier-recorded
  vkCmdResetQueryPool dispatch executes later on the GPU (cost: a day of
  "primitives_written=0").
- Exact for the GLES3-legal surface (ES3 forbids indexed/non-list draws during XFB);
  indexed/indirect/tess capture masks off with a one-time warning. `maxTransformFeedbackStreams=1`.
- Gotchas for the next reader: `nir_io_xfb.out[]` is indexed by absolute start component
  and strides/offsets are in DWORDS; producer = `nir_io_add_intrinsic_xfb_info` after IO
  lowering + constant folding; **GL semantics: End+Begin RESTARTS capture at 0, only
  Pause/Resume appends** (we almost "fixed" correct behavior — the oracle was wrong, not
  the driver).
- Oracle: `xfb-test.c` (surfaceless ES3, interleaved capture, pause/resume append,
  primitives query). Verified: ES 3.1 context, capture byte-correct, resume=96 append,
  written==4.

### 2. Crash 1: gnome-shell SIGABRT on Firefox launch = silent vkr import failure (virgl 3e3d754)

The compositor importing a client's wl_buffer (1×1 probe then real window buffers) was a
**never-exercised path on macOS**: `vkr_get_fd_info_from_resource_info` knows only
DMABUF/OPAQUE fds, our blobs are SHM *carriers* (pixels live in the global IOSurface) or
fd-less map_ptr shares → silent `VK_ERROR_INVALID_EXTERNAL_HANDLE` → guest treats
vkAllocateMemory as async-success → vkBindImageMemory2 "failed to look up object 2179
(type 8)" → CS fatal → **ring FATAL** → guest `vn_ring_submit abort on fatal` = the
"silent" SIGABRT. (Trigger context: the IFP2 modifier-probe synthesize from the KK
winsys work opened mutter's dmabuf path; on the earlier MVK runs mutter had fallen back
to wl_shm, which is why this never fired before.)

Fix (virgl fork `3e3d754`): thread `iosurface_id`/`map_ptr` onto `vkr_resource` (via the
render-server import protocol AND on export-side registration), and translate
import-from-resource allocs into **VkImportMemoryHostPointerInfoEXT** over the bytes the
exporter's GPU actually writes (IOSurface base ≻ map_ptr ≻ SHM mmap). Explicit-modifier
(import) image creates skip the fresh-IOSurface allocation. Every unhandled case now logs.

- **Sub-bug found by the loud path**: a SAME-context re-import reads the EXPORT-side
  vkr_resource, whose union holds an fd NUMBER (`u.fd`), not a mapping — read as
  `u.data` it became `pHostPointer=0xc2`. Fixed by carrying iosurface_id/map_ptr on the
  export-side resource too + a plausibility guard on the SHM fallback.

### 3. Crash 2: limina-vmm SIGSEGV in AGX residency commit = KK nil-in-residency-set (kk-fixes.patch)

With imports flowing, the whole worker died at `kk_queue_submit →
kk_device_make_resources_resident → AGXG13XFamilyResidencySet commit` (null+0x18).
`newBufferWithBytesNoCopy` returns **nil** on a rejected pointer/length and
`kk_AllocateMemory` added it to the residency set unconditionally; the next submit's
residency commit segfaults the process. Now fails the allocation loudly
(`[LIMINA-KK-IMPORT]`) — which is exactly what exposed the 0xc2 sub-bug above.

### 4. Crash 3: Firefox abort in wsi present = upstream venus use-after-free (patches/mesa/0005)

`vn_wsi_clone_present_info` deep-copies the `VkPresentRegionKHR` array but **not the
per-region `pRectangles`** (`VkRectLayerKHR[]`). zink's kopper frees its rectangle
storage (`cpi->regions`) as soon as vkQueuePresentKHR returns; venus's **async present
thread** then reads freed memory → `assert(rect->layer == 0)`
(wsi_common_wayland.c) on our debug build, garbage damage on release. Fix =
`patches/mesa/0005-venus-deep-copy-present-region-rectangles.diff`, **baked into
dev-enh.raw** (applied in-image at ~/mesa-venus, installed atomically). Upstream mesa MR
candidate.

**Deploy lessons (paid for this round):**
- `boot-seated-kk.sh` RE-CLONES /tmp/seated-kk.raw every boot — in-guest installs vanish
  unless baked into `Fedora-Workstation-43.dev-enh.raw` (boot it with `LIMINA_DISK=`).
- NEVER `cp` over a mapped .so in a live guest — it overwrites the inode under the
  running process (gnome-shell SIGSEGV'd on corrupted text). `cp` to a temp name +
  `mv` (atomic rename) keeps the old inode alive for running processes.

**Net state:** seated KK desktop + Firefox with working WebGL1+WebGL2; the remaining
known cosmetic thread is the MSAA backbuffer INCOMPLETE (pre-existing, both backends).
MVK host remains ES2-only (no XFB in MoltenVK) — WebGL2-on-KK is a genuine
differentiator. Next: perf battery (aquarium FPS, glmark2, vkcube/vkmark; KK vs MVK).

## Round 15 (2026-06-10): perf battery — KK vs MVK (WebGL / GL / Vulkan)

All runs on the dev-enh guest (our zink + venus, canonical env), 4 vCPU / 4 GiB, host M1 Max.
glmark2 full default suite; "windowed" = wayland flavor inside the seated session (real
present path), "GBM" = `glmark2-*-gbm` with the session stopped (offscreen-ish, no
present throttling — only comparable to the other GBM number, not to windowed).

| benchmark                         | KK (KosmicKrisp)         | MVK (MoltenVK)            |
|-----------------------------------|--------------------------|---------------------------|
| WebGL2 aquarium 500 fish (1024²)  | **54 fps**               | N/A — ES2 only (no XFB)   |
| WebGL2 aquarium 5000 fish         | **17 fps**               | N/A                       |
| glmark2-es2-wayland (windowed)    | **1983**                 | N/A — crashes (see below) |
| glmark2-wayland (windowed)        | **1852**                 | N/A                       |
| vkmark (wayland, raw Vulkan)      | **3419**                 | N/A — no VkSwapchain      |
| glmark2-es2-gbm (offscreen)       | 1392                     | **3170**                  |
| glmark2-gbm (offscreen)           | 1378                     | **3102**                  |

**Readings:**
- **Raw GL throughput: MVK ≈ 2.3× KK** on the identical GBM path (3170/3102 vs 1392/1378).
  KK is the younger driver and our KK scanout path forces LINEAR render targets —
  expensive on a TBDR GPU. Lots of headroom, not a verdict.
- **Feature coverage: KK strictly wider.** WebGL2 (our XFB), and the entire windowed
  Vulkan-WSI world (vkmark 3419, real VkSwapchain through venus) exist only on KK.
- KK windowed (1983) outscores KK GBM (1392): the wayland swapchain images are
  optimal-tiled; the GBM flavor's linear buffers are the slow path on KK.

**MVK leg structural finding (the windowed N/A):** venus guest WSI requires the renderer
to expose `KHR_external_semaphore_fd` with sync_fd import (`renderer_sync_fd.
semaphore_importable` gates `exts->KHR_swapchain`, vn_physical_device.c). KK satisfies it;
MoltenVK does not ⇒ venus advertises NO `VK_KHR_swapchain` on the MVK leg ⇒ every windowed
GL client dies in `zink_kopper update_swapchain` calling a NULL `vkCreateSwapchainKHR`
(zink never checks the extension — upstream robustness gap worth an MR), taking the
session with it (Xwayland frames-client collateral). The desktop itself is fine (mutter =
GBM, no swapchain). First diagnosed as the IFP2 synthesize opening the modifier path —
gating the synthesize to KK (virgl `5f62d46`) is still right (MVK's import side doesn't
exist), but the crash was the missing-WSI NULL, present either way: **invariance under the
gate was the tell** (same lesson as round 13: identical A/B ⇒ differential not reaching
the system under test).
- Building MVK windowed support = give the MVK leg sync_fd semaphores (vkr-side sync_fd
  emulation — venus already "cheats" binary semaphores; or MoltenVK-side) + the
  vkUseIOSurfaceMVK-at-bind import path. Tracked as future work; KK is the daily driver.

## Round 16 (2026-06-11): the KK perf gap ROOT-CAUSED — GPU-bound on shader-generated load chains, not virtualization overhead

**Question:** host Firefox does 60fps at 5k–10k fish; the VM does ~16fps at 5k. Where do
the cycles go? (KK GBM glmark2 was already 2.3× behind MVK on an identical guest stream.)

**Method:** measure before theorizing. (1) per-thread CPU on both sides under load,
(2) host GPU utilization counters (`ioreg -c IOAccelerator`, Device/Renderer/Tiler %),
(3) a new aggregate Metal-bridge counter `LIMINA_KK_STATS=1` (in kk-probes.patch): per-second
render/blit/compute encoder creations, Load-action passes, draws — tests the render-pass-
split theory without drowning in per-call logs, (4) `MESA_KK_DEBUG=msl` dump of the actual
fish vertex shader (saved: `evidence/aquarium-fish-vs-kk.msl`).

**Measurements (aquarium 5000 fish, kiosk 1280×800, seated KK desktop):**

| metric | VM (KK) | host native (Chrome, 1024²) |
|---|---|---|
| fps | ~16 | 60 (vsync-capped) |
| GPU Device util | 93% | 9–12% |
| GPU Tiler util | 92–93% | 1–2% |
| guest CPU | ~95% idle | — |
| worker CPU | ~1 core | — |
| draws/s (KK probe) | ~82–85k | — |
| render encoders/s | ~134 (≈8/frame) | — |
| blit encoders/s | 0 | — |

⇒ per-frame GPU work ≈ **35× native**; tiler (vertex-stage) work ≈ **two orders of
magnitude** over native. CPU everywhere is idle: **NOT command-stream/virtualization
overhead, NOT pass splitting** (8 passes/frame is sane, no blit storm — the structural
theories died on the counter). The cost is *inside the draws*: the GPU is latency-bound
executing KK-generated shader code.

**Root cause (from the MSL dump), three compounding generators:**
1. **Vertex pulling** (`kk_nir_lower_vbo.c`, AGX-heritage): every attribute = root-table
   ptr → base ptr → stride → packed fetch, a ~4-deep *dependent* load chain per attribute
   per vertex (×6 attributes for the fish VS). MVK uses Metal `[[stage_in]]`/
   `MTLVertexDescriptor` (hardware-assisted prolog, uniform-register bases) — explains the
   KK-vs-MVK 2.3× on identical streams.
2. **Per-scalar bounds-checked UBO loads**: WebGL ⇒ robust context ⇒ every uniform scalar
   is `bound < size ? load : 0` — ~25 compare+select+load per vertex (and the same per
   fragment), never vectorized.
3. **Everything through one root table** (bindless argument buffer): the UBO descriptor
   itself is re-loaded (device address space, 64-bit address reconstructed) *per vertex* —
   Metal's uniform preloading is structurally defeated; nothing is in uniform registers.

**Improvement plan (ranked, measure each):**
- **(A) sizing A/B, afternoon:** env-gated KK hack skipping robust bounds + scalarization
  on UBO loads → aquarium fps + glmark2. Sizes lever 2 in isolation.
- **(B) sizing A/B, afternoon:** vkr relax forced-LINEAR to true scanout only (non-scanout
  dmabufs stay optimal-tiled both sides of the cross-context import) → renderer-side win;
  windowed-vs-GBM 1983/1392 already hints ~40% on GL loads.
- **(C) per-draw root-table writes of resolved UBO base+size** (KK already writes
  attrib_base/strides per draw): removes the per-vertex descriptor chase. Moderate.
- **(D) vectorize robust UBO loads** (vec4 load + single range check): NIR-level, upstream-
  able. Moderate.
- **(E) `[[stage_in]]` vertex fetch** for the common case (no exotic formats): the
  canonical fix for lever 1, biggest single win, real driver work. MVK proves the ceiling.
- Side find: `nir_to_msl.c` workaround forces ALL `load_global` to `coherent device`
  (cache-bypassing) unless `MESA_KK_DISABLE_WORKAROUNDS=6` — not the aquarium's path but
  worth an A/B on SSBO-heavy loads.

Probe lessons: `LIMINA_KK_STATS` cost nothing and killed two wrong theories in one interval;
the guest Firefox unit must wait for `/run/user/1000/wayland-0` (gnome-shell "active"
races the socket).

## Round 17 (2026-06-11): ⭐ AQUARIUM 16→60fps — the per-draw GPU index-UNROLL was the whole gap

Continuation of round 16 ("understand the bottleneck and claw it back"). Three sizing
A/Bs, two exonerations, then the kill. All knobs live in kk-xfb.patch (KK working tree).

**Exoneration 1 — robust bounds checks (LIMINA_KK_NOROBUST=1):** drops ALL bounded-load
lowering + vertex-attrib clamps. Verified loaded (zero `? *(constant` patterns in any
compiled MSL) — aquarium IDENTICAL (~82k draws/s, 93% GPU). The per-scalar checks are
latency-shadowed; not the cost. (Knob kept for future sizing.)

**The microbench that re-aimed everything — spikes/kk-draw-bench/** (host-only, no VM:
headless Vulkan, N draws × V verts, variant matrix attrs 0/1/6 ± fish-like UBO reads ±
full-screen fill ± per-draw rebinds; driver picked by VK_ICD_FILENAMES):
- Draw/vertex path: KK 0.48µs/draw vs MVK 0.34 (fish6u: 0.50 vs 0.36) — only **1.4×**,
  ~2 Gverts/s. KK's vertex pulling + root tables are FINE. The 30×-per-draw VM gap is
  NOT raw KK codegen.
- Per-draw rebinds (zink-like stream): KK 1.20µs vs MVK 0.36 — 3.3×, real but secondary.
- Fill path: KK 9.8µs/draw FLAT across depth-mode AND blending = no early-Z, no HSR ever.
  Generated FS unconditionally writes [[depth(any)]] (msl_ensure_depth_write, any depth
  attachment) and [[sample_mask]] (any shader using derivatives = any texturing FS,
  Vulkan-Portability#54 helper-quad path) ⇒ late-Z always. LIMINA_KK_EARLYZ=1 gates both:
  9.8→5.3µs; + MESA_KK_DISABLE_WORKAROUNDS=4,5 (helper writes, fake discard guard):
  → 4.1µs vs MVK 3.3 (blend ordering sane again). Big for fill-bound content — but
  **Exoneration 2:** aquarium UNCHANGED under EARLYZ (fish are ~1500px draws; not fill-bound).

**The kill — Metal System Trace (xctrace, attach limina-vmm, 6s):** GPU time by channel:
**Compute 5354ms (89%!)**, Vertex 80ms, Fragment 82ms. The render work was always tiny;
the GPU was saturated by ~150 compute encoders/s avg 5.8ms. Source: kk_cmd_draw.c
`kk_unroll_geometry` — for every indexed LIST draw with primitive restart enabled, KK
converts the draw to indirect and dispatches `libkk_unroll_geometry` (1024×draw_count
threads) to rewrite the whole index stream through the heap. **WebGL2/GLES3 mandate
always-on PRIMITIVE_RESTART_FIXED_INDEX ⇒ zink sets restartEnable on EVERY indexed draw
⇒ all 5100 GL_TRIANGLES fish draws/frame unrolled, ~10µs GPU each ≈ 50ms/frame of compute
for index buffers that NEVER contain the restart sentinel.** requires_index_robustness
was checked and does not fire (direct draws, no overread).

**Fix (sizing knob LIMINA_KK_NOLISTRESTART=1, kk_cmd_draw.c requires_unroll_restart):**
skip unroll for list topologies. Result, same 5000-fish kiosk scene:
draws/s 82k → **308k sustained ≈ 60fps vsync-capped**, GPU 93% → **46%**. Pixel-verified
via iosdump (evidence/aquarium-5k-60fps-nolistrestart.png). **VM aquarium now matches the
user's host-native Firefox experience (60fps@5k).**

Spec note: the skip deviates only for list draws whose index stream actually contains the
restart sentinel (would emit junk triangles instead of cutting) — same trade the upstream
code already makes when the feature is off. Upstream-quality fixes: (a) cache/scan
"contains-restart-index?" per (buffer,range,generation) and unroll only on hit;
(b) cache unroll OUTPUTS keyed the same way (fish meshes are static — 5000 unrolls/frame
of identical data); (c) driconf-style per-app toggle. The unroll kernel itself also looks
~50× too slow for what it does (200M idx/s effective) — worth its own pass.

Knob ledger (all default-off, A/B only): LIMINA_KK_NOROBUST, LIMINA_KK_EARLYZ,
LIMINA_KK_NOLISTRESTART (kk-xfb.patch); LIMINA_KK_STATS (kk-probes.patch);
MESA_KK_DISABLE_WORKAROUNDS=4,5,6,7 (upstream mechanism). Next: EARLYZ correctness
review (why does msl_ensure_depth_write exist?), rebind-path cost (3.3×), re-run
glmark2/vkmark battery with knobs, upstream all of it.

### Round 17 addendum: limina-vmm host CPU profile (user-flagged)

`sample limina-vmm 5` under the 60fps aquarium (evidence/vmm-cpu-aquarium-60fps.sample.txt):
**idle desktop 7.9% (no leak); 60fps aquarium 185%** ≈ 1 core of vCPU threads (guest's own 60fps
work) + ~1 core of venus/KK command processing spread over the per-context vkr ring threads.
Hot ring thread breakdown (vkr-ring-6 / Firefox, 63% busy): 45% in kk_queue_submit →
vk_cmd_queue_execute — KK **records** vkCmds at venus-decode time (vk_cmd_enqueue_*) then
**replays** them on the CPU at submit (double processing); the replay inner loop is per-draw
kk_flush_gfx_state → kk_upload_descriptor_root + kk_cmd_buffer_flush_push_descriptors (~50% of
replay — the CPU twin of the 3.3× per-draw rebind cost from the microbench, same dirty-tracking
lever); mtl_residency_set_commit ~5%/submit; ~30% of ring-thread samples parked in the ring's
nanosleep busy-poll (wakeup churn). Levers, cheap→deep: dirty-track root/push-descriptor uploads
in KK (cuts CPU and GPU rebind cost together); event-driven ring wait; direct-encode at decode
time (skips the vk_cmd_queue replay entirely — upstream-scale change).

### Round 17 addendum 2: memory-overhead investigation (host + guest, staged workloads)

Method: fresh boot, then `memsnap.sh` (new, in this spike) snapshots at idle → WebGL aquarium
60fps → idle → apple.com (CSS-heavy) → unsplash.com (image-heavy) → final idle. Full log
preserved in evidence/memlog-2026-06-11.txt.

**Host worker RSS: 5.22 GiB (boot idle) → 6.27 (aquarium) → 6.83 GiB (final idle) — never
returns.** Accounting (vmmap): `shared memory` = 12 GiB VA / ~2.7 GiB resident = the 4 GiB guest
RAM + the 8 GiB venus shm-blob window; the growth is the **guest-page high-water mark** — touched
guest pages stay host-resident forever absent ballooning. This is the M6 dynamic-memory case,
now measured. VSZ ~428 GiB is reserved VA (shm window + Metal), harmless.

**⭐IOSurface LEAK (confirmed, ours):** count 9 → 26 over the first browsing session, then a
targeted test — launch+quit Firefox 3× — leaked **exactly +4 surfaces (+15.6 MB ≈ 4× 1280×800
BGRA, the wayland swapchain depth) per cycle**, monotonic: 30/34/38. Unbounded for a long-lived
VM under window churn. Suspects: vkr forced-dedicated winsys IOSurface (vkCreateImage-alloc /
vkDestroyImage-release — does context teardown hit it?) and the cross-context import retain
(vkr_mtl_iosurface_lookup ref released in vkr_device_memory_release — importer = gnome-shell,
which outlives Firefox). Next: vkr_log counters on alloc/retain/release, find the unpaired path.

**Healthy:** `IOAccelerator (graphics)` (KK/Metal buffers) recycles correctly — 257 MB idle,
733 MB under aquarium, back to 270 MB. MALLOC zones flat. No host-side heap growth.

**Guest:** idle "used" ≈ 2.0–2.4 GiB of 3.9 — dominated by Fedora Workstation services, not VM
overhead: packagekitd up to 357 MB, gnome-software 220–315 MB, evolution daemons ~230 MB, fwupd
183 MB ≈ >1 GiB of distro daemons. gnome-shell 185–216 MB (normal for zink/venus); Firefox ~580 MB
under WebGL (normal). Product note: 4 GiB is a comfortable Workstation minimum; ballooning (M6)
is what turns the high-water mark back into shared headroom.

### Round 17 addendum 3: IOSurface leak FIXED (virgl fork 0bafb6e)

Root cause read straight from vkr_device.c once the cycle oracle confirmed the leak:
`vkr_device_object_destroy()` — the leftover-object sweep for guests that exit without
destroying their Vulkan objects (Firefox fast shutdown skips vkDestroyImage) — called the
driver's DestroyImage but had no "vkr allocs" release case for VK_OBJECT_TYPE_IMAGE, so
`vkr_image.mtl_iosurface` (our winsys/scanout backing surface) leaked once per winsys image.
DEVICE_MEMORY had its release case (why the cross-context import refs never leaked) — IMAGE
didn't. Fix: new `vkr_image_release()` called from BOTH destroy paths. **Verified: vmmap
IOSurface count 9/9/9 flat across three Firefox launch/quit cycles (was 30/34/38).**
Upstream-relevant: the teardown sweep's asymmetry between per-type driver destroys and
per-type vkr-alloc releases is an easy trap for any fork hanging allocations off objects.

## Round 18 (2026-06-11): LIMINA_KK_EARLYZ correctness review — CLEAN, promoted to boot default

The knob (round 16/17 archaeology) drops KK's two blanket FS injections that force late-Z on
every pipeline with a depth attachment: `msl_ensure_depth_write` (writes `gl_FragCoord.z` to a
`[[depth(any)]]` output in EVERY fragment shader under a depth attachment) and the helper-quad
`msl_lower_static_sample_mask(0xFFFFFFFF)` (any texturing FS). Both date to the initial KK
import commit (`7c268a1e918`) with no rationale; the sample-mask one cites Vulkan-Portability
#54 (implicit-LOD/derivative accuracy via live helper quads). Dropping them restores early-Z /
hidden-surface removal on Apple GPUs (kk-draw-bench fill: 3× → near-parity vs MoltenVK).

**Evidence, both legs of the review:**

1. **Vulkan CTS A/B (host KK, M1 Max):** built VK-GL-CTS in `third_party/`, ran the 10,907
   early-Z-sensitive cases (`fragment_operations.early_fragment.*`, `glsl.discard.*`,
   `glsl.derivate.*`, `query_pool.occlusion_query.*`, `pipeline.monolithic.depth.*`,
   `...multisample.sample_mask.*`) via deqp-runner, baseline vs `LIMINA_KK_EARLYZ=1` —
   harness: `cts-earlyz-ab.sh`. Result: **status-identical** (4010 Pass / 2 Warn / 0 Fail /
   6895 Skip both legs; skips = unsupported formats/exts: maintenance5, early_and_late ext,
   16x MSAA, exotic depth formats). The groups that would break if the hammer were
   load-bearing all ran green: every discard test (25), the invocation-counting
   early-fragment tests, occlusion queries (434), derivatives (1656), sample-mask (32).
   The 2 Warns (sample_count_early_fragment_tests_depth_samples_2/4, quality warnings)
   exist in BOTH legs — pre-existing, not EARLYZ.
   **Knob-loaded proof (the invariance lesson):** MSL dump of an early_fragment test FS has
   the injected `[[depth(any)]]` output with the knob off and NOT with it on; test passes
   both ways. The identical A/B is a real exoneration, not a dead differential.
2. **Human eyeball pass (the pixel oracle):** user drove the seated desktop booted with
   `LIMINA_KK_EARLYZ=1` — windows, overview, Firefox scrolling — and reports **correctness
   clean**. (Stutters seen during the pass were host CPU contention from the concurrent CTS
   build + cargo install + Spotlight indexing the fresh clone, not the knob: EARLYZ only
   removes GPU-side work.)

**Why it's safe (the model):** Metal's fixed-function depth path performs its own
early-vs-late promotion per spec — it punts to late-Z automatically when the FS discards,
writes depth, or has visible side effects. Vulkan only *requires* late-Z in those same cases.
The blanket injection was conservatism, not a semantic need KK's MSL backend has — none of
the CTS cases that exist precisely to catch wrong-early-Z (invocation counting via SSBO
atomics, occlusion counters, discard+derivative interactions) can tell the difference.

**Perf payoff on the flagship workload (measured post-review):** aquarium 5k fish on the
EARLYZ boot holds 60fps (draws ≈308k/s, same as round 17) at **28–30% device utilization vs
46% without EARLYZ** — ~1.6× GPU headroom at the vsync cap, free. (kk-draw-bench had predicted
this: the fill variant went 3×-vs-MVK → near-parity with the injections dropped.)

**Decision: default-ON in boot-seated-kk.sh** (same opt-out pattern as NOLISTRESTART:
`LIMINA_KK_EARLYZ=0` disables). Upstream note: the right KK fix is narrowing the injection
condition to shaders that actually need it (or deleting it if Metal's promotion is fully
trusted); our 11k-case slice is strong evidence but KK upstream would want a full-CTS run
before changing conformance-adjacent behavior.

### Round 18 addendum: 10k-fish probe — ceiling is now the CPU replay thread, not the GPU

With 5k vsync-capped, jumped to numFish=10000 (host-native Firefox: 57–59fps). Result:
**~42fps** (draws ≈420k/s ÷ ~10,100 draws/frame) at only **28–38% device utilization** —
EARLYZ removed the GPU wall; the new ceiling is CPU. `sample` confirms: one vkr ring thread
~76% busy in kk_queue_submit → vk_cmd_queue_execute (KK record-then-replay), inner loop
kk_flush_gfx_state → kk_upload_descriptor_root + flush_push_descriptors + residency-set
add/remove/commit — the same per-draw path as round 17 addendum 1, now quantified as the
throughput cap: **~420k draws/s on one core ≈ 2.4µs CPU per draw**. The lever is unchanged
and now top of the perf queue: KK dirty-tracking for root/push-descriptor state (helps CPU
and GPU), then event-driven ring wakeups / direct-encode-at-decode (bigger, upstream-level).

## Round 19 (2026-06-11): replay-thread relief — BO-cache cap + slim push uploads, 42→54fps @10k

Attacked the round-18 CPU ceiling (serial ring-thread replay, 2.4µs/draw) with the weighted
sample tree from the 10k run. The surprise: **27% of the thread was allocator/residency
machinery, not memcpy** — `kk_cmd_bo_create` 8.5% + `mtl_residency_set_commit` 7% + pool paths.
Cause: per draw the replay uploads the full 2 KiB push-descriptor array + the 2.2 KiB root
table ≈ 44 MB/frame ≈ ~350 BOs (128 KiB each) per frame, while `KK_CMD_POOL_BO_MAX=32` caps
the pool's free list at 4 MiB — so ~300 Metal buffers are CREATED and DESTROYED every frame.

Two fixes (kk-patches/kk-perf.patch, knobs default-ON in boot-seated-kk.sh, =0 disables):
1. **LIMINA_KK_BOCACHE** — free-BO cache cap 32 → 512 (env value ≥64 = custom cap). Microbench
   (kk-draw-bench attr6 --rebind, 5k draws): 1.07 → 0.84 µs/draw (−22%).
2. **LIMINA_KK_SLIMPUSH** — size push uploads by `layout->non_variable_descriptor_buffer_size`
   (what regular descriptor sets already do) instead of sizeof(data)=2 KiB. Zink pushes
   descriptors every draw, so this scales the upload burn directly. (`set_sizes` has no
   readers; verified write-only.)

**Aquarium 10k: 420k → ~550k draws/s ≈ 54–55 fps (was 42), GPU 41–46%.** Host native is
57–59 — the gap is nearly closed. Correctness: all 1,190 `*push_descriptor*` dEQP-VK cases
A/B status-identical (80 runnable = the monolithic graphics+compute push matrix; rest skip
on unsupported variants), plus the seated desktop is itself a per-draw push-descriptor
workload. BOCACHE is semantics-free (pure caching).

Remaining on the replay thread (ranked): vk_cmd_enqueue deep-copies per command at decode
(malloc per cmd), per-draw root-table upload (2.2 KiB, could slim the unused
dynamic_buffers tail / skip when only sets[] changed), VB-bind compare-skip, and the
structural fix — direct encode at decode instead of record-then-replay (upstream-level).

### Round 19 addendum: post-fix profile + ring relax-cap EXONERATED — the hunt converges

Fresh weighted sample at 10k post-round-19: BO churn and residency are GONE from the hot
tree; the ring thread now has ~33% idle-poll headroom (no longer saturated), GPU 41–50%,
guest 59% idle — **nothing is pegged**, so the residual vs host (54–55 vs 57–59) is
chain latency, not stage throughput. Hottest remaining slice: descriptor bytes handled
3× per draw (venus decode ~5% + vk_cmd_enqueue deep-copy ~9% + replay push-set update
~7%) — only worth attacking via direct-encode-at-decode (upstream-level, on the ledger).

Tested the one cheap latency lever: `LIMINA_RING_RELAX_US` (virgl f29cbb9) caps the ring
poll's unbounded exponential backoff (a burst landing mid-sleep can eat multi-ms). A/B at
10k: **no change** (~550k draws/s both legs; env + fresh dylib verified in the worker) —
ring wake-up latency is not the limiter. Knob kept default-off for future latency work.

**Verdict: at 93–95% of host-native Firefox on the flagship workload, the aquarium gap
hunt is converged.** Remaining residue is most plausibly Firefox-side frame pacing +
guest-side vn_relax fence-wait backoff (guest mesa, bake-time experiment). Side finding
for the hygiene ledger: VN-DBG journal spam costs rsyslogd 30% + abrt 20% of a guest core
under load. Also: LIMINA_KK_STATS itself costs ~2% of the submit thread (limina_stats_bump in
the sample) — drop it for max-perf runs.

## Round 19 erratum + round 20 (2026-06-11): SLIMPUSH truncation bug FIXED; transition flicker pre-existing

**The user's eyeball battery caught a real SLIMPUSH bug.** glmark2 scenes with multi-binding
push sets (texture, shading, effect2d-edge) flickered — background through the window.
Root cause: **pushed descriptors ACCUMULATE** (a push overwrites only the bindings it names
and retains the rest), so sizing the upload by the LATEST push's layout truncates retained
bindings written earlier under a larger layout. Confirmed empirically by a detection counter:
`used=256 > latest-layout=80` firing steadily on those scenes. Fix (kk-perf.patch): track a
per-set high-water mark (`limina_used_size`, monotonic per cmd buffer) and size uploads by it.
Lesson for the patch notes: the upload-size invariant for push sets is NOT the latest
layout's size — regular sets get away with layout-sizing only because each set object has
exactly one layout for life.

**Residual flicker EXONERATES the knobs — it's pre-existing.** After the fix, a fainter
flicker remained, correlated 1:1 with glmark2 SCENE TRANSITIONS (every 10s in the loop;
timeline: IFP2 format sweeps + new winsys image allocs in the guest journal 4s before the
user's "now!", GPU dipping to 0% for 2-11s). Discriminator boot with ALL FOUR KK knobs off:
**flicker still there** → pre-existing first-present-class gap (a freshly allocated winsys
buffer reaches the compositor before first valid render; undefined alpha → background shows
through). New ledger thread; repro = `glmark2-es2-wayland -b texture -b shading -b
"effect2d:kernel=..." --run-forever`, watch a transition. Suspects: zink first-acquire
present, mutter dmabuf import timing, glmark2 attach-before-draw. Also filed: guest sends
CTX_ATTACH/DETACH for a dead ctx=2 every ~20-40s (benign-looking lifecycle wart, worker log).

### Round 20 addendum: round-15 battery re-run (windowed legs, full knob stack + SLIMPUSH fix)

| bench (windowed, in-session)   | round 15 | round 20 | delta |
|--------------------------------|----------|----------|-------|
| glmark2-es2-wayland            | 1983     | 1882     | −5%   |
| glmark2-wayland                | 1852     | 1944     | +5%   |
| vkmark                         | 3419     | 3266     | −4%   |

Flat within run-to-run noise — expected: windowed glmark2/vkmark at 800×600 are
swap/present-bound (scenes run at 2300+ fps internally), so the per-draw knobs don't move
them. The knobs' value shows on draw-heavy workloads (aquarium 5k 16→60fps, 10k 42→54fps).
No broad regression from the round-17/18/19 stack. GBM offscreen legs deferred to the
rebind-cost thread (they're its measurement vehicle, and need the session stopped).

### Round 20, continued: the flicker is PER-SESSION; probe validated; hunt automated

Key session (2026-06-11 afternoon, post-gdm-restart boot): **certifiably clean** — jellyfish
4 min + 2s-transition loop 3 min (~90 transitions) with LIMINA_RED_PROBE armed = **zero bleed
events**, user eyeball agrees, on the exact workloads that flickered repeatedly the previous
boot. The probe demonstrably works: it captured my jellyfish→glmark2 workload switch with
frame-level precision (window unmap = red→95% for 6 frames; mutter map animation = 4-frame
decay back to baseline) — that signature (full-window absence, animated return) is also what
a real flicker event should look like if the surface unmaps, vs a 1-frame square hole if the
compositor loses a texture.

Other findings on the way:
- **Jellyfish steady-state translucency** (user question) = ARGB visual honored by mutter +
  scene writes alpha<1: forcing --visual-config alpha=0 makes the window opaque. Separate
  from the flicker (which persists on a no-alpha visual and = full window absence). Policy
  question (XRGB preference) parked.
- The flicker therefore is NOT alpha bleed-through: with an opaque visual the desktop still
  showed through = mutter composited those frames WITHOUT the window's content.
- A flickery-boot session also DIED under the tight loop (12:35–12:46, no coredump), and
  gnome-shell+vkmark double-SIGSEGV'd at 12:28:39 (cores banked, unsymbolized). Same boot.
  Possibly all symptoms of one per-session defect.
- gdm-restart sessions land in the GNOME overview (probe blind there — dimmed backdrop);
  flicker-hunt.sh escapes it with ydotool per cycle.

**flicker-hunt.sh**: boot-cycles the template (red wallpaper + ydotool baked), counts bleed
frames objectively per session, archives every worker log (/tmp/flicker-hunt/), stops on a
flickery catch. Diff target: evidence/worker-clean-session-2026-06-11.log.gz (the clean
reference). Worker REBUILD GOTCHA hit on the way: bare cargo build strips the hypervisor
entitlement — run crates/limina-vmm/sign.sh after any rebuild (VmCreate fails otherwise).

## Round 21 (2026-06-11): ⭐ FLICKER ROOT-CAUSED — present-before-GPU-complete on the zero-copy path

**The artifact:** single stale frames presented to glass (user-visible as "the window briefly
disappears"). Captured in screen recordings: the stale frames are complete, perfectly
rendered OLD frames — overview content whose on-frame clock matches the event minute.

**Mechanism (convicted by a 3-recording, 8-phase blind A/B):** mutter repaints its
double-buffered KMS surface and flushes; the worker presents the IOSurface to Core
Animation AT FLUSH TIME = mutter's SUBMIT time, before KK/Metal executed the repaint. CA
samples with NO synchronization → shows the buffer's previous content. Normally that's
33 ms old (invisible); when window churn left an unpresented overview/no-window frame in a
buffer, the race exposes it as a visible flicker. The flight recorder shows a perfectly
regular 415/574 present ping-pong through events — nothing wrong at the virtio-gpu level;
the staleness is GPU-timing-only. The famous "alternating stale/clean at 60 Hz for 5
frames" burst = one buffer's repaint lagging repeatedly while its sibling stayed timely.

**The A/B:** LIMINA_PRESENT_COPY (supervisor copies the scanout into a private 3-deep ring
before CA sees it; IOSurfaceLock SYNCHRONIZES WITH PENDING GPU WRITES, so every copied
frame is complete). An automated toggle alternated it every 5 min while the user recorded
the window for ~1 h; a frame-anomaly scanner (spikes tools: /tmp scan — promoted below)
found **25 stale frames in 5 bursts, ALL in copy-OFF phases; zero in 10+ min of copy-ON**
(Poisson p≈0.001). The toggle file (/tmp/limina-present-copy) flips it live.

**Earlier theories now closed:** SLIMPUSH truncation (real bug, fixed, but unrelated to
this artifact) · alpha bleed (refuted: opaque visual still flickered) · per-session coin
flip (was timing variance) · mutter import failure (journal silent) · supervisor id cache
(exonerated: the copy reads through the same cache and fixed it) · ring relax backoff
(exonerated round 19) · buffer-age lie (disfavored: copy-ON clean means under-repair
isn't reaching glass).

**Fix policy:** LIMINA_PRESENT_COPY default-ON in boot-seated-kk.sh (4 MB CPU copy + GPU
sync wait per present at ≤60 Hz — trivial). The REAL fix is fence-accurate presentation
(#8 flip-completion thread): present only after the guest's GPU work for the scanout
buffer completes (vkr fence for the resource), which also unblocks the kmscube stall and
gives mutter honest frame pacing. The copy doubles as the software-2D-style fallback.

Oracles built this round: LIMINA_RED_PROBE (worker present-path bleed detector; frame-precise,
validated on a window unmap), flicker-hunt.sh (boot-cycle objective session verdicts),
/tmp/scan-anomalies.swift + extract-frames.swift (recording forensics — promoted to
spikes/venus-draw-probe/).

**Verification leg (same day):** dedicated copy-ON recording, tight texture/shading loop:
**zero anomalies in 55,925 frames / 16 min** (scan-anomalies threshold 40). Mitigation
confirmed beyond the blind A/B.

**Lock-only variant FAILED (same day):** LIMINA_PRESENT_LOCK (IOSurfaceLock+Unlock the live
guest surface before each present — the zero-copy version of the sync, commit fe79a9a).
Fresh boot, same loop: user saw **several anomalies within seconds** of starting to record
— far worse than untreated copy-OFF (~5 bursts/hour). Scanner-confirmed on the aborted
clip: **6 anomalies in 7.4 s (432 frames)**. Live-toggling copy back on in the same session
(touch /tmp/limina-present-copy, rm the lock marker) returned it to clean — a fresh 8-min
recording scanned **0 anomalies in 27,876 frames**. Verdict: the copy's load-bearing
property is **immutability**, not the GPU sync. Two mechanisms the lock can't close:
(a) at present (= mutter flush) time the repaint may not even be *submitted* to Metal yet
(venus ring decode is async) — IOSurfaceLock can only wait on GPU work the kernel already
knows about, so the "sync" can be a no-op exactly when it matters; (b) even a
complete-at-lock frame is overwritten by the guest's next repaint (~33 ms) while CA is
still sampling it — the same reuse race SURFACE_RING=3 fixes on the 2D path. The copy wins
because CA gets a snapshot nobody ever touches again, regardless of write timing.
Consequence for #8: fence-accurate presents must ALSO provide immutability (hold the
buffer from guest reuse until flip completion — i.e. flip-completion pacing, not fences
alone), or keep the copy for the display hop. LIMINA_PRESENT_COPY stays default-ON;
LIMINA_PRESENT_LOCK kept in-tree as the documented negative result.
