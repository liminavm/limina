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

## Caveats / known gaps (for productization, not blockers)

- **GLES 3.1, not desktop GL yet.** The probe requested the GLES API; the build has `opengl=true`
  too. Confirming a desktop-GL (≥3.3 core) context on zink-on-KK is a quick follow-up — virgl's
  `vrend` can drive either, but desktop GL is the nicer target for Linux GL guests.
- **KK is missing `VK_EXT_custom_border_color`**, which zink lists as a base requirement →
  `WARNING: Some incorrect rendering might occur`. The clear still rendered correctly (warning, not
  error), but it's a real KK feature gap. We own KK → add the extension (or let zink emulate). Lead,
  not cause — verify per-feature against real pixels before trusting/fearing it.
- **driconf `options_info` wiring** — the proper fix (vs. the tolerate-empty patch) is feeding
  zink's option descriptions into the drisw/kopper loader path so driconf works normally.

## Next steps (the wiring, now that the unknown is dead)

1. Confirm a **desktop-GL** context + a textured/triangle draw (not just clear) on zink-on-KK.
2. Build **virglrenderer's `vrend`** against this Mesa (its `USE_EGL`+GLES path → surfaceless EGL →
   zink → KK), drop `NO_VIRGL`, init with `USE_EGL`.
3. Advertise a real **VIRGL2 capset** in the coexist virtio-gpu; let the 4 KiB guest's virgl driver
   bind it (copy model → immune to the 16k/4k `hv_vm_map` wall).
4. Guest validation: stock 4 KiB Fedora renders GNOME/glmark **accelerated** via virgl; characterize
   the venus→virgl→sw selection. venus stays the 16k enhanced fast path.
