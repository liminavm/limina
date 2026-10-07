# Sizing KosmicKrisp's command-allocator ceiling

`LIMINA_KK_ALLOC_CEILING` caps each KK device's command-allocator pool (`kk_device.h`:
`struct kk_alloc_pool` is a member of the device). At the ceiling an acquire waits up to
`LIMINA_KK_ALLOC_WAIT_MS` for GPU progress and then fails; `0` disables the ceiling.

## The probe

`poolprobe.c` drives the pool past any ceiling on the host, no VM:

- `poolprobe open <n>`: n command buffers begun and never ended.
- `poolprobe stall <n> <fills> <ms>`: n submits held behind an unsignalled timeline semaphore.
- `poolprobe resubmit <n> <fills> <ms>`: one command buffer resubmitted n times (KK re-records it).

`build.sh` builds a private zink-on-KK tree with asserts on; `env.sh` points host GL/VK at it.

## Measured 2026-10-07 (M1 Max, seated F44 enhanced guest, EFI+venus)

`mix.sh` ran in the guest for six minutes: Firefox on the WebGL aquarium (5000 fish),
glmark2, two vkcubes and vkmark in a loop, composited in the GNOME overview.

- The vrend device, which every guest GL client shares, peaked at 78 allocators (67-71
  borrowed at once). About 60 stay borrowed on the idle desktop.
- Each venus device (one per guest Vulkan device) peaked at 1.
- With the ceiling at 512 the same mix peaked at 77 with no waits or refusals, and every client
  rendered (`run2-mid.png`, a window capture mid-run).

`peaks.sh <log>` summarises per-pool peaks from a worker log; `peaks2.txt` is the ceiling-on run.
The default of 512 is about 6.5x the shared device's peak; the growth warning fires at 128.
