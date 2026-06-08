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
