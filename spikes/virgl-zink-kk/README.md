# spike: virgl-over-zink-on-KosmicKrisp (host GL for vrend, no ANGLE)

## Why

limina hard-disables virgl (`NO_VIRGL`, `crates/limina-vmm/src/krun/mod.rs`) because
virglrenderer's `vrend` GL renderer needs an EGL/GLX **GL context** and Apple Silicon
ships no system GL/EGL. The documented baseline-3D plan was **virgl-over-ANGLE**
(ANGLE = GLES+EGL on Metal). This spike tests a cheaper alternative that reuses a stack
we already own: **zink-on-KosmicKrisp** as vrend's host GL provider.

- **KK** (KosmicKrisp) = mainline Mesa's Vulkan-on-Metal driver. We already build it
  natively on macOS arm64 from `/Volumes/mesa-cs/mesa` (build at `/Volumes/mesa-cs/build-kk`,
  `-Dplatforms=macos -Dvulkan-drivers=kosmickrisp`).
- **zink** = Mesa's gallium GL→Vulkan driver. Running zink **on KK** gives desktop GL on
  Metal, from the *same Mesa tree* — no new mega-dependency. (This path is already proven
  end-to-end in the guest today: guest zink → venus → host KK; here we run zink directly
  on host KK, dropping the venus hop.)

If a host GL context comes up on zink-on-KK, virglrenderer's vrend can be built against it
and we get **accelerated GL for the stock 4 KiB guest** via virgl's copy model — immune to
the 16k/4k `hv_vm_map` page wall (unlike venus). venus stays the 16k enhanced fast path.

## The one unknown this spike must kill

Mesa-on-macOS today = GLX-via-CGL (XQuartz/X11, `src/glx/apple/`) + the macos **Vulkan WSI**.
There is **no native headless EGL** for the `macos` platform (no `platform_macos.c` in EGL).
But `platform_surfaceless.c` + `platform_device.c` are compiled **unconditionally** whenever
EGL is built. So the crux:

> Does Mesa's **surfaceless** (or device) EGL platform build and `eglInitialize` on Darwin,
> WITHOUT DRM/GBM, and can **zink** bring up a GL context on it pointing at the **KK ICD** —
> enough to `glClear` + read back the cleared color?

- **Pass** → zink-on-KK is the host GL backend; next step is building vrend against it +
  advertising the VIRGL2 capset.
- **Surfaceless too DRM-entangled** → the fallback is writing `platform_macos.c` (an
  offscreen EGL platform modeled on surfaceless, IOSurface-backed) — bounded, ownable work.
- **Total wall** → fall back to virgl-over-ANGLE.

## Files

- `build-mesa-zink-kk.sh` — reconfigure a fresh Mesa build (`/Volumes/mesa-cs/build-zink-kk`)
  from the KK source tree, adding zink + opengl + egl (surfaceless). Native macOS build.
- `eglprobe.c` — minimal headless probe: `eglGetPlatformDisplay(SURFACELESS)` → init →
  GL context → `glClear` → `glReadPixels`. Prints `GL_RENDERER` (expect zink/KosmicKrisp)
  and the read-back pixel.
- `RESULTS.md` — findings (read the numbers before writing the conclusion).
