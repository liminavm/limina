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
