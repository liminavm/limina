# Fedora's venus crashes the headless vkcube replay: a guest Mesa regression

**Symptom.** `gfxrecon-replay --wsi headless fixtures/traces/vkcube.gfxr` on the distro venus ICD
(`/usr/share/vulkan/icd.d/virtio_icd.aarch64.json`) dies with SIGSEGV (rc 139) in `virtgpu_submit`,
from `vn_QueueSubmit` in gfxreconstruct's virtual swapchain image transition after
`vkAcquireNextImageKHR`. limina's venus (`/etc/vulkan/icd.d/limina-venus.json`) replays the same
trace in the same boot, and vkmark runs on the distro ICD.

**Cause: Fedora's Mesa, between 26.1.8 and 26.2.6.** Not the host, not the guest kernel.

`probe.sh` runs one arm. Measured 2026-11-23, every arm twice in one boot:

| arm | host | guest (`metal.test`) | distro venus |
|---|---|---|---|
| g0 | limina `f84263d2` | 7.3.4, Mesa 26.2.7 | SEGV, SEGV |
| g1 | limina `f84263d2` | 7.2.15, Mesa 26.2.6 (backup before 2026-11-20) | SEGV, SEGV |
| g2 | limina `f84263d2` | 6.19.10, Mesa 26.1.8 (backup before 2026-11-04) | 1620, 1783 fps |
| h1 | bundle `2026-11-22-2` (before the fork rebase) | 7.3.4, Mesa 26.2.7 | SEGV, SEGV |
| h2 | bundle `2026-10-15-1` | 7.3.4, Mesa 26.2.7 | SEGV, SEGV |
| s1 | limina `f84263d2` | 6.19.10, Mesa 26.1.8, **26.2.7's `libvulkan_virtio.so`** | SEGV, SEGV |

- **The host is ruled out:** a bundle from 2026-10-15, when this replay last recorded a row, crashes
  on today's guest (h2), and today's host replays on the 2026-11-04 guest (g2).
- **The kernel is ruled out:** 26.2.7's driver alone, on the 6.19 guest that replays with its own
  26.1.8, crashes (s1).
- The reverse swap, 26.1.8's driver on 7.3.4, fails `vkCreateInstance` with
  `VK_ERROR_OUT_OF_HOST_MEMORY` before any submit. Mesa before 26.2.6 cannot run venus on 7.2+
  kernels, so that arm says nothing about the crash.

**Open:** the Mesa commit. limina's vendor venus is built from a newer branch and does not crash, so
either upstream fixed it after 26.2.7 or our branch carries the fix. A bisect of
`mesa-26.1.8..mesa-26.2.6` for `src/virtio/vulkan` builds only the venus driver, on the 6.19 guest
with the `SWAP_LIB` arm of `probe.sh`.

**What it costs the stock tier.** A stock Fedora 44 guest runs this Mesa. vkmark and the GL rows
(zink on this venus) run, so the battery's only casualty is the distro vk-replay row; whether real
applications hit the same path is not measured.
