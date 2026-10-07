# List-restart probe

`probe.c` checks whether host GL honours primitive restart inside a triangle list, on the stack
vrend draws through (EGL surfaceless → Mesa st → zink → KosmicKrisp). It draws `{0, R, 1, 2, 3}`
as `GL_TRIANGLES` from an element buffer and reads the pixels back: conformant restart draws only
triangle 1-2-3 (right half); ignoring restart draws through vertex 0 (left half).

Build and run against a host Mesa prefix (both runs default KK, no `LIMINA_KK_NOLISTRESTART`):

    cc -I$PREFIX/include probe.c -L$PREFIX/lib -lEGL -o probe
    VK_ICD_FILENAMES=<kk icd json> DYLD_LIBRARY_PATH=$PREFIX/lib:/opt/homebrew/lib \
      MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink LIBGL_DRIVERS_PATH=$PREFIX/lib \
      EGL_PLATFORM=surfaceless ./probe [index|fixed]

| host Mesa                               | index (glPrimitiveRestartIndex) | fixed (FIXED_INDEX) |
|-----------------------------------------|---------------------------------|---------------------|
| limina-kk de950d67e1 (KK skips lists)    | FAIL: left 373, right 39 px     | FAIL: same          |
| restart scan 6295aedf93                 | PASS: left 0, right 748 px      | PASS: same          |

Measured 2026-10-07 on an M1 Max. With list restart left out of zink's supported modes, the GL
frontend sees the restart index in this draw and sends it to primconvert, so KK never gets a list
restart. On the shipping stack zink passes list restart to KK, whose default skip draws the list
as if restart were off.

The probe never reaches KK's restart unroll on either stack, so it cannot hang the GPU.
Running it on the shipping stack with `LIMINA_KK_NOLISTRESTART=0` would.
