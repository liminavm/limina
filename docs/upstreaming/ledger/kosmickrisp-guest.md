<!-- ledger-check: dep=kosmickrisp-guest -->
# kosmickrisp-guest — upstreaming ledger

`liminavm/mesa` `metal-native-context` branch (`third_party/manifest.toml [kosmickrisp-guest]`),
pin `044be6b8db0`, base `059715d2aa5` — the `[kosmickrisp]` (`limina-kk`) pin. The series is the
host KosmicKrisp branch plus guest-only commits: the KosmicKrisp driver on the limina
native-context transport (the limina bridge) inside the guest, the guest WSI, and the venus,
virgl, zink and VA fixes the guest needs. A vendor build (`-Dlimina-vendor`) drives every
virtio-gpu: zink on KosmicKrisp for a limina transport, virgl for any other, venus beside them.
Rows here cover only `base..rev`; the `limina-kk` commits underneath are in `kosmickrisp.md`.
Schema + protocol: `README.md`. Rows are keyed by SUBJECT.

Not vendored by `cargo xtask vendor`, for the same reason as `[kosmickrisp]` (Mesa needs a
case-sensitive filesystem): the working tree is `/Volumes/mesa-cs/mesa-metal-guest`, built by
`scripts/build-gpu-drivers.sh` and installed into a guest by `guest/gpu-drivers/install.sh` under
`/opt/limina`. There is no committed export; builds read the tree.

Disclosure is not gated for KosmicKrisp (user decision, 2026-10-21; `README.md`, phase 3); a
`sec` note here records the bug class only.

**`ord` is a label, not a position.** Rows carrying a four-digit label are the fixes that came
from the former guest-RPM series; the label is what the notes below cite. Rows with `—` came
from the bridge/vendor work. Match a row to the branch by subject.

**Verdicts.** `need` rows with a SHA in `checked` were made against that upstream `main` and have
not been re-checked against this base; `unchecked` rows are first guesses from the commit
messages and a read of the diffs. One spot check: upstream `main` `d44453ce07b` (2026-10-14) still
prints MSL floats with `"%.*le"` (`src/kosmickrisp/compiler/msl_type_inference.c:778`), the bug
`41353793c95` fixes.

The upstreamable subset lives as branch `upstream/guest-2026-10` in `liminavm/mesa` (on `main`
`b39d173ca93`), rewritten to upstream form: Mesa-style message, verified `Fixes:`, `Cc:
mesa-stable` where a release carries the bug, no limina content. Three subjects were reworded
there; the upstream subject is in the notes. Commits for rows here: `5e812ddb0c0` stub-instance
fallback (0003), `fc7366ac84c` blob_mem (0009, held), `14084ecd72d` three-plane 4:2:0 (0011),
`5bed42b1b2b` EGL fd export (0016), `128d6465c8c` vl/compositor matrix (0017), `685704f8781`
dmabuf size (0012), `f8a85712230` vl/compositor sampler swizzle (not on this branch). The PBO-wait
fix (0020) is on its own branch, `upstream/virgl-pbo-wait` (`224228b878c`). The zink lost-wakeup
fix is not carried: upstream now broadcasts under the usage mutex and re-checks in a loop. Every
verdict rests on a reproducer run on an upstream stack — stock QEMU + virglrenderer 1.3.0 or
plain zink on anv — never on limina: `spikes/upstream-repro/<dir>/` holds the program, the
before/after table and the MR draft.

| ord | subject | files | diag | need | checked | issue | mr | test | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| — | kk/limina: stop texture_transfer deadlocking when staging grows | `lmn_bridge.h`, `lmn_device.c`, `lmn_memory.c` |  | unchecked | — | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | the limina bridge exists only in this fork. sec: guest-side self-deadlock |
| — | kk: put LINEAR on the dma-bufs a limina-bridge swapchain shares | `kk_wsi.c` |  | unchecked | — | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | gated on a bridge that can share textures; a generic KK `supports_modifiers` opt-in could go upstream later |
| — | wsi/wayland: skip feedback tranches with no modifier the driver can use | `wsi_common_wayland.c` |  | unchecked | — | none-yet | none-yet | to-decide | no | standalone | guest-enhanced (generic) | upstream-after-cleanup | common WSI code, any driver facing a modifier-less KMS scanout tranche (virtio-gpu); needs a repro with a non-KK driver |
| — | kk/limina: let a signal behind a queue wait be exported without blocking | `lmn_queue.c` |  | unchecked | — | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | bridge queue code; check whether upstream KK's `kk_binary_export_sync_file` has the same `vk_sync_wait(INFINITE)` fallback |
| — | kk/limina: ask what the host speaks before creating a context | `lmn_device.c`, `lmn_probe.h`, `virtgpu_limina_drm.h` +1 |  | unchecked | — | n/a | n/a | n/a | **review** | standalone | guest-enhanced | carry | depends on the fork kernel's `DRM_VIRTGPU_LIMINA_INFO` uapi. sec: an ioctl in the driver number range issued against render nodes, mitigated by checking the node is virtio-gpu first |
| — | limina: a vendor build that installs beside the system Mesa | `meson.build`, `meson.options`, `platform_drm.c` +5 |  | unchecked | — | n/a | n/a | n/a | low | standalone | guest-enhanced | carry | limina packaging (`-Dlimina-vendor`), with a **carve-out**: the option-independent `platform_drm` hunk that checks a GBM device's backend name before reading it as a `gbm_dri_device` fixes an in-process type confusion and could go upstream alone. sec: no trust boundary |
| — | limina: a vendor build serves no client that names other drivers | `eglglvnd.c`, `zink_screen.c`, `gbm_dri.c` +2 |  | unchecked | — | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | vendor-build device policy; `limina: a vendor build drives every virtio-gpu` replaces its decline-everything rule |
| — | limina: a vendor build drives every virtio-gpu | `loader.c`, `pipe_loader_drm.c`, `vl_winsys_drm.c` +7 |  | unchecked | — | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | vendor-build device policy: the DRI loader answers zink for a limina transport and virgl for any other virtio-gpu; EGL/GBM take every virtio-gpu; video opens virgl everywhere; venus marks its driverInfo "(limina)". The `pipe_loader_drm_probe_fd_driver` hunk (name the driver instead of asking the loader) is generic |
| — | kk: print MSL float literals with a '.' whatever the locale | `msl_float.c`, `msl_float.h`, `meson.build` +2 |  | needed | d44453ce07b | none-yet | none-yet | ships its own test | no | standalone | host | upstream-now | generic KK bug still on upstream `main` (above) |
| — | kk/limina: sync the wire header to version 5 and the fence UAPI | `limina_gpu_wire.h`, `virtgpu_limina_drm.h` |  | unchecked | — | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | syncs the limina wire header to VERSION 5 (bridge protocol) |
| — | zink: bound the buffer views a resource object caches | `zink_batch.c`, `zink_context.c`, `zink_resource.c` +2 |  | needed | d44453ce07b | none-yet | none-yet | `spikes/zink-tbo-churn/` (run it on plain zink first) | low | twin of `limina-kk` `c40298fd0cc` | guest-enhanced | upstream-after-cleanup | generic zink: the per-object buffer-view cache had no bound (Firefox's WebRender grew it to ~100k views a day). The same commit as the host row in `kosmickrisp.md`, one MR. sec: a guest app grows its own driver's objects, which the transport backs with host textures |
| 0001 | venus/wsi: linear-modifier fallback + 16F swapchain block for virtio-gpu presents | `vn_wsi.c`, `wsi_common.h`, `wsi_common_wayland.c` |  | needed | b39d173ca93 | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | works around our scanout path; not a clean send (common-WSI knobs) |
| 0002 | venus/wsi: drop the 16-bit-unorm wayland swapchain format | `wsi_common_wayland.c` |  | needed | b39d173ca93 | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | unconditional deletion in common code; the upstreamable shape is host-capability format filtering |
| 0003 | venus: degrade to the stub instance when ring setup fails post-connect | `vn_instance.c` |  | needed | b39d173ca93 | none-yet | none-yet | none-fits (the host must fail ring setup) | no | standalone | guest-enhanced (stock-tier purpose) | **upstream-now** | upstream subject "venus: fall back to the stub instance when ring setup fails". Repro: QEMU `hostmem=64K` → main loses lavapipe too (`vkCreateInstance` OOM), fix keeps llvmpipe. `4cf0989083d` removes the 16k-host trigger only on 7.2+ kernels. `venus-stub-instance/` |
| 0005 | venus: surface ring loss as VK_ERROR_DEVICE_LOST instead of abort() | `vn_common.c`, `vn_ring.c`, +4 |  | needed | b39d173ca93 | none-yet | none-yet | to-decide | no | standalone | guest-enhanced | **RFC first** | conflicts on main (`vn_ring.c`). Upstream aborts on ring FATAL by design; `VN_DEBUG=no_abort` covers watchdog/iteration aborts only. Policy change → issue/RFC first; draft issue text in `venus-ring-loss/` (reproducer: one refused `vkGetMemoryFdPropertiesKHR` aborts the process) |
| 0006 | venus: allocate dma-buf import memory synchronously | `vn_device_memory.c` |  | needed | b39d173ca93 | n/a | n/a | n/a | no | standalone | guest-enhanced | carry (blocked) | upstream vkr fails a refused import as a CS error, not a VkResult: sync only moves the abort (main: silent success; patch: abort). Sendable once vkr returns `VK_ERROR_INVALID_EXTERNAL_HANDLE`. `venus-dmabuf-import/` |
| — | venus: match the X server's DRM device by its node, not only by PCI | `vn_wsi.c` |  | unchecked | — | none-yet | none-yet | to-decide | no | standalone | guest-enhanced (generic) | upstream-now | `wsi_device_matches_drm_fd` compares PCI bus info only, so any non-PCI virtio-gpu (virtio-mmio on arm64 VMMs) takes the X11 prime-blit path; needs an upstream-stack repro and a message in the submitter's own words |
| — | venus: a limina vendor build advertises VK_EXT_memory_budget | `vn_physical_device.c` |  | unchecked | — | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | upstream gates the extension behind `VN_DEBUG=mem_budget`; limina's host answers from its ledger (the cap the guest is held to), so the vendor build turns it on by default |
| — | zink: keep every transient a shadow call populated valid | `zink_render_pass.c` |  | needed | 7d6e9088821 | none-yet | none-yet | piglit: `kov/piglit` `msrtt2-depth-partial-clear` | no | standalone series of 3 | guest-enhanced + host zink-on-KK | **upstream-now** | follow-up MR after !45485 (the recursion fix, `Fixes: 82add9f2e99`, which the base carries). `Fixes: 635f0c5f4fe`, Cc: mesa-stable. Branch `zink-msrtt-shadow` on `kov/mesa`: renderpass atom kept dirty while `zink_batch_rp()` runs (`Fixes: 769b356b5b6`, main only), the recursion fix, this commit, and the `glFlush` route below. Plain zink on anv (no MSRTSS): main SIGSEGV in all seven modes but `zs`; the recursion fix alone leaves `mrt`/`mrt3` stale; with all four, 5/5 correct in every mode and piglit fbo/clear/sRGB/multisample/texture-view groups match main. `zink-msrtt-shadow-recursion/`. Commit message must be the author's own words before posting (Mesa AI policy) |
| — | zink: populate shadow attachments before reducing the fb to flush clears | `zink_context.c` |  | needed | 7d6e9088821 | none-yet | none-yet | piglit: as above | no | standalone series of 3 | guest-enhanced + host zink-on-KK | **upstream-now** | the `glFlush` route the recursion fix makes reachable; same MR and same note on the commit message; goes to stable with the others |
| — | zink: survive the descriptor pools a lost device cannot create | `zink_descriptors.c`, `zink_draw.cpp` |  | needed | d44453ce07b | none-yet | none-yet | `spikes/timer-query-hang` (host) | guest-DoS | standalone | guest-enhanced + host zink-on-KK | as `kosmickrisp.md` | cherry-picked from `limina-kk` `161be8ef694`; its row and verdict live in `kosmickrisp.md` |
| — | zink: reset the context whose work hung the GPU, not the device | `zink_batch.c`, `zink_context.c`, `zink_screen.c` +1 |  | n/a | — | n/a | n/a | piglit `ext_timer_query@time-elapsed` (host) | guest-DoS | standalone | host zink-on-KK | carry | cherry-picked from `limina-kk` `3d7a8fe4244`. Inert in this build until the guest KosmicKrisp exposes `vkGetCommandPoolHangCountLIMINA` (host `909b3baeae8`); zink on venus never sees the entry point either |
| 0008 | virgl: settle a CPU write into a shared resource before unmap returns | `virgl_texture.c` |  | not reproducible upstream | b39d173ca93 | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | needs a venus consumer of a virgl resource, which upstream vkr cannot import; GL-to-GL is 0/20 stale on main. `virgl-shared-unmap/` |
| 0009 | virgl: report blob_mem for a resource that was already imported | `virgl_drm_winsys.c` |  | needed | b39d173ca93 | none-yet | none-yet | to-decide | no | standalone | guest-enhanced | **held for the pair** | `Fixes: 87383e3163d`. Protocol-level repro: main sends no SET_TYPE, fix does; the image still fails on stock vrend. Sent only together with (a) Mesa: SET_TYPE carries the composite format (NV12), not plane 0's lowered R8, and (b) virglrenderer: `vrend_renderer_pipe_resource_set_type` builds per-plane `aux_plane_egl_image`s for a multi-plane import, as the GBM-allocated path does — until the image renders end to end on stock QEMU. `virgl-blob-mem-cache-hit/` |
| 0010 | virgl: let planar YUV formats be looked up in the sampler bitmask | `virgl_screen.c` |  | needed | b39d173ca93 | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | pays off only against a host advertising composite formats; goes with the video protocol work (0013–0015) |
| 0011 | virgl: do not offer three-plane 4:2:0 as a decode target | `virgl_screen.c` |  | needed (code read) | b39d173ca93 | none-yet | none-yet | none-fits (no stock host reaches it) | no | standalone | guest-enhanced | **upstream-now** | `Fixes: 6b5aecb1955`. Not demonstrable on QEMU (no switch enables virgl video decode); the argument is the code plus ffmpeg's exact-match selection. `virgl-decode-planar-420/` |
| 0012 | virgl: fill in the size of an exported dmabuf | `virgl_drm_winsys.c` |  | needed | b39d173ca93 | none-yet | none-yet | to-decide | no | standalone | guest-enhanced | **upstream-now** | upstream version reworked as "virgl: report the size of an exported dmabuf": size from `lseek()` on the exported fd, since `res->size` is the request (1 for host-backed resources). main reports 0, fix 4096 = the dma-buf. The fork keeps `res->size`, which is right for its real-memory decode targets. `virgl-export-size/` |
| 0013 | virgl: give video decode targets real guest memory | `virgl_resource.c`, `virgl_video.c`, +2 |  | needed | b39d173ca93 | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | fork-only cap bit (31) — needs the virglrenderer protocol upstream first |
| 0014 | virgl: allocate a decode target as one composite planar resource | `virgl_resource.c`, `virgl_video.c`, `virgl_hw.h` |  | needed | b39d173ca93 | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | fork-only cap bit (30); same dependency as 0013 |
| 0015 | virgl: report a plane's offset when exporting it | `virgl_resource.c` |  | needed | b39d173ca93 | n/a | n/a | n/a | no | standalone | guest-enhanced | carry | only matters for 0014's chained planes; upstream has since added GBM-layout STRIDE/OFFSET answers, merged with the chain walk by the next row |
| — | virgl: answer per-plane queries from the chain and the GBM layout both | `virgl_resource.c` |  | unchecked | — | n/a | n/a | n/a | no | fold-into:virgl: report a plane's offset when exporting it | guest-enhanced | carry | upstream answers STRIDE/OFFSET from the resource's GBM layout, indexed by plane; ours steps along the chain to the plane's own resource (how a decode target's planes are found). Each covers what the other misses: chain first, then that resource's GBM layout when it has one, its metadata otherwise |
| 0016 | egl/dri2: fail eglExportDMABUFImageMESA when the driver cannot export an fd | `egl_dri2.c` |  | needed | b39d173ca93 | none-yet | none-yet | to-decide | no | standalone | guest-enhanced (generic) | **upstream-now** | `Fixes: 8f7338f284c`. Fd-table exhaustion repro: main returns EGL_TRUE with a stale fd 3/3 (virgl and zink-on-venus), fix EGL_FALSE. `egl-export-fd-failure/` |
| 0017 | vl/compositor: upload the matrix the frontend set, not the init default | `vl_compositor*.c/h` |  | needed (narrowed) | b39d173ca93 | none-yet | none-yet | to-decide | no | standalone | guest-enhanced (generic) | **upstream-now** | `f5eb8ab7151` fixed RGB→RGB / YUV→RGB; RGB→YUV and 1-component identity still use the seed. `Fixes: f5eb8ab7151`. Stock vrend renders every conversion black (two unrelated host-side faults), so no before/after pixels on QEMU. `vl-compositor-matrix/` |
| 0020 | virgl: don't wait for a read-only map of a clean resource | `virgl_resource.c` |  | needed | 5d4c4bd553a | n/a | none-yet | to-decide | no | standalone | guest-enhanced (generic) | **upstream-now** | `glTexSubImage2D` from a PBO waits for the host on every call: with no blit-based transfers the PBO is mapped for reading on the CPU while the transfer of the data just written is queued. Skips the wait for a read-only map that needs no readback (every host-side write marks the resource dirty); blob resources still wait. Stock QEMU: 1.1 → 0.18 ms per upload, the same as client memory, all tiles verified; piglit buffer/PBO/texture-transfer groups no regressions. Firefox Canvas Test on limina: PBO path −27% vs CPU pointer on stock Mesa, −1.2% (noise) with the fix. Opting virgl into blit-based transfers also removes the waits but loses the uploads on vrend. Upstream form on `wip/virgl-pbo-wait`, not on `upstream/guest-2026-10`. `virgl-pbo-upload-wait/` |
| — | venus: don't dereference a missing bo when reporting a failed allocation | `vn_device_memory.c` |  | needed | 059715d2aa5 | none-yet | none-yet | to-decide | no | standalone | guest-enhanced (generic) | **upstream-now** | regression from upstream `78f9b71dd7c`, which moved the `memory_reports` early return into `vk_device_memory_report_emit()`: `vn_device_memory_emit_report()` now reads `mem->base_bo->res_id` for a failed import or exportable allocation, whose `base_bo` is NULL, so a refused dma-buf import segfaults `vkAllocateMemory` instead of returning `VK_ERROR_INVALID_EXTERNAL_HANDLE`. Any venus guest with a host that refuses an import hits it; on limina `refused_venus_import_leaves_the_context_alive` (`vkudmabufimport.py Venus forcealloc`) |

## Send queue

One MR per row, in this order — deterministic reproducers first: the two zink shadow commits
(follow-up to !45485), 0016, 0003, 0012, the sampler-swizzle fix below, 0011, 0017, 0020. 0009
waits for its Mesa + virglrenderer follow-up pair. Ring loss (0005) goes as an issue first.
Filing is the user's; the MR and issue drafts are in each `spikes/upstream-repro/` README. Every
commit except 0020 is on `upstream/guest-2026-10`; 0020 is on `upstream/virgl-pbo-wait`.

**Mesa's AI policy governs every send** (`docs/submittingpatches.rst`, "Expectations on
contributors"; the text rule since MR !43990, 2026-08-25):
- Code made with AI help needs a disclosure trailer: `Assisted-by: TOOL (MODEL)`, or
  `Generated-by:` when the AI wrote almost all of it. `Co-authored-by` is reserved for humans.
- Commit messages, code comments, MR descriptions and GitLab comments must be the submitter's own
  words, not AI-generated. The commit messages, comments and drafts on `upstream/guest-2026-10`
  were written with Claude, so they are research notes: rewrite them before sending.
- No autonomous tool may submit or touch issues or MRs. `Signed-off-by` is optional.

## Found along the way (not in the series)

- **VA post-processing on vrend is broken since 26.2.0** (`210e557f7e0`): a compositor shader emits
  `SAMP[0].wwww`, which vrend cannot translate, so the context dies. Fixed on the upstream branch as
  "vl/compositor: don't swizzle the sampler operand of the alpha fetch" (`Fixes: 210e557f7e0`):
  main 3 host shader errors per VPP run, fix none. Not on this branch (our host never hit it).
  `vl-compositor-sampler-swizzle/`.
- **virgl VA post-processing on vrend still draws black after that fix**: the compositor's matrix is
  a real buffer at constant slot 0 (virgl: UBO 0), which vrend never reads (`CONST[0]` is filled
  only from inline constants). Unclaimed; a virgl/vrend matter.
- **vkr fails `vkGetMemoryFdPropertiesKHR` on a non-venus resource as a CS error** — a virglrenderer
  issue (`venus-dmabuf-import/`, `--query`).

## Patches outside this branch

These live in the tombstone pool or only on the host `limina-kk` branch; their upstream verdicts
were made against older mains and need re-checking at MR time.

| subject | where | verdict (checked) |
|---|---|---|
| zink nullDescriptor emulation MR37115 | tombstone pool | track !37115 (not ours); retire per consumer (c9e4f184e593) |
| mesa/fbobject: guard NULL pipe_resource in do_discard_framebuffer | tombstone pool | upstream-now, 7-line NULL guard (c9e4f184e593) |
| zink: guard dmabuf semaphore import/export when external_semaphore_fd is absent | tombstone pool (two patches, one MR) | upstream-now (c9e4f184e593) |
| zink/kopper: guard surface creation on the instance surface extensions | tombstone pool | upstream-now, could join the semaphore MR (c9e4f184e593) |
| zink: re-look-up the pipeline when vertex elements change and vertex input is static | local `kk-hardening` only, not on `limina-kk` | upstream-now; deterministic repro in `spikes/notification-text-corruption/` (1a4286e1abb) |

## Enhanced-tier rubric

**(a)** Every guest-venus/virgl/zink row ships only in the vendor drivers of the enhanced tier
(`guest/gpu-drivers`, installed under `/opt/limina`). **(b)** A stock guest runs Fedora's Mesa; the
bugs these fix are present there and degrade (a crash in one client, a software-decode fallback, a
lost Vulkan instance), none prevents boot. **(c)** Host-side alternatives exist only for 0006/0008
(renderer behaviour) — both are carries precisely because the fix belongs in the renderer
upstream. **(d)** Each upstream-now row retires when the Fedora base carries it; the stock tier
then absorbs the fix.
