# RESULTS — virgl-over-zink-on-KosmicKrisp

**Date:** 2026-06-20 · M1 Max, macOS 26.5 · Mesa 26.2.0-devel (git-178a3d7396, the KK tree)

## Verdict: SHIPPED — virgl works end-to-end on a stock 4 KiB guest via zink-on-KK. No ANGLE needed.

The coexist GPU now does what the goal asked: a fully-capable guest picks **venus** (16 KiB
enhanced tier), a stock 4 KiB guest picks **virgl** (accelerated baseline, copy model → immune to
the 16k/4k `hv_vm_map` wall), and **software-2D** remains the final floor. Host GL for virgl's
`vrend` comes from zink-on-KosmicKrisp via surfaceless EGL — the same Mesa tree we build for venus.
Details below; the end-to-end guest result is in "Step 2 VERIFIED" further down.

The load-bearing unknown (can Mesa's headless EGL bring up a GL context on zink → KosmicKrisp →
Metal on Darwin?) is **resolved, pixel-verified**:

```
GL_RENDERER : zink Vulkan 1.3 (Apple M1 Max (MESA_KOSMICKRISP))
GL_VERSION  : OpenGL ES 3.1 Mesa 26.2.0-devel
readback px : (0, 128, 255, 255)        ← exactly glClearColor(0, 0.5, 1, 1)
RESULT      : PASS
```

`eglprobe` got a surfaceless `EGLDisplay`, initialized EGL 1.5, created a GLES2 context, cleared
to a known color, and `glReadPixels` returned that exact color — through the full
`zink → KK → Metal` path. The renderer string literally names KosmicKrisp. This means the host
GL provider for virglrenderer's `vrend` can be **zink-on-KK from the Mesa tree we already build**,
instead of standing up ANGLE.

## How it built (native macOS arm64, from the KK Mesa tree)

`build-mesa-zink-kk.sh` reconfigures `/Volumes/mesa-cs/mesa` (KK source) into
`/Volumes/mesa-cs/build-zink-kk`, adding the GL stack to KK's macos Vulkan config:

```
-Dplatforms=macos -Dvulkan-drivers=kosmickrisp -Dgallium-drivers=zink
-Dopengl=true -Dgles2=enabled -Degl=enabled -Degl-native-platform=surfaceless
-Dglx=disabled -Dglvnd=disabled -Dshared-llvm=enabled
-Dmoltenvk-dir=$(brew --prefix molten-vk) -Dprefer_static=true -Dbuildtype=debug
```

The obstacles were all **mundane, not architectural** — every wall was a missing tool or a
config default, never "macOS can't do this":

1. **LLVM keg-only** — KK requires CLC→LLVM (`with_kosmickrisp_vk ∈ with_driver_using_cl`); LLVM 22
   is keg-only so `llvm-config` isn't on PATH. Prepend `$(brew --prefix llvm)/bin`.
2. **expat keg-only** — the EGL/dri driconf parser needs it; add its pkgconfig.
3. **bison too old** — Apple's `/usr/bin/bison` is 2.3 (2008); Mesa's GLSL glcpp grammar needs
   > 2.3. KK (opengl=false) never built glcpp so it never hit this. `brew install bison`, prepend
   its bin, and **reconfigure** (meson bakes the bison path into build.ninja at configure time).
4. **`_EGL_PLATFORM_MACOS` undefined** — `macos` is a valid *windowing* platform but NOT a valid
   *egl-native-platform* (no such enum). With `egl=enabled` Mesa appends `surfaceless` to the
   platform list, but `auto` picks `platforms[0]=macos`. Pin `-Degl-native-platform=surfaceless`.
5. **zink `dlopen("libvulkan.1.dylib")`** — bare soname; Homebrew's loader isn't on dyld's default
   path. Export `DYLD_FALLBACK_LIBRARY_PATH=/opt/homebrew/lib` **inside** the run script (dyld
   strips `DYLD_*` across the SIP-protected `/bin/bash` boundary, so it must be set after bash).
6. **driconf assert** (the one code change) — on the `surfaceless → sw-vk → zink` loader path,
   `config->options_info` is **not** populated with zink's driconf option descriptions, so the
   cache is empty and `zink_internal_create_screen`'s first `driQueryOptionb()` trips
   `assert(info[i].name != NULL)`. Patched zink to guard each query with `driCheckOption` (safe on
   an empty cache) + option default — see `patches/zink-driconf-tolerate-empty-options.patch`.

## Desktop GL confirmed (2026-06-20) — works, capped at GL 3.1

`glprobe.c` (desktop-GL textured-triangle, `eglBindAPI(EGL_OPENGL_API)`, GL loaded via
`eglGetProcAddress` — no libGL since glx/glvnd are off) is a second **PASS**:

```
ctx granted : 3.0  →  GL_VERSION : 3.1 Mesa   (desktop GL: YES, not ES)
inside  px  : (255,0,255,255)   ← exact magenta texture color → texture sampling works
outside px  : (0,0,0,255)       ← black clear                 → rasterization works
RESULT      : PASS — desktop GL textured-triangle render on zink-on-KK
```

Full pipeline through zink→KK→Metal: VAO/VBO, compiled GLSL 1.40 vertex+fragment program, texture
upload+sample, rasterized triangle (inside=texture color, outside=clear). So desktop GL is real here,
not just ES.

**But the version is capped at GL 3.1 (GLSL ≤ 1.40; zink also offers ES 3.0/3.1).** Every core-profile
request (3.2/3.3/4.1 core, 4.1 compat) fails `EGL_BAD_MATCH`; only a 3.0-request context is granted and
it reports 3.1. Cause: zink gates GL ≥ 3.2 / 4.x on Vulkan features KK doesn't yet expose — the visible
one is `VK_EXT_custom_border_color` (zink's "base requirements" warning). **Lifting the GL ceiling = add
those extensions to KK** (we own it). GL 3.1 is a usable baseline for vrend/virgl; 3.3+ is an upgrade.

## Step 1 DONE (2026-06-20): vrend initializes on zink-on-KK

virglrenderer's `vrend` GL renderer now comes up on zink-on-KK through its own surfaceless EGL
winsys (`spikes/virgl-zink-kk/vrendprobe.c`):

```
virgl_renderer_init(USE_EGL | USE_SURFACELESS | USE_GLES) = 0
VIRGL2 capset: version=2 size=1408
RESULT: PASS — vrend initialized on zink-on-KK, VIRGL2 caps present
```

Build chain (all native macOS, separate prefixes so the working venus-only `virgl-prefix` is
untouched): `build-epoxy-egl.sh` → `third_party/epoxy-egl-prefix` (epoxy WITH EGL; Homebrew's is
CGL-only), then `build-virglrenderer-gl.sh` → `third_party/virgl-gl-prefix` (`-Dplatforms=egl`,
venus + vrend; on Darwin virglrenderer enables EGL without GBM).

Two issues fixed to get here:
1. **virglrenderer never wired the no-GBM EGL path.** `vrend_winsys_init`'s `USE_EGL` branch was
   gated entirely on `ENABLE_GBM`; on macOS (no GBM) it fell to `#else` → "EGL is not supported on
   this platform". The display-id/surfaceless `virgl_egl_init` variant already existed but was
   never called. Patched to connect it on `HAVE_EPOXY_EGL_H && !ENABLE_GBM` →
   `patches/virglrenderer-vrend-egl-no-gbm-macos.patch`.
2. **Must use the GLES host path (`USE_GLES`), not desktop GL.** With desktop GL, epoxy routes GL
   calls to Apple's *system* OpenGL framework (`libGL.dylib`) → `glFlush` segfaults (no CGL
   context). GLES calls dlopen `libGLESv2` by name → resolve to our zink-on-KK Mesa. This is the
   same host-GLES model the ANGLE-based vrend uses; guest GL is unaffected (virglrenderer
   translates guest GL → host GLES). Also: cookie must be non-NULL for the vrend path.

Next: wire it into the worker (step 2) — drop the hard `NO_VIRGL`, init vrend on the coexist device
(GLES + surfaceless), advertise the VIRGL2 capset, keep software-2D fallback.

## Step 2 (2026-06-20): worker integration done; host vrend proven in the REAL worker

Committed `feat(gpu): enable virgl/vrend (zink-on-KK) on the coexist device`. The worker
(`limina-vmm`) now links the EGL virglrenderer + our epoxy-egl, is signed with
`allow-dyld-environment-variables`, and on a real boot its log shows **48 ASTC `GL_INVALID_ENUM`
cap-probe lines** — i.e. vrend's zink-on-KK GL init ran *inside the production worker* (same
signature as `vrendprobe`), not just the standalone probe. venus path intact, no crash.

**(Superseded — see Step 2 VERIFIED below.)** An earlier note here claimed guest-side
verification was "BLOCKED by a GOP firmware-splash boot hang." **That was a misdiagnosis**: the
boots were never hung — a frozen `--display-capture` PNG and a too-short SSH wait were read as a
hang. Serial capture (`--console`) showed every guest reaching `gdm.service` + `fedora login:`;
big CoW first-boots under software render simply take ~5–8 min to reach SSH. Wait longer and the
guest comes up. (Retraction recorded in memory `limina-windowed-reboot-present-race`.)

## Step 2 VERIFIED END-TO-END (2026-06-20): stock 4 KiB guest renders through virgl→zink→KK→Metal

A **stock Fedora 43 (4 KiB) guest** booted on the coexist GPU device and bound the virgl gallium
driver, with the host GL provider resolving to zink-on-KK. Decisive, pixel-verified:

```
guest dmesg : [drm] features: +virgl +edid +resource_blob +host_visible
              [drm] number of cap sets: 5
guest eglinfo:
  OpenGL compatibility profile renderer: virgl (zink Vulkan 1.3(Apple M1 Max (MESA_KOSMICKRISP)))
  OpenGL ES profile renderer:            virgl (zink Vulkan 1.3(Apple M1 Max (MESA_KOSMICKRISP)))
worker log  : virtio-gpu virgl_flags = 0x35b, software_2d = false (coexist = true)
capture PNG : /tmp/f43c-cap.png (704 KB) — fully rendered GNOME desktop (wallpaper, dock, notification)
```

So the full chain is live in the real worker: **guest GL → guest virgl driver → virtio-gpu → host
virglrenderer/vrend → zink → KosmicKrisp → Metal.** The renderer string literally names virgl AND
KosmicKrisp, and the desktop composites through it (pixel proof, not a proxy). venus path intact,
no crash, software-2D still the floor.

**Follow-up (non-fatal, deferred per user):** a small bounded count of guest-side
`virtio_gpu` `ERR_UNSPEC` (0x1200) responses on some SUBMIT_3D/CTX_DETACH commands — GL still works
and the desktop renders. Likely GL-3.1-cap / missing-feature mismatches; root-cause alongside the
KK feature work that lifts the GL ceiling (see `limina-kk-feature-gaps`). Boot the test path with
`spikes/virgl-zink-kk/boot-virgl-guest.sh`.

## Caveats / known gaps (for productization, not blockers)

- **KK is missing `VK_EXT_custom_border_color`** (and likely more), which zink lists as a base
  requirement →
  `WARNING: Some incorrect rendering might occur`. The clear still rendered correctly (warning, not
  error), but it's a real KK feature gap. We own KK → add the extension (or let zink emulate). Lead,
  not cause — verify per-feature against real pixels before trusting/fearing it.
- **driconf `options_info` wiring** — the proper fix (vs. the tolerate-empty patch) is feeding
  zink's option descriptions into the drisw/kopper loader path so driconf works normally.

## Next steps (the core path is done — these are upgrades / productization)

1. ~~Confirm a **desktop-GL** context + a textured/triangle draw on zink-on-KK.~~ **DONE** (GL 3.1).
2. ~~Build **virglrenderer's `vrend`** against this Mesa, drop `NO_VIRGL`, init with `USE_EGL`.~~ **DONE.**
3. ~~Advertise a real **VIRGL2 capset** in the coexist virtio-gpu; let the 4 KiB guest bind it.~~
   **DONE** (5 capsets; guest dmesg `+virgl`).
4. ~~Guest validation: stock 4 KiB Fedora renders GNOME **accelerated** via virgl.~~ **DONE**
   (eglinfo renderer = `virgl (zink … KosmicKrisp)`; rendered-desktop capture).

Remaining (deferred per user — "first make it work, then add features to KK"):

- **GL ceiling: 3.1 → 3.2 by adding `ARB_depth_clamp` (re-probed 2026-06-30). GEOMETRY-SHADER
  THEORY WAS WRONG.** Two probes, in order:
  1. Advertised `VK_EXT_custom_border_color` (`0005`) → its base-requirement warning gone, but GL
     **still 3.1** (`glprobe-after-0005.txt`). So custom_border_color was a **lead, not the cause**
     (as this RESULTS flagged at line ~161). I then *wrongly* concluded geometry shaders were the
     cap (KK lacks `geometryShader`; Metal has no GS). **That was itself a lead-not-cause error** —
     disproven by the next probe.
  2. Advertised `VK_EXT_depth_clip_enable` (`0006`) → GL **3.1 → 3.2 core** (`ctx granted 3.2 core`;
     `glprobe-after-0006.txt`), zero warnings. **`ARB_depth_clamp` was the real 3.1 gate** — Mesa's
     desktop-GL `ver_3_2` (`src/mesa/main/version.c`) requires `ARB_depth_clamp` (which zink exposes
     via `VK_EXT_depth_clip_enable`), and **geometry shaders are NOT in the desktop 3.2/3.3 gates**
     (only the GLES `OES_geometry_shader` path). KK already had core `depthClamp` + the Metal
     `setDepthClipMode` bridge; `0006` just exposes the EXT + honors the decoupled state.
  **Now at GL 3.2 core.** Next gate for 3.3 = the 3.3 ext set (`ARB_blend_func_extended`/`dualSrcBlend`,
  `ARB_timer_query`, `ARB_instanced_arrays`, …), NOT geometry shaders — enumerate KK vs those before
  chasing 3.3. The enhanced **venus** tier is unaffected (Vulkan, not GL-version-gated). Lesson (again):
  don't conclude a cap from a missing-feature observation — *test* it. Tracked in `limina-kk-feature-gaps`.
- **Root-cause the bounded `0x1200` (ERR_UNSPEC)** SUBMIT_3D/CTX_DETACH responses — non-fatal.
- **Production `.app` bundling** of zink-kk Mesa + epoxy-egl + KK ICD via `@rpath` (no `DYLD_*` /
  no `VK_ICD_FILENAMES` at runtime) — the dev path uses `boot-virgl-guest.sh`'s env exports.
- **Characterize the venus→virgl→sw selection** more fully (host advertises all capsets; guest Mesa
  picks per its kernel page size / driver support).

---

## 2026-07-28 — iosurfpbo: vrend→IOSurface pinned-PBO scanout (design plan A1) — PASS

Probe: `iosurfpbo.c` (`run-probe.sh iosurfpbo`). Question: on the exact vrend host config
(surfaceless EGL, GLES 3.1, zink-on-KK) can glReadPixels into a `GL_AMD_pinned_memory`
PBO whose client memory is `IOSurfaceGetBaseAddress` land pixels in the IOSurface, and
what does it cost? (docs/design/vrend-iosurface-scanout.md)

```
GL_AMD_pinned_memory : EXPOSED     (after patches/kosmickrisp/0014 — mesa gated it
                                    to desktop GL; the gallium path is API-agnostic)
EGL_ANDROID_native_fence_sync : ADVERTISED, create OK, wait SATISFIED
glFenceSync                   : OK, wait SATISFIED
pixel corner(8,8)  = (255,26,26,255)  OK   ← scissored region, via IOSurfaceLock readback
pixel body(504,504)= (51,102,153,255) OK
A1 blit 2560x1440 (draw + glReadPixels + glFinish): 0.65 ms/frame (60 frames)
RESULT: PASS
```

Findings:
- **A1 is viable and cheap**: 0.65 ms/frame *worst-case* (full glFinish per frame) vs the
  ~6 ms/frame release-build CPU chain (readback+convert+upload) it replaces — and the new
  cost is GPU-side, freeing the CPU entirely. RGBA IOSurface + GL_RGBA readback avoids
  any swizzle.
- **The suspected-broken EGL native fence path works** for create+wait on zink-on-KK
  (`vrend_state.use_egl_fence` will be true and functional). `eglDupNativeFenceFDANDROID`
  export was NOT probed — vrend's `do_wait` prefers dup+poll and falls back to
  `eglClientWaitSync`; verify the fallback engages cleanly during wiring.
- `GL_PACK_ROW_LENGTH` handles IOSurface `bytesPerRow` ≠ tight rows; the 512-wide test
  surface got bpr=2048 (tight) — 2560-wide also tight (10240). Non-tight cases remain
  covered by the row-length pixelstore.
