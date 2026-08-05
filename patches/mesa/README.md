# patches/mesa — limina enhanced-tier GUEST Mesa patches (venus; zink rows retired)

The limina enhanced tier ships a **patched guest Mesa**. Since the 2026-08-04
drop-guest-zink decision (`docs/design/gl-path-vrend-vs-zink.md`) the *living* patches are
all **venus** (the guest Vulkan driver — guest GL rides virgl/vrend, unpatched); the zink
rows below are retained as documented dead weight until the next respin physically drops
them. Source clone + build happen in the Apple `container` Linux build env (host APFS is
case-insensitive and can't check out mesa). The host-side Mesa work (KosmicKrisp + host
zink) lives on the mesa fork's `limina-kk` branch (fork model — `patches/kosmickrisp/` is
retired).

> **⚠ This is a patch POOL, not a single-base series** (unlike `patches/libkrun`/
> `patches/virglrenderer`): raw context diffs, three different effective bases, and three
> consumers applying different subsets. That shape is documented debt — flagged in the
> 2026-07-01 review (`docs/reviews/2026-07-01-full-review.md` Part III) — to be resolved
> when the patches go to Mesa upstream (each becomes an MR and the pool shrinks). Until
> then, THIS file is the map; keep it current when adding/retiring a diff.

## Bases × consumers (the map)

| Consumer | Base | Applies |
|---|---|---|
| ~~`scripts/build-mesa-zink.sh`~~ **ARCHIVED 2026-08-04** (`scripts/archive/`) | ~~Mesa main `3515c52e8cf3`~~ | ~~0001–0006~~ — the prefix-style zink build died with the RPM pivot + drop-guest-zink; 0002–0006 now have NO live consumer |
| `scripts/build-venus.sh` (guest venus ICD rebuild) | tag `mesa-26.1.0` (parametric `MESA_TAG`) | 0009 + 0010 *(dev vehicle, not respun — see 0010 retirement below)* |
| `scripts/provision/f44/build-mesa-rpm.sh` (the SHIPPING F44 RPM, currently 26.1.5-6.limina) | Fedora F44 SRPM, repo-current (26.1.4 at authoring, **26.1.5 since the -6 respin**) | 0001 + 0015 + 0011–0014 + 0016 + 0017 *(0010 retired 2026-08-04; 0001 + 0014 retire at the NEXT respin — drop-guest-zink)* |
| `scripts/build-mesa-rpm.sh` (the SHIPPING F43 RPM, 26.1.5-1.limina since the 2026-08-05 repoint) | **the F44 koji SRPM `mesa-26.1.5-1.fc44`** (pinned as `MESA_SRPM_URL`), built in the fc43 container against F43's repos | 0015 + 0011–0013 + 0016 + 0017 — the venus-only set, identical to F44's next-respin set *(0001 + 0009 + 0010 + 0014 + 0016-pre all left this build with the repoint)* |

**Drop-guest-zink (2026-08-04) — status of the zink rows.** zink-as-guest-GL is no longer a
supported configuration, so **0001, 0003, 0004, 0006 are DEAD in the guest** (0003–0006
already stopped shipping when the prefix build died; 0001 still rides both RPMs and comes
out at each family's next respin). **0014's guest trigger is gone** (no zink in any
supported guest config) but it stays temporarily since it's harmless and load-bearing
HISTORY for the host build (applied there as fork commit `47308c0f026`); drop from the
specs at the next respins. **0002 stays live**: core GL-frontend guard, reachable under
virgl too. None of this changes upstream-MR worthiness — 0014 (both bugs), 0013, 0012,
0002, and 0003+0004(+0006 as "gate optional entrypoints") remain the send queue
(`docs/upstreaming/ledger/mesa.md`).

**0010 RETIRED from the shipping F44 RPM (2026-08-04).** Both halves are dead against the current
host stack: (a) vkr advertises `VK_EXT_external_memory_dma_buf` itself (virgl `f2f038a3`) so
upstream venus's own dma-buf branch fires; (b) KosmicKrisp natively implements
`VK_EXT_image_drm_format_modifier` (LINEAR-only) + `EXT_queue_family_foreign` and vkr passes
modifier creates through verbatim (mesa-fork `befa0f2731e`/`d918b98d869`, virgl `0cc513fd`), so
upstream venus's passthrough gate advertises the extension and the host answers the layout
queries truthfully — where 0010(b) fabricated tight-packed pitches that were wrong whenever
`width*4` missed Metal's 16-byte row alignment. An enhanced guest by definition runs against our
host, so no compatibility window exists; a *stale-host* mix is absorbed by KK's graceful
explicit-pitch handling (`[KK-MODIFIER]` log). Full story:
`docs/design/drm-format-modifier-for-real.md`, `spikes/modifier-necessity/RESULTS.md`. The file
stays in the pool for the F43 script until its respin.

### ~~BACKLOG~~ EXECUTED 2026-08-05: the F43 RPM is repointed at the F44 SRPM base

**Done exactly as planned below.** `scripts/build-mesa-rpm.sh` rewritten to the F44 SRPM
recipe (koji `mesa-26.1.5-1.fc44` pinned via `MESA_SRPM_URL`), still built in the fc43
container against F43's repos (the old-toolchain stress the track exists for — builddep
resolved clean with the Rust drivers %undefined). Both families now build the SAME base with
the SAME venus-only patch set; the `3515c52` main-snapshot pin and the git-archive machinery
are gone. **0016-pre is deleted from the pool** (it IS upstream; zero consumers remain).
0009 + 0010 remain as files only for `scripts/build-venus.sh` (a stale dev vehicle — archive
candidate) and 0001/0014 only for the F44 spec until its next respin. The version moved
26.2.0-3.limina → 26.1.5-1.limina — a deliberate one-time DOWNGRADE; `install-enhanced.sh`
grew a `dnf downgrade` branch for it (validated live on `Fedora-Workstation-43.enhanced.raw`:
downgrade transaction + re-versionlock + venus enumerating 26.1.5 in the rebooted seated
session, human-eyeballed desktop; `.test` recloned 2026-08-05). This was the precondition for
the limina-guest fork migration (task #11 in-session). Original plan kept for the record:

F43 exists **on purpose** — it is the "how well does the enhanced tier support an older distro"
track, and the intent is to carry ~3 Fedora releases (F43 drops when F45 joins). But building it
from a pinned Mesa *main* snapshot defeats that: it routes around the friction the track exists to
expose, and it makes F43 ship a **different patch subset** than F44 (no 0011/0012/0013 — including
0012, the venus stub-instance degrade that guards the stock-4k GRUB fallback) plus `0016-pre`,
a backport whose only job is to make an old base accept our patches. So F43 currently tests "a June
main snapshot on an older distro", not "our enhanced tier on an older distro".

Fix: point `scripts/build-mesa-rpm.sh` at the **same 26.1.4 SRPM base F44 uses** (via the existing
`MESA_SRPM_URL` pin) instead of `MESA_COMMIT=3515c52e8cf3`. That keeps every old-distro stress that
matters (GNOME 49 vs 50, older kernel, older glibc/llvm/libdrm at build time, the older
mutter/shell ABI) while collapsing this table from three bases to two and giving both families the
identical patch set: **0009 and 0016-pre retire**, F43 gains 0015 + 0011–0014.

Deliberately NOT "rebuild F43 from its own 25.2.4 SRPM" (the F44 method applied literally): that
stresses "do our patches apply to an old mesa", an axis we never ship on — given the choice we
would not deliver a 25.2.4-based mesa to an F43 guest when a 26.1.4-based one swaps in cleanly and
is versionlocked. The soname-swap machinery in `build-mesa-rpm.sh` already handles 25.x→26.x, so
this is a base change, not a new mechanism.

Main unknown: mesa 26.1.4 has to *build* in an F43 container — old llvm/rust/libdrm could bite.
That is itself an old-version signal worth having explicitly. Validation pass = build → installer
pass over `Fedora-Workstation-43.enhanced.raw` → venus live in the seated session → reclone
`.test`. (2026-08-04: retiring the F43 RPM base now DOES free the `3515c52e8cf3` pin —
`build-mesa-zink.sh`, its former co-owner, is archived.) The repoint also collapses the
respin-time retirement list in one motion: 0001 + 0009 + 0010 + 0014 + 0016-pre all leave
the F43 build, and both families converge on the venus-only set — the precondition for the
limina-guest fork migration (task #24).

**0009 vs 0015 — same fix, two bases.** Fedora's 26.1.4 stable backported an equivalent of
0009's `vn_wsi_clone_present_info` rectangle deep-copy, so 0009 no longer applies there.
`0015-venus-wsi-present-fix-post-rect-clone.diff` is 0009 minus those three hunks — use it on
bases ≥ 26.1.4-stable; keep 0009 for bases without the backport (26.1.0…26.1.3, and 26.2.0,
which branched before it). Never apply both.

gitlab.freedesktop.org is Anubis-bot-blocked → builds clone a GitHub mirror.

**Duplicate encodings of 0010 — RETIRED 2026-08-04.** `venus-dmabuf-patch.py` (the
anchored-string re-encoding of 0010's physdev edits) and
`spikes/venus-draw-probe/patch-venus-dmabuf-nomod.py` (the F44 no-modifier variant) were
kept "until the F44 scanout story is settled" — it settled: the host advertises the
modifier ext for real and 0010 itself is retired from the shipping RPM. Both .py files
deleted; 0010.diff remains only for the F43 script until its respin.

## Patches (apply in filename order)
- **`0001-zink-nullDescriptor-emulation-MR37115.diff`** — **DEAD in guest (drop-guest-zink 2026-08-04; still applied by both RPMs until their next respins).** Mesa **MR !37115**: zink
  nullDescriptor *emulation* (dummy descriptors) so zink runs on Vulkan implementations
  that lack `robustness2.nullDescriptor` — which MoltenVK does. Without it zink bails
  (`Zink requires the nullDescriptor feature of KHR/EXT robustness2`) → llvmpipe. Clean (no
  LIMINA-DIAG debug hacks). Not in any Mesa release/main — we must carry it until it lands.
- **`0002-fbobject-guard-null-pipe-resource-in-discard.diff`** — `do_discard_framebuffer()`
  dereferences `att->Renderbuffer->texture` (a `pipe_resource`) without a NULL check. A
  gbm/kopper winsys back buffer can be `Complete` yet have no `pipe_resource` while the
  swapchain rotates it, so `glInvalidateFramebuffer(GL_BACK)` crashed gnome-shell at swap
  on the mutter native/KMS path. Guard `prsc` for NULL. Upstreamable (#30 seated, wall 1).
- **`0003-zink-guard-missing-external-semaphore-fd-on-dmabuf-import.diff`** — **DEAD in guest (no live consumer since the prefix build died; MR-worthy).** and
  **`0004-...-on-dmabuf-export.diff`** — `zink_screen_import/export_dmabuf_semaphore()`
  call `VKSCR(GetSemaphoreFdKHR)` / `VKSCR(ImportSemaphoreFdKHR)` whenever built with
  libdrm on Linux, without checking `VK_KHR_external_semaphore_fd` is supported. venus over
  MoltenVK has no fd-based external semaphores → those entrypoints are NULL → swap-buffers
  jumps through a NULL function pointer. Gate both on
  `screen->info.have_KHR_external_semaphore_fd` (the flag zink already uses for dmabuf /
  cl_gl_sharing elsewhere); fall back to implicit dmabuf sync. Upstreamable (#30 seated,
  walls 2–3). Together 0002+0003+0004 make seated gnome-shell *survive* on venus (no more
  crash-loop); the remaining blocker is KMS-CRTC scanout-format negotiation, not a crash.
- **`0006-zink-kopper-guard-missing-surface-extensions.diff`** — **DEAD in guest (same).** kopper guards for the
  surfaceless/no-WSI path on KK.
- **`0007`** — *removed 2026-06-24.* It was a backport already upstream in 26.x; on the
  newer tree the fuzzy `patch -F5` fallback silently re-applied it at +347 lines, **duplicating**
  `get_external_image_handle_type_props` → `zink_resource.c` "redefinition" build failure. It is
  obsolete (upstream) so it was dropped rather than rebased.
## Venus present fix (`0009` + `0010`) — built against `mesa-26.1.0` via `scripts/build-venus.sh`

These two **reproduce the working accelerated-tier venus** that the golden
`Fedora-Workstation-43.dev-enh.raw` ran. Root-caused 2026-06-24 (see `docs/dev-enh-recipe.md`):
the present fix was a **lost limina patch set that had only ever lived in dev-enh's in-guest
`~/mesa-venus` tree** — never exported, the same discipline gap as KK `0001`. A clean rebuild
at *any* mesa version (26.1.0 release, 26.2) failed identically at `gbm_surface_lock_front_buffer`
until these were recovered; it was **never a version regression**. They supersede the old
`0005` (present-region; now folded into `0009`) and `0008` (dma-buf; now folded into `0010`),
both retired. dev-enh's base was pinned by multi-file blob fingerprint to **`mesa-26.1.0`** (the
26.1 release branch; the "26.1.0-devel" VERSION string was a red herring — main had bumped to
26.2.0-devel at the April branch-point). The patched files are **byte-identical to `mesa-26.1.0`
except our edits**, so these diffs are **pure limina, no upstream drift**. Inert `if (0) fprintf`
debug traces and a dead `tiling_translated_to_optimal` flag were stripped (DIAG hygiene, below).

- **`0009-venus-wsi-present-fix.diff`** (`vn_wsi.c`, `src/vulkan/wsi/wsi_common.h`,
  `wsi_common_wayland.c`) — the WSI half:
  - `treat_invalid_modifier_as_linear`: macOS-host virtio-gpu advertises every DRM modifier as
    `INVALID`; rewriting those to `LINEAR` keeps mesa on the IOSurface single-memory path
    instead of the prime-blit fallback that breaks zero-copy.
  - `vn_wsi_create_image`: translate `DRM_FORMAT_MODIFIER` swapchain images → `OPTIMAL` and
    strip the modifier pNext, because the in-process renderer returns
    `memoryRequirements.size = 0` for modifier images → trips `wsi_create_native_image_mem` →
    `gbm_surface_lock_front_buffer failed`.
  - the `VkPresentRegionKHR::pRectangles` deep-copy (old `0005`).
- **`0010-venus-image-physdev-native-modifier.diff`** (`vn_physical_device.c`, `vn_image.c`,
  `vn_image.h`) — the device half:
  - **native DRM-format-modifier reporting**: venus itself reports `DRM_FORMAT_MOD_LINEAR`
    (features from OPTIMAL tiling) so kopper's Wayland path can negotiate a modifier at all —
    without it swapchain creation fails. (This is the piece beyond `0008` that `0009` alone
    lacked.)
  - dma_buf-on-opaque-fd: the `else if` keying off `KHR_external_memory_fd` →
    `renderer_handle_type = OPAQUE_FD` + force-enable `EXT_image_drm_format_modifier` /
    `EXT_queue_family_foreign` (old `0008`). Without it stock venus reports `caps.dmabuf=0` →
    gbm dumb-buffer fallback → NULL `bo->image` → gnome-shell SIGSEGV.
  - `vn_image` modifier plane-count + tiling-translation handling.

`scripts/build-venus.sh` (parametric `MESA_TAG`, default `mesa-26.1.0`; `FEDORA_REL` must match
the guest) applies `0009`+`0010` and emits `libvulkan_virtio.so` + the ICD. dev-enh's exact
venus+WSI sources are preserved under `spikes/venus-261-source/`. The host KosmicKrisp +
host-zink series lives separately in `patches/kosmickrisp/`.

- **`0011-venus-wsi-drop-16bit-unorm-swapchain.diff`** (2026-06-30, ships in the F44 RPM as
  mesa 26.1.3-2.limina) — venus/Wayland WSI: drop 16-bit-unorm swapchain formats
  (`R16G16B16A16_UNORM` etc.), matching lavapipe. venus offered a wgpu-unrenderable
  `Rgba16Unorm` Wayland swapchain format, producing the "ghost UI" in wgpu apps. NOT a KK
  gap (see the corrected story in the `limina-kk-feature-gaps` memory). Upstreamable with
  the lavapipe precedent as the argument.
- **`0012-venus-degrade-to-stub-instance-when-ring-setup-fails.diff`** (2026-07-01, the
  two-tier Vulkan-floor fix) — venus: when the renderer connects but the instance ring /
  version handshake fails (on limina: a 4 KiB-page guest under the 16 KiB host can't map
  the ring's 132 KiB shmem blob), degrade to the existing STUB instance (0 devices) instead
  of returning `VK_ERROR_OUT_OF_HOST_MEMORY` — which the loader treats as fatal for the
  WHOLE `vkCreateInstance`, killing lavapipe with it. Root-caused + validated RED→GREEN on
  a stock F44 guest 2026-07-01 (venus+lavapipe → llvmpipe enumerates, no error). Protects
  the enhanced image's *stock-kernel GRUB-fallback boot* today; truly-stock guests get it
  when it lands upstream (strong candidate — the stub path already exists for version
  mismatches). Applied by `build-venus.sh` and the F44 RPM build.
- **`0013-venus-pin-icd-for-tls-destructor.diff`** (2026-07-02, thread-exit crash fix) —
  venus registers a C11/pthread TLS key whose destructor `vn_tls_free` lives in the ICD,
  but TLS-key destructors (unlike `__cxa_thread_atexit_impl`) do not pin their DSO; when
  the Vulkan loader `dlclose()`s the driver after the last instance is destroyed, any
  thread that ever used venus SIGSEGVs in `__nptl_deallocate_tsd` on exit. Fix: after
  creating the key, re-open ourselves with `RTLD_NODELETE` (dladdr on `vn_tls_free`) so
  the destructor stays mapped. Deterministic repro + full story:
  `spikes/egl-tsd-repro/` (surfaceless EGL init + `eglTerminate` on a worker thread —
  the shape of niri's headless `egl_*` tests). Upstream `main` still has the bug
  (checked 2026-07-02). Clearly upstreamable.
- **`0014-zink-fix-unflushed-batch-wait-lost-wakeup.diff`** (2026-07-12, suite-wedge fix; **guest trigger GONE since drop-guest-zink — retire from the specs at next respins; lives on host-side as fork commit `47308c0f026`, still an upstream-now MR**) —
  zink: `zink_batch_usage_unflushed_wait()`'s multi-context branch checked `u->unflushed`
  outside `u->mtx` and then `cnd_wait`ed with no re-check/loop, while `submit_queue`
  cleared the flag and broadcast without the mutex — a textbook lost wakeup. Bit as a
  100-minute `venus_replay` hang (eglretrace frozen in `glReadPixels`); live-confirmed by
  reading `unflushed == false` out of `/proc/<pid>/mem` while the thread slept, and by
  resuming the process with a hand-delivered `pthread_cond_broadcast` from gdb. Also fixes
  the `trywait` path's absolute-vs-relative `cnd_timedwait` timespec (always-expired).
  Full forensics: `spikes/venus-replay-zink-hang-2026-07-12/RESULTS.md`. Upstream `main`
  still has the bug (checked 2026-07-12). Clearly upstreamable. NOTE: the same code ships
  in the HOST zink-on-KK GL build (`/Volumes/mesa-cs`) — apply there on the next host mesa
  refresh.
- **`0015-venus-wsi-present-fix-post-rect-clone.diff`** (2026-07-20, base catch-up) — the
  0009 present-fix re-cut for bases that already carry the upstreamed
  `vn_wsi_clone_present_info` rectangle deep-copy (Fedora mesa ≥ 26.1.4 stable): 0009 minus
  those three hunks, keeping the vn_wsi_init modifier/format flags, the create-image
  DRM_MOD→OPTIMAL translation, and the wsi_common/wayland plumbing. See "0009 vs 0015" in
  the map above; validated on the F44 26.1.4-1.fc44 SRPM (PREP + full RPM build + venus ICD
  in the payload). One of 0009/0015 per base, never both.
- **`0016-venus-ring-loss-device-lost-not-abort.diff`** (2026-07-20, resume-crash hardening) —
  venus: surface ring loss as `VK_ERROR_DEVICE_LOST` instead of `abort()`. A dead venus ring
  (host sets `VK_RING_STATUS_FATAL_BIT_MESA` — e.g. a snapshot-restore replay gap, the
  2026-07-20 vkmark-on-resume SIGABRT) used to abort() the process at its next submit
  (`vn_ring_submit_internal`), relax-wait (`vn_relax`), or feedback probe (ffb/sfb/qfb).
  Now: `vn_relax` returns false on ring FATAL (callers stop waiting), the ring submit/wait
  paths return DEVICE_LOST (submission bookkeeping unwound, reply decoder NULLed so
  generated `vn_call_*` fail cleanly), and the three feedback probes propagate instead of
  aborting. The watchdog/iter aborts (renderer HANG detection) are deliberately unchanged.
  Authored on the mesa-cs tree (26.2-era); applies to the F44 26.1.4 SRPM base directly, and
  to the F43 26.2.0 snapshot after `0016-pre` (see below) — wired into both RPM builds
  (F43 since the 26.2.0-3.limina respin). Upstream mesa still aborts (checked 2026-07-20).
  Clearly upstreamable —
  companion to the host-side virglrenderer 0040 create-arg closure
  (`spikes/m9-vkmark-resume-crash/RESULTS.md`).
- **`0017-venus-fix-ring-submit-freelist-capacity.diff`** (2026-07-21, perf; **superseded
  upstream** 2026-07-27 by !43229's bounded cache — but the 26.1.5-6 respin proved Fedora's
  26.1.5 tarball does NOT carry it yet: 0017 still applied fail-loud clean. Keep until a
  base bump actually contains !43229, then drop; no MR — see `limina-venus-submit-freelist`)
  — venus: fix the
  ring-submit free-list degeneration. `vn_ring_get_submit` matched recycled nodes on
  `submit->shmem_count >= wanted`, but the caller (`vn_ring_submission_get_ring_submit`)
  overwrites that field with the count USED this time — a node last used for a direct
  (0-shmem) submission is recorded 0 forever, its real ≥2-slot capacity lost. The free list
  (never pruned before ring destruction) fills with recorded-0 nodes; every ≥1-shmem
  submission walks the whole list, misses, and mallocs a node that later joins the same
  list → unbounded growth + O(n) walk per submission = quadratic CPU creep in any
  long-running venus app. Live evidence (blobs WebGL demo, 2026-07-21): perf-annotate put
  88% of the zink-flush-queue thread on the list-walk load; at constant workload over
  ~10 min firefox grew 19.6→29.3% CPU, worker 75→83%, RES +25MB, while the demo's JIT
  stayed flat. Fix: track the allocated slot count in a new `shmem_capacity` field, set once
  at malloc; match the scan on it. Wired into the F44 script AND the F43 script (since the
  26.2.0-3.limina respin; F43 needs `0016-pre` first — the F43 base predates the free-list
  scan). Upstream mesa main still has the bug (checked 38169ede9b2, 2026-07-21). Clearly
  upstreamable. Memory/forensics: `limina-venus-submit-freelist`.
- **`0016-pre-venus-ring-get-submit-freelist-scan-backport.diff`** — **DELETED 2026-08-05
  (F43 repoint)**: its one job was making the old F43 main-snapshot base accept 0016/0017;
  the repointed base (26.1.4+ stable) carries the free-list scan already, and the diff was
  verbatim upstream (never MR material). Entry kept for the record: — verbatim
  upstream `2cf1f6cb508` "venus: fix unbound malloc leak in vn_ring_get_submits" (Yiwei
  Zhang, landed on main 2026-06 AFTER our F43 `MESA_COMMIT` pin 3515c52; its stable
  backport `d54c04f96c2` is already in the F44 26.1.4 base — never apply it there). It
  replaces vn_ring_get_submit's pop-first-node-if-small shape with the free-list capacity
  scan: the code 0016's vn_ring.c hunk anchors on and 0017 fixes (the scan as landed
  matches on the caller-overwritten `shmem_count` — the very bug 0017 addresses). Chain is
  exact: this diff's post-image blob (`ff72774d243`) is 0016/0017's pre-image. F43
  (`scripts/build-mesa-rpm.sh`) only; NOT for upstreaming (it IS upstream).

## Re-export / DIAG hygiene
The in-guest working tree carried temporary `LIMINA-DIAG` debug hacks (force
`driver_name_is_inferred=false`, `mesa_loge(__LINE__)` markers) — those are **debug only and
must never be committed here**. Only clean, upstreamable patches belong in this series.
