# Trace-replay graphics tests — phase-1 spike results (2026-06-12)

Plan context: memory `limina-trace-replay-plan` (apitrace/gfxreconstruct replay tests;
correctness = llvmpipe-reference pixel compare; perf = trend ledger, NOT a gate).
This spike validated the pipeline pieces and found a real tier-2 bug.

## Validated

- **apitrace 13.0 is in Fedora 43 repos**; `apitrace trace --api egl` captures
  glmark2-es2-wayland IN the seated session on real zink→venus
  (`GL_RENDERER: zink Vulkan 1.3(Virtio-GPU Venus (Apple M1 Max) (MESA_KOSMICKRISP))`,
  build scene score 2313 vs 602 on llvmpipe — 4×). ~1 MB trace for 2s @512x512.
- **llvmpipe replay of a venus-captured trace works** (`eglretrace --headless
  --benchmark`, 4628 frames) → the reference-render comparison design is sound.
- **ENV TRAP (cost the first hour):** SSH shells do NOT inherit the session's
  `environment.d` zink env. Without it the GL stack tries classic virgl (`CTX_CREATE
  init=0x2` → EINVAL, our worker is venus-only) and **silently falls back to
  llvmpipe** — capture AND both replays ran identical-llvmpipe and the A/B numbers
  matched at ~810fps. Invariance smelled, worker log confirmed (no `init=0x4`).
  **Always verify the backend via the worker log (`CTX_CREATE ... init=0x4` = venus)
  or GL_RENDERER**, never assume from env.
- eglretrace on Xwayland needs `DISPLAY=:0` + `XAUTHORITY=/run/user/1000/
  .mutter-Xwaylandauth.*` (mutter's private auth file).

## BLOCKED + the bug it found: X11 EGL clients crash on zink→venus

`eglretrace` is X11-only (no Wayland/surfaceless backend; `EGL_PLATFORM=surfaceless`
still XOpenDisplay()s), and **any X11 EGL GL client on zink→venus segfaults** —
reproduced with plain `glmark2-es2` under Xwayland (exit 139), so this is a real
user-facing tier-2 defect (X11 GL apps on the venus desktop), not an apitrace quirk.
The all-Wayland seated desktop never exercises this path.

Backtrace (coredumpctl, /opt/mesa-zink build):
```
#0  0x0 — NULL FUNCTION POINTER CALL
#1  zink_kopper_displaytarget_create   (libgallium-26.2.0-devel.so)
#2  resource_create.constprop
#3  kopper_allocate_textures
#4  dri_st_framebuffer_validate
... eglMakeCurrent (X11/DRI3 path)
```
Lead (NOT verified as cause): zink warns `Vulkan device ... doesn't support base Zink
requirements: have_EXT_custom_border_color`. The crash is a missing-WSI/loader-entry
null deref in kopper's X11 displaytarget path — we own `/opt/mesa-zink`
(scripts/build-mesa-zink.sh, mesa main @3515c52 + patches/mesa/) and can fix it there.

## Consequences for the test plan

1. **Fix the kopper X11 null-deref first** — it unblocks BOTH the replay tests and
   X11 GL apps on the desktop. Until then venus replay cannot run.
2. After the fix, the `venus_replay` test shape stands as planned: capture fixtures
   via this spike's script, replay venus + llvmpipe in the same boot, snapshot
   (`-S`/`--snapshot-interval`), tolerance-compare. (Snapshot comparison itself is
   still UNTESTED on venus — first thing to prove post-fix.)
3. Longer-term alternative if X11 stays awkward: patch apitrace for a Wayland or
   surfaceless retrace backend (we own everything).

## Files

- `capture-replay.sh` — the spike driver (capture on venus, replay both backends;
  the venus replay crash is the live repro for the kopper bug).
- Trace fixtures are regenerable + gitignored (like the `.raw` images); current
  test trace: `~/traces/glmark2-build.trace` in the guest, copy at
  `/tmp/glmark2-build.trace` on the host.
