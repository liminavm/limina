# dev-enh enhanced tier — ground-truth recipe & forensic inventory

**Status:** reconciled 2026-06-24 from THREE sources: (a) a live catalog of the golden image
`Fedora-Workstation-43.dev-enh.raw` (CoW clone, SSH); (b) a transcript-archaeology agent on
**deploy + present provenance**; (c) a transcript-archaeology agent on **build provenance**.
Two early live-catalog conclusions were **WRONG** and are corrected below (see §4) — the live
catalog had inspected the wrong artifacts. Where sources disagreed, decisive **live binary
checks** (md5, pagesize) settled it.

## Why this doc exists

We have a known-working enhanced-tier image (`dev-enh`): accelerated GNOME on
zink→venus→virglrenderer(vkr)→KosmicKrisp(KK)→Metal with zero-copy IOSurface present. A
from-scratch rebuild on vanilla Fedora 43 ("the clone", `Fedora-Workstation-43.enh-build.raw`)
renders but **cannot present** — `gbm_surface_lock_front_buffer failed` every frame → black.
This doc reconstructs exactly how dev-enh was built so we can **replicate** it, and records the
divergences that remain candidate causes of the present failure.

---

## 1. The verified recipe (corrected, cross-checked)

### Kernel — **custom 16 KiB kernel, host-supplied by direct `--kernel` boot**
- dev-enh's **accelerated tier** is direct-booted: `limina … --kernel
  target/test-guest/kernel/Image-16k --cmdline "root=/dev/vda3 rootflags=subvol=root
  rootfstype=btrfs rw selinux=0 console=ttyAMA0"` (the canonical launcher is
  `spikes/venus-draw-probe/boot-seated-kk.sh`). The **kernel is never inside the disk image**;
  the disk supplies only the btrfs root.
- **16 KiB pages are load-bearing**: a 4 KiB guest cannot map venus host-visible blobs on a
  16 KiB host (`HV_BAD_ARGUMENT`). So venus accel **requires** the 16K kernel.
- Source: `torvalds/linux` tag **`v6.12`** (shallow), `arm64 defconfig` +
  `limina.fragment` (`CONFIG_ARM64_16K_PAGES=y`, virtio stack, btrfs/virtio-net builtin,
  DRM_VIRTIO_GPU, overlayfs, SELinux, PL011) + **`patches/linux/0001-0003`** (`0001` fence
  blob-scanout flushes, `0002` ARGB8888 primary plane, `0003` advertise LINEAR modifier).
  Built in a `fedora:43` Apple `container` via `scripts/build-test-kernel.sh PAGESIZE=16k`.
- The disk image *also* contains stock Fedora kernels (`6.17.1`, `6.19.13`, 4 KiB) reachable
  via EFI/GRUB **if you boot with no `--kernel`** — but that path is the **software (llvmpipe)
  baseline**, NOT the venus tier. (This is what the live catalog accidentally booted; see §4.)

### zink (`/opt/mesa-zink` megadriver) — mesa `3515c52` + **`0001–0006`**, fedora:43 host build
- **Deployed binary VERIFIED:** `/opt/mesa-zink/lib64/libgallium-26.2.0-devel.so` =
  **md5 `11acd704…`, 25281496 bytes, Jun 12 17:13**. This binary was proven (this session,
  msg 8242) **bit-for-bit identical** to mesa `3515c52` + `patches/mesa/0001–0006`, built in a
  **fedora:43 Apple `container`** (`-Dgallium-drivers=zink,llvmpipe,softpipe -Dvulkan-drivers=`
  empty `-Dplatforms=x11,wayland -Degl=enabled -Dgbm=enabled -Dgles2=enabled -Dllvm=enabled
  -Dbuildtype=release --prefix=/opt/mesa-zink`), then `scp`'d in (`tar -C /opt --zstd -x`).
- The six patches: `0001` nullDescriptor (MR!37115), `0002` NULL-guard discard framebuffer,
  `0003`/`0004` gate external-semaphore-fd on dmabuf import/export, `0005` venus present-region
  deep-copy (`vn_wsi.c` half), `0006` kopper surfaceless guard. (`0007` removed; `0008` is the
  venus dma-buf patch — §venus.)
- **fedora:44 yields a *different* binary** (`0e539673`) — the build distro is the entire byte
  difference. Reproduce in **fedora:43**.
- ⚠️ The in-guest `~/mesa` (HEAD `3515c52`, Jun 7, only nullDescriptor) and `~/build-mesa-zink.sh`
  (applies only `37115.diff`) are a **superseded experiment** — they did NOT build the deployed
  binary. Do not trust them as the recipe.

### venus (`/usr/lib64/libvulkan_virtio.so`) — mesa `26.1.0-devel`, DEBUG, in-guest, + vn-wsi-fix
- **Different mesa snapshot than zink** (venus `26.1.0-devel` vs zink `26.2.0-devel`/`3515c52`),
  built from a **separate `~/mesa-venus` tree** (an extracted tarball, **not git** → no SHA).
- **27 MB DEBUG build** (meson default buildtype; options `-Dvulkan-drivers=virtio
  -Dplatforms=wayland -Dllvm=disabled`). Built **in-guest** (Fedora 43, 4 KiB guest userland);
  deployed by in-place `cp` to `/usr/lib64/libvulkan_virtio.so`.
- Carries `vn-wsi-fix` = three edits (recovered to `patches/mesa/0008` + `0005`):
  - **A** `vn_physical_device.c`: opaque-fd branch → `renderer_handle_type = OPAQUE_FD_BIT`,
    `supported_handle_types |= OPAQUE_FD | DMA_BUF` (KK is opaque-fd-only).
  - **B** `vn_physical_device.c`: force `exts->EXT_image_drm_format_modifier = true` +
    `exts->EXT_queue_family_foreign = true`. **Intentional** — stock venus advertises neither →
    `caps.dmabuf=0` → dumb buffer → gnome-shell SIGSEGV; forcing both makes it work.
  - **C** `vn_wsi.c`: `vn_wsi_clone_present_info` deep-copies per-region `pRectangles` (kopper
    frees them on present return → use-after-free in the async present thread).
- ICD JSON `/usr/share/vulkan/icd.d/virtio_icd.aarch64.json`, selected by `VK_DRIVER_FILES`.

### virglrenderer / vkr (host `third_party/virgl-prefix`) — FORK, venus-only build
- Fork on branch `gkvm/macos-blob-map-ptr`, HEAD **`855bf70`** (Jun 11; fork base `2048dfb7`).
  Three named scanout commits: **`70c9f0c`** (zero-copy IOSurface scanout via forced-LINEAR
  images + `VK_EXT_external_memory_host` host-pointer import — the headline mechanism), **`3e3d754`**
  (cross-context dmabuf import), **`5f62d46`** (gate IFP2 synthesize to KK).
- dev-enh-era build: **venus-only, NO GL/vrend** — `-Dvenus=true -Dplatforms=[]`, render-server
  in-process thread, MoltenVK linked directly.
- ⚠️ The **current `scripts/build-virglrenderer.sh`** uses `-Dplatforms=egl
  -Drender-server-mode=thread` (the later virgl+vrend "coexist" pivot) and claims "no source
  patches" — it will **NOT reproduce the dev-enh dylib**. The on-disk `.dylib` (Jun 23) is a
  post-dev-enh rebuild. To reproduce: check out `855bf70`, build `-Dplatforms=[]` + direct MoltenVK.
- NOT in dev-enh: the flicker fix (`463057d`) and transfer_read fix (`patches/libkrun/0024`) —
  both post-dev-enh, targeting the vrend/coexist path dev-enh lacks.

### Host KosmicKrisp (KK) — mesa `178a3d7` + the foundational `0001` patch
- Base mesa **`178a3d73968`** (26.2.0-devel). KK is upstreamed into `src/kosmickrisp`, built
  natively from case-sensitive `/Volumes/mesa-cs/mesa` (`-Dvulkan-drivers=kosmickrisp
  -Dbuildtype=debug`), bundled in `limina.app`.
- **dev-enh DEPENDS on KK `patches/kosmickrisp/0001`** (~1340-line foundational delta: the
  `VK_EXT_transform_feedback` impl `kk_nir_lower_xfb.c` authored Jun-11 *specifically* so
  dev-enh's WebGL aquarium renders; external-memory opaque-fd/dma_buf; nil-residency guard;
  zink driconf). It does **NOT** need `0002`/`0003` (authored Jun-24 for the F44/mutter-50 path
  that dev-enh's mutter-49.5 never hits). ⚠️ This patch existed only as an **uncommitted working
  tree** until 2026-06-24 — same discipline gap as the venus dma-buf patch.

### mutter — 49.5 + 3 patches
- Upstream tag **49.5** (`658f672`) from `third_party/mutter`, built in-guest
  (`--prefix=/usr --libexecdir=/usr/libexec`), hand-installed (rpm-invisible). Patches:
  `0001` #32 stencil-clip degrade (the load-bearing one: zink/venus framebuffer reports
  `GL_STENCIL_BITS=0` so stencil clips no-op → must degrade multi-rect clip to bbox),
  `0002` x11 frames NULL guard + libexecdir, `0003` ext-data-control clipboard.
- **Load-path gotcha:** gnome-shell loads `libmutter-17.so.0.0.0` from `/usr/lib64/` directly,
  the rest from `/usr/lib64/mutter-17/` — install the **full cogl+clutter+mtk+mutter set from
  one build** or the fix sits inert. Use `spikes/venus-draw-probe/install-mutter-fix.sh`.

### Env / autologin / boot
- Env via `~/.config/systemd/user/org.gnome.Shell@wayland.service.d/*.conf` (load-bearing) +
  `~/.config/environment.d/zink.conf`: `LD_LIBRARY_PATH`/`LIBGL_DRIVERS_PATH`/`__EGL_VENDOR_LIBRARY_DIRS`
  → `/opt/mesa-zink`, `MESA_LOADER_DRIVER_OVERRIDE=zink`, `GALLIUM_DRIVER=zink`,
  `VK_DRIVER_FILES=…virtio_icd.aarch64.json`, `VN_PERF=no_semaphore_feedback,no_fence_feedback,no_event_feedback,no_query_feedback`,
  `LD_PRELOAD=/home/claude/abrtcatch.so` (debug shim). No env forces scanout linear — vkr does.
- gdm autologin `claude`, GNOME Wayland.
- dev-enh is a **developer/debug image** (debug venus, LD_PRELOAD shim, leftover source trees).

---

## 2. Present / scanout path
dev-enh runs the **venus zero-copy present**: `SET_SCANOUT_BLOB … (zero-copy)`. The host
scanout buffer is **LINEAR**, forced in vkr (`70c9f0c`): vkr forces scanout images
`VK_IMAGE_TILING_LINEAR`, strips `INPUT_ATTACHMENT`, forces dedicated alloc, and
host-pointer-imports the **IOSurface base address** (`VK_EXT_external_memory_host`) → GPU
renders into the IOSurface → Core Animation zero-copy. A present race (CA gets the IOSurface at
mutter flush, before the GPU write lands) is mitigated host-side by `GKVM_PRESENT_COPY`
(default-ON, 3-deep IOSurface ring). **This host mechanism is identical on the rebuild — not
the differentiator.** Guest virtio-gpu KMS reports `DRM_CAP_ADDFB2_MODIFIERS = 0`.

---

## 3. Divergences: dev-enh (works) vs the clone (black) — CORRECTED

| Axis | dev-enh (works) | clone (black) | differentiator? |
|---|---|---|---|
| zink | `3515c52` + `0001–0006`, **fedora:43** host build (`11acd704`) | `0001–0006` (build distro TBD — confirm md5) | **likely NOT** (verify clone built fedora:43, not :44) |
| venus | `26.1.0-devel` **debug**, in-guest | `26.2.0-devel` **release** (per earlier note) | **candidate** |
| kernel | custom **16K** `Image-16k` (`v6.12`+`patches/linux`) | custom **16K** overlayfs rebuild | **candidate** (different 16K builds) |
| boot harness | `boot-seated-kk.sh` (specific env/flags) | plain `limina --window` | **candidate** |
| mutter | 49.5 + 3 patches | 49.5 + 3 patches | no (same) |
| KK | `178a3d7` + `0001` | `178a3d7` + `0001`(+`0002`/`0003`) | no (clone is superset) |
| vkr/host scanout | identical bundle | identical | no |

### RESOLVED 2026-06-24 by a controlled 3-run A/B (all via `boot-seated-kk.sh`, same host stack):

| Run | disk (venus) | kernel | gbm lock | verdict |
|---|---|---|---|---|
| 1 | dev-enh (27M debug `26.1`) | original `Image-16k` | **0** (pixel-confirmed desktop) | baseline reference |
| 2 | dev-enh (27M debug `26.1`) | my overlayfs `Image-16k` | **0** | **kernel ruled out** |
| 3 | clone (2.1M `26.2`/`3515c52`) | my overlayfs `Image-16k` | **20** (black) | **harness ruled out** |

Run 2 vs Run 3 differ in **only the disk** (same kernel, same harness, same host KK/vkr). zink
on the clone is **byte-identical** (`11acd704`); env/mutter equivalent. **The lone divergence is
the venus build: clone `26.2.0-devel`(`3515c52`) release/2.1M vs dev-enh `26.1.0-devel`
debug/27M.** ⇒ **the present failure is caused by the venus build** — the 26.1→26.2 version bump
(WSI/dma-buf/modifier path) and/or debug-vs-release. Boot harness, kernel, zink, env, mutter all
eliminated.

**Confirmed by binary swap (2026-06-24):** dropping dev-enh's 27M `26.1.0-devel` venus onto the
broken clone disk (nothing else changed) → gbm lock 0, **desktop renders (pixel-confirmed)**.
The venus binary is the entire difference.

### Refinement — the regression predates the 26.1.0 *release* (2026-06-24)
A from-source build of the **`mesa-26.1.0` release tag** (`1d7b6318b9`) + our venus patches
(`0005`+`0008`, both apply cleanly; built `scripts/build-venus.sh`, fedora:43, 27M debug,
dev-enh's exact meson options) **FAILS** the same way (window stuck on boot console, gbm lock
failures ongoing). So:

| venus | result |
|---|---|
| dev-enh `26.1.0-devel` (pre-branch main snapshot) | **works** |
| `mesa-26.1.0` release (`1d7b6318b9`, branch pt + rc) + our patches | **fails** |
| `26.2.0-devel` (`3515c52`) + our patches | **fails** |

⇒ The breaking venus change landed **before the 26.1.0 release**, between dev-enh's earlier
devel snapshot and the release tag. **Pinning to the 26.1 tag is insufficient.** Our patches are
NOT the cause (they apply cleanly and dev-enh carries the same ones). It's an upstream venus
WSI/external-memory/dma-buf change.

### ROOT CAUSE (2026-06-24): a LOST venus WSI patch, not a version regression
Diffing dev-enh's exact `~/mesa-venus` source (preserved at `spikes/venus-261-source/`) against
`mesa-26.1.0` revealed that dev-enh carries a **4-file WSI present fix that was never exported**
to `patches/mesa/` — the same discipline gap as KK `0001` and venus `0008`. Our exported venus
patches (`0005` present-region deep-copy, `0008` dma-buf advertise) are real but **not** the
present fix. The actual fix:
- **`src/vulkan/wsi/wsi_common.h`** — new `wsi_device` fields `treat_invalid_modifier_as_linear`,
  `block_16f_swapchain_formats`.
- **`src/vulkan/wsi/wsi_common_wayland.c`** — in `wsi_wl_display_add_drm_format_modifier`, rewrite
  `DRM_FORMAT_MOD_INVALID` → `DRM_FORMAT_MOD_LINEAR` (macOS-host virtio-gpu advertises every
  modifier as INVALID; without this mesa takes the prime-blit fallback that breaks IOSurface
  zero-copy) + drop 16F/10:10:10:2 formats.
- **`src/virtio/vulkan/vn_wsi.c`** — set those flags; translate `DRM_FORMAT_MODIFIER` swapchain
  images → `OPTIMAL` and strip the modifier pNext (renderer returns `memoryRequirements.size=0`
  for modifier images → trips `wsi_create_native_image_mem` → `gbm_surface_lock_front_buffer
  failed`). Plus the `0005` present-region deep-copy (already exported).
- **`src/virtio/vulkan/vn_image.c`** — modifier plane-count fix (return 1 for LINEAR instead of
  asserting) + inert `if(0)` debug traces.

This is why every clean rebuild failed at `gbm_surface_lock_front_buffer`, on **both** 26.1.0 and
26.2.0 — they were missing the WSI fix, not suffering a version regression. The fix is
version-independent.

### RESOLVED & REPRODUCED FROM SOURCE (2026-06-24)
The complete present fix turned out to be **6 limina-edited venus files** (the rest of the
dev-enh↔release diff is pure upstream drift, functionally irrelevant — confirmed by a
6-file-subset build that presents with 0 gbm failures). Captured as two patches over
`mesa-26.1.0`, built by `scripts/build-venus.sh`:
- **`patches/mesa/0009-venus-wsi-present-fix.diff`** — `vn_wsi.c` + `wsi_common.h` +
  `wsi_common_wayland.c` (treat-invalid-modifier-as-linear + DRM_FORMAT_MODIFIER→OPTIMAL
  swapchain xlate + present-region deep-copy; supersedes old `0005`).
- **`patches/mesa/0010-venus-image-physdev-native-modifier.diff`** — `vn_physical_device.c`
  (**native LINEAR-modifier reporting** — the load-bearing piece `0009` alone lacked — +
  dma_buf-on-opaque-fd, supersedes old `0008`) + `vn_image.c`/`.h` modifier handling.

`0005`/`0008` retired (subsumed). The accelerated GNOME desktop renders from a clean from-patches
build (pixel-confirmed). dev-enh's exact venus+WSI sources preserved under
`spikes/venus-261-source/`.

**Remaining polish (not blocking):** `0010` carries dev-enh's `26.1.0-devel` versions of its 3
files (limina edits + incidental upstream drift) because the exact dev-enh base commit wasn't
pinned (its blobs match multiple branches). A follow-up can pin that base and distil `0010` to a
minimal upstream-clean diff, and port `0009`/`0010` onto current mesa (26.2) so venus rides the
same tree as zink. The build is **pinned to `mesa-26.1.0`** for now, which is faithful to dev-enh
(it ran venus 26.1 + zink 26.2 mixed).

---

## 4. What the live catalog got WRONG (and why) — do not repeat
- **"dev-enh zink = only 0001."** FALSE. The live catalog read the **stale Jun-7 `~/mesa`
  experiment tree** + `build-mesa-zink.sh`. The **deployed** binary is `11acd704` =
  `0001–0006` (md5-verified live). Lesson: inspect the *deployed* artifact, not leftover source.
- **"dev-enh presents on stock 4K EFI / overturns the 16K requirement."** FALSE. The live
  catalog booted the clone with **no `--kernel`** → the 4K **llvmpipe software** greeter, whose
  "gbm lock: 0 failures" is the *software* present, not venus. venus accel needs 16K
  (`HV_BAD_ARGUMENT` on a 16K host otherwise). Lesson: confirm the renderer (venus vs llvmpipe)
  before reading a present result as "the venus tier works."
- **Both transcript agents' "mutter 49.5 vs 50.0 is the differentiator."** Still rejected for
  *our* case: the clone is mutter-49.5 too. (mutter-50 has its own modifier-RT crash, separate.)
- **"dev-enh works with no KK fixes."** Literally FALSE — it needs KK `0001` (XFB etc.). True
  part: it needs none of the recent `0002`/`0003`.

---

## 5. Replication blockers (must be read off live binaries; not in transcripts)
1. Guest venus exact mesa commit/SHA — tarball, not git. Only `26.1.0-devel` known.
2. Guest venus md5 / exact bytes — no md5 recorded for the working 27 MB debug build.
3. Kernel exact commit + `uname -r` + toolchain — only floating `v6.12` tag pinned.
4. Host virglrenderer dep versions; the dev-enh-era dylib was overwritten (rebuild from `855bf70`).
5. dev-enh-era KK dylib byte-identity — no snapshot preserved (`0001` uncommitted until Jun-24).
6. Build page size for the zink (container) and venus (in-guest) builds — unstated.
7. venus `-Dplatforms` exact value — `wayland` vs `x11,wayland` unresolved.

## 6. Proposed controlled replication (NOT yet executed — awaiting go-ahead)
1. **First, re-observe the baseline:** boot dev-enh via `boot-seated-kk.sh` (16K) and confirm
   venus-accelerated present works (the apples-to-apples reference we never re-took this session).
2. Reproduce on a clean F43 clone: zink `3515c52`+`0001–0006` (**fedora:43** container, expect
   `11acd704`); venus from `~/mesa-venus` (mesa `26.1.0-devel`, debug) + vn-wsi-fix; mutter 49.5
   + 3 patches; KK `0001`; **16K `Image-16k` kernel**; the env drop-in; gdm autologin; boot via
   `boot-seated-kk.sh`.
3. Bisect the remaining suspects (venus build, kernel, boot harness) against the dev-enh reference.
