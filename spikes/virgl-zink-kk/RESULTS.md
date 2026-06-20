# RESULTS — virgl-over-zink-on-KosmicKrisp

**Date:** 2026-06-20 · M1 Max, macOS 26.5 · Mesa 26.2.0-devel (git-178a3d7396, the KK tree)

## Verdict: PASS — host GL on zink-on-KK works via surfaceless EGL on macOS. No ANGLE needed.

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

## Caveats / known gaps (for productization, not blockers)

- **KK is missing `VK_EXT_custom_border_color`** (and likely more), which zink lists as a base
  requirement →
  `WARNING: Some incorrect rendering might occur`. The clear still rendered correctly (warning, not
  error), but it's a real KK feature gap. We own KK → add the extension (or let zink emulate). Lead,
  not cause — verify per-feature against real pixels before trusting/fearing it.
- **driconf `options_info` wiring** — the proper fix (vs. the tolerate-empty patch) is feeding
  zink's option descriptions into the drisw/kopper loader path so driconf works normally.

## Next steps (the wiring, now that the unknown is dead)

1. ~~Confirm a **desktop-GL** context + a textured/triangle draw on zink-on-KK.~~ **DONE** (GL 3.1,
   `glprobe.c` PASS). Optional follow-up: add `VK_EXT_custom_border_color` (+ whatever else zink
   gates on) to KK to lift the GL ceiling to 3.3/4.x core.
2. Build **virglrenderer's `vrend`** against this Mesa (its `USE_EGL`+GLES path → surfaceless EGL →
   zink → KK), drop `NO_VIRGL`, init with `USE_EGL`.
3. Advertise a real **VIRGL2 capset** in the coexist virtio-gpu; let the 4 KiB guest's virgl driver
   bind it (copy model → immune to the 16k/4k `hv_vm_map` wall).
4. Guest validation: stock 4 KiB Fedora renders GNOME/glmark **accelerated** via virgl; characterize
   the venus→virgl→sw selection. venus stays the 16k enhanced fast path.
