# crossmark — cross-API graphics tier probe

The same scene rendered by a GL backend (`crossmark_gl.c`, EGL + GLES 3.0) and
a Vulkan backend (`crossmark_vk.c`), so the GL-vs-Vulkan question gets a
same-workload answer on every tier instead of comparing glmark2 scenes with
vkmark scenes. Successor to the 2026-07-28 tier battery
(`perf/2026-07-28-tier-battery.md`), whose numbers predate virgl 0049-0056 and
kk 0014/0015.

## The matrix

| cell | binary | env |
|---|---|---|
| vrend (virgl tier) | crossmark-gl | stock guest boot (virgl tier) |
| zink-on-venus | crossmark-gl | enhanced guest, venus ICD |
| venus | crossmark-vk | enhanced guest, `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json` |
| host zink-on-KK (GL ref) | crossmark-gl | host, zink env from boot-enhanced-efi-kk.sh |
| host KK (VK ref) | crossmark-vk | host, `VK_ICD_FILENAMES=<mesa-cs build-kk devenv json>` |

## Shapes (`-S`)

- `draws` — N flat triangles, one per-draw constant update (command-stream
  throughput; drawstorm's shape). `-n` sets N.
- `state` — N draws cycling 8 program/pipeline variants (bind + uniform +
  draw). Where GL-state -> vkPipeline translation hurts zink.
- `upload` — per frame: 1 MiB of texels streamed through a buffer (PBO
  orphan / staging copy) into a 512x512 texture, then 100 quads sampling it.
- `desktop` — a compositor-ish frame: 6 large + 40 small textured quads +
  clear. Few draws, big raster area.

Both backends use the *idiomatic* mechanism per API on purpose (glUniform4fv
vs push constants, glUseProgram vs pipeline bind, PBO vs staging buffer): the
comparison is what real apps pay on each API, not synthetic parity.

Timing sections per frame: `draw` (API call loop incl. upload writes),
`flush` (glFlush / vkQueueSubmit), `sync` (glFinish / vkWaitForFences).

## Scene identity

Per-draw parameters are pure functions in `crossmark.h` (`cm_draw_params`),
textures are deterministic patterns, filtering is NEAREST, dither disabled,
and both APIs' readbacks put the NDC y=-1 row first — so the `pixel-hash`
lines are directly comparable across backends and tiers. Equal hashes prove
the workloads are identical; a mismatch means eyeball the dumps before
trusting a comparison (fp math may differ across drivers — vrend's TGSI path
especially — so a mismatch is a *flag*, not automatically a bug). Trap: guest
glReadPixels has returned black before (#28) — the GL backend warns on an
all-zero readback rather than trusting it.

## Present axis (`-p` windowed, `-F` fullscreen)

Offscreen isolates command-stream + render cost, but skips the
scanout/present path (venus fence-present, vrend timer present, zero-copy
IOSurface) — `-p`/`-F` render the same shapes to a Wayland xdg-toplevel
instead, uncapped: the GL leg uses `wl_egl_window` + `eglSwapInterval(0)`,
the Vulkan leg a `VK_KHR_wayland_surface` swapchain preferring mailbox then
immediate (it logs the mode it got — fifo means VSYNC-CAPPED, distrust the
number). Timing gains a `present` section (acquire + queue-present / swap);
`sync` is 0 in present mode (present-paced, no glFinish). The pixel hash is
disabled (no swapchain readback; content is validated offscreen).

Present legs are guest-only (macOS has no Wayland; present *is* the
guest→host scanout path). Run them inside the seated GNOME session
(`WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000` over ssh), PIN
the display mode first (`scripts/perf/set-guest-display.py --write-config
1280x800 1.0` + reboot — the perf-display-pinning rule), and confirm which
scanout path engaged via the worker's `[FLUSH2]`/`[FENCEPRESENT]` DIAGs.
v1 limitation: no swapchain recreate — a resize mid-run exits(2); rerun.

Env knob: `CM_GL_NO_PBO=1` switches the GL upload shape to client-pointer
`glTexSubImage2D` (diagnostic for PBO-path driver bugs; see RESULTS.md).

## Run recipes

Guest (over ssh, no seated session needed for offscreen):

    sudo dnf install -y gcc make vulkan-loader-devel glslang mesa-libEGL-devel mesa-libGLES-devel
    make
    VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json ./crossmark-vk -S draws -n 10000
    ./crossmark-gl -S desktop      # hits whatever GL stack the tier provides

Host references (from this dir):

    VK_ICD_FILENAMES=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./crossmark-vk -S draws -n 10000

Sample the worker DURING a run and verify the probe is alive before AND
after (`pgrep crossmark`) — pipeline creation delays the real work by seconds
(same trap as drawstorm).

Results land in RESULTS.md when the matrix runs.
