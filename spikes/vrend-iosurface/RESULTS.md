# vrend IOSurface world — import (phase 1) + scanout (phase 2)

**Date:** 2026-08-04 · **Status:** BOTH GREEN, human-verified live ·
**Design:** `docs/design/vrend-iosurface-scanout.md` (plan C)

## What this proves

A venus Vulkan client's wayland buffer, composited by a **virgl (vrend)**
compositor, now shows the client's actual rendering, zero-copy. Before this,
the cross-driver `PIPE_RESOURCE_SET_TYPE` import either poisoned the
compositor's context permanently (pre-`5fdb3bba`) or typed the buffer as a
blind placeholder (black window).

The chain, host-side only — no guest changes:

```
vrend set_type (res->iosurface_id)
  → vkr_mtl_iosurface_lookup                     (virglrenderer)
  → virgl_egl_image_from_iosurface               (virglrenderer)
  → EGL_IOSURFACE_LIMINA 0x3B9A EGLImage target  (mesa egl_dri2)
  → dri2_from_iosurface_limina                   (mesa dri)
  → WINSYS_HANDLE_TYPE_IOSURFACE_LIMINA = 6      (mesa, ref in com_obj)
  → zink: plain OPTIMAL VkImage + dedicated memory chaining
    VkImportMemoryMetalHandleInfoEXT{MTLTEXTURE, IOSurfaceRef}
  → KK kk_AllocateMemory: newTextureWithDescriptor:iosurface:plane:0
    from the dedicated image's own kk_image_layout
```

## The probe

`eglimport-probe.c` (`./run-probe.sh`): IOSurfaceCreate(BGRA) → CPU-write a
pattern → surfaceless zink-on-KK EGL → `eglCreateImageKHR(EGL_IOSURFACE_LIMINA)`
→ `glEGLImageTargetTexture2DOES` → FBO readback. Second-scale iteration vs
minutes-scale VM boots; it bisected both mesa faults below.

**Use an R/B-asymmetric color.** v1 used magenta (ff 00 ff) — R/B-symmetric —
and PASSed over a channel swap that only a human eyeball on live vkcube caught.
The probe now writes pure red and explicitly distinguishes "R/B SWAPPED" from
other failures.

## Faults found (all fixed)

1. **mesa/zink: the whole import was compiled out.** `ZINK_USE_DMABUF` is
   `#if !defined(__APPLE__)`; the apple `from_handle` branch and the
   `allocate_bo` metal-handle chaining sat inside it. Symptom: EGL 0x300C at
   image creation, strings absent from the built `.o`. Moved outside the guard
   (mesa `7fa2cd97839`).
2. **mesa/zink: `resource_from_handle` never installed on KK** (no
   external_memory_fd/win32) → NULL call → SIGSEGV. Installed on `__APPLE__`.
3. **virgl/vrend: the imported-BGRA compensation swapped our channels.**
   `vrend_resource_supports_view()`'s `is_imported && is_bgra` arm models the
   GBM/dmabuf world where EGLImage-backed BGR* textures sample in raw byte
   order; it drove a sampler-view R/B swap, an rgb→bgr fragment-output swizzle,
   a glClearColor swizzle, and srgb skip-decode. Our imports are real VkImages
   through zink — **natively ordered** (probe: BGRA-written pure red reads back
   RGBA `ff 00 00 ff`). Exempted on `__APPLE__` (virgl `acdb43f5`), including
   the 24bpp `GL_RGB8` internal-format override — a GBM quirk that would have
   re-engaged the swap for the `B8G8R8X8` buffers gnome-shell actually declares.
4. **Placeholder fallback must be zeroed** — glTexStorage contents are
   undefined; an unzeroed placeholder shows stale GPU memory (cross-context
   info leak). (User catch.)

## Validation

- Probe: PASS, native channel order.
- Live: `vrend-arm.raw`, gnome-shell on vrend, vkcube on venus — three 500×500
  `B8G8R8X8_UNORM` swapchain buffers adopted (worker log
  `adopted IOSurface id …`), spinning cube with correct colors
  (human-verified), no context poison (`check-gpu-context-health.sh` OK,
  guest 0x1200 count at benign baseline).

## Phase 2 — scanout (virgl `d042ed65`, same day)

The scanout direction: `vrend_resource_iosurface_init` now makes the display
IOSurface the scanout texture's **storage** through the same EGLImage chain;
`vrend_renderer_resource_sync_iosurface` degenerates to a completion barrier
(`glFinish`). The A1 pinned-PBO readpixels blit became the fallback when the
EGLImage is refused; the CPU readback tier below that is untouched.

Proven before wiring by two new probe arms (both PASS):
- **render-into**: GPU `glClear` into the imported texture lands in the
  IOSurface bytes, natively ordered — the fundamental scanout contract.
- **texsubimage**: vrend's GLES transfer upload (CPU-swizzled RGBA-order data
  with `GL_RGBA`/`GL_UNSIGNED_BYTE`) is accepted against the native-BGRA
  EGLImage texture and converted so the final surface bytes equal the guest's
  original BGRA — the fbcon/dumb-BO path survives.

Live validation: fbcon + GDM + session at 2560×1440 `B8G8R8X8_UNORM`, all
three scanouts EGL-backed (worker log), zero readback blits, vkcube (import
half) running on top simultaneously — boot text, desktop, colors, and cube
all human-verified; health gate clean. The zink→venus session arm was also
re-validated against this host stack (all good) before the switch.

**Bonus fix:** resource destroy freed `egl_image` only under
`#ifdef ENABLE_GBM` (off on macOS) — every destroyed EGLImage-backed resource
leaked the image and pinned its VkImage/MTLTexture/IOSurface. Now guarded by
`HAVE_EPOXY_EGL_H`.

**Open (carried from A1):** fence-accurate present (the completion barrier is
`glFinish` — correct but unpaced).
