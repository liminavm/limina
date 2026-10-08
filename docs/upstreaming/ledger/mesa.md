# mesa (guest) — patch-audit ledger

The `limina-guest` series (`third_party/manifest.toml [mesa-guest]`, base `mesa-26.2.3`), 19
patches. Schema + protocol: `README.md`. Rows are keyed by the `limina-guest` SUBJECT; ordinals
follow `patches/mesa-guest/` and drift on re-export.

**Checked against upstream `main` b39d173ca93 (2026-10-05).** The upstreamable subset lives as
branch `upstream/guest-2026-10` in `/Volumes/mesa-cs/mesa-upstream` (local; not pushed anywhere),
rewritten to upstream form: Mesa-style message, verified `Fixes:`, `Cc: mesa-stable` where a release
carries the bug, `Signed-off-by`, no limina content. Three subjects were reworded there; the
upstream subject is in the notes. Every verdict below rests on a reproducer run on an upstream
stack — stock QEMU + virglrenderer 1.3.0 or plain zink on anv — never on limina:
`spikes/upstream-repro/<dir>/` holds the program, the before/after table and the MR draft.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | venus/wsi: linear-modifier fallback + 16F swapchain block for virtio-gpu presents | `vn_wsi.c`, `wsi_common.h`, `wsi_common_wayland.c` |  | needed | b39d173ca93 | n/a | n/a | no | standalone | guest-enhanced | carry | works around our scanout path; not a clean send (common-WSI knobs) |
| 0002 | venus/wsi: drop the 16-bit-unorm wayland swapchain format | `wsi_common_wayland.c` |  | needed | b39d173ca93 | n/a | n/a | no | standalone | guest-enhanced | carry | unconditional deletion in common code; the upstreamable shape is host-capability format filtering |
| 0003 | venus: degrade to the stub instance when ring setup fails post-connect | `vn_instance.c` |  | needed | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced (stock-tier purpose) | **upstream-now** | upstream subject "venus: fall back to the stub instance when ring setup fails". Repro: QEMU `hostmem=64K` → main loses lavapipe too (`vkCreateInstance` OOM), fix keeps llvmpipe. `4cf0989083d` removes the 16k-host trigger only on 7.2+ kernels. `venus-stub-instance/` |
| 0004 | venus: pin the ICD when creating the TLS-destructor key | `vn_common.c` |  | needed | b39d173ca93 | n/a | !44986 | no | standalone | guest-enhanced | **merged** | upstream subject "venus: don't run a thread's TLS teardown from an unloaded driver"; `Fixes: d17ddcc8477`; merged as `935c4ec39a3` with the reviewer's v2. Retire the fork's pin once the guest base carries it. An `atexit()` handler, which glibc runs at `dlclose`, marks the key invalid, frees the calling thread's TLS and deletes the tss key; other threads still holding venus TLS leak a few dozen bytes, a narrow race with a thread exiting during the last `vkDestroyInstance` remains, and unloading is unchanged. Plain-Vulkan repro: main SIGSEGV 3/3, fix clean 3/3 and the driver still unloads, and Vulkan teardown from an exit handler that runs after venus's (a C++ global's destructor) works; the `__cxa_thread_atexit_impl` hook closes the race but delays unloading, and an `RTLD_NODELETE` pin never unloads. `venus-tls-destructor/` |
| 0005 | venus: surface ring loss as VK_ERROR_DEVICE_LOST instead of abort() | `vn_common.c`, `vn_ring.c`, +4 |  | needed | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced | **RFC first** | conflicts on main (`vn_ring.c`). Upstream aborts on ring FATAL by design; `VN_DEBUG=no_abort` covers watchdog/iteration aborts only. Policy change → issue/RFC first; draft issue text in `venus-ring-loss/` (reproducer: one refused `vkGetMemoryFdPropertiesKHR` aborts the process) |
| 0006 | venus: allocate dma-buf import memory synchronously | `vn_device_memory.c` |  | needed | b39d173ca93 | n/a | n/a | no | standalone | guest-enhanced | carry (blocked) | upstream vkr fails a refused import as a CS error, not a VkResult: sync only moves the abort (main: silent success; patch: abort). Sendable once vkr returns `VK_ERROR_INVALID_EXTERNAL_HANDLE`. `venus-dmabuf-import/` |
| 0007 | zink: don't recurse forever populating a shadow attachment | `zink_render_pass.c` |  | needed | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced + host zink-on-KK | **upstream-now** | `Fixes: 82add9f2e99`. Plain zink on anv (no MSRTSS): main SIGSEGV 5/5, fix clean 5/5 with correct pixels. `zink-msrtt-shadow-recursion/` |
| 0008 | virgl: settle a CPU write into a shared resource before unmap returns | `virgl_texture.c` |  | not reproducible upstream | b39d173ca93 | n/a | n/a | no | standalone | guest-enhanced | carry | needs a venus consumer of a virgl resource, which upstream vkr cannot import; GL-to-GL is 0/20 stale on main. `virgl-shared-unmap/` |
| 0009 | virgl: report blob_mem for a resource that was already imported | `virgl_drm_winsys.c` |  | needed | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced | **held for the pair** | `Fixes: 87383e3163d`. Protocol-level repro: main sends no SET_TYPE, fix does; the image still fails on stock vrend. Sent only together with (a) Mesa: SET_TYPE carries the composite format (NV12), not plane 0's lowered R8, and (b) virglrenderer: `vrend_renderer_pipe_resource_set_type` builds per-plane `aux_plane_egl_image`s for a multi-plane import, as the GBM-allocated path does — until the image renders end to end on stock QEMU. `virgl-blob-mem-cache-hit/` |
| 0010 | virgl: let planar YUV formats be looked up in the sampler bitmask | `virgl_screen.c` |  | needed | b39d173ca93 | n/a | n/a | no | standalone | guest-enhanced | carry | pays off only against a host advertising composite formats; goes with the video protocol work (0013–0015) |
| 0011 | virgl: do not offer three-plane 4:2:0 as a decode target | `virgl_screen.c` |  | needed (code read) | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced | **upstream-now** | `Fixes: 6b5aecb1955`. Not demonstrable on QEMU (no switch enables virgl video decode); the argument is the code plus ffmpeg's exact-match selection. `virgl-decode-planar-420/` |
| 0012 | virgl: fill in the size of an exported dmabuf | `virgl_drm_winsys.c` |  | needed | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced | **upstream-now** | upstream version reworked as "virgl: report the size of an exported dmabuf": size from `lseek()` on the exported fd, since `res->size` is the request (1 for host-backed resources). main reports 0, fix 4096 = the dma-buf. The fork keeps `res->size`, which is right for its real-memory decode targets. `virgl-export-size/` |
| 0013 | virgl: give video decode targets real guest memory | `virgl_resource.c`, `virgl_video.c`, +2 |  | needed | b39d173ca93 | n/a | n/a | no | standalone | guest-enhanced | carry | fork-only cap bit (31) — needs the virglrenderer protocol upstream first |
| 0014 | virgl: allocate a decode target as one composite planar resource | `virgl_resource.c`, `virgl_video.c`, `virgl_hw.h` |  | needed | b39d173ca93 | n/a | n/a | no | standalone | guest-enhanced | carry | fork-only cap bit (30); same dependency as 0013 |
| 0015 | virgl: report a plane's offset when exporting it | `virgl_resource.c` |  | needed | b39d173ca93 | n/a | n/a | no | standalone | guest-enhanced | carry | only matters for 0014's chained planes |
| 0016 | egl/dri2: fail eglExportDMABUFImageMESA when the driver cannot export an fd | `egl_dri2.c` |  | needed | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced (generic) | **upstream-now** | `Fixes: 8f7338f284c`. Fd-table exhaustion repro: main returns EGL_TRUE with a stale fd 3/3 (virgl and zink-on-venus), fix EGL_FALSE. `egl-export-fd-failure/` |
| 0017 | vl/compositor: upload the matrix the frontend set, not the init default | `vl_compositor*.c/h` |  | needed (narrowed) | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced (generic) | **upstream-now** | `f5eb8ab7151` fixed RGB→RGB / YUV→RGB; RGB→YUV and 1-component identity still use the seed. `Fixes: f5eb8ab7151`. Stock vrend renders every conversion black (two unrelated host-side faults), so no before/after pixels on QEMU. `vl-compositor-matrix/` |
| 0018 | zink: fix lost-wakeup deadlock in the multi-context unflushed-batch wait | `zink_batch.c` |  | needed | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced + host zink-on-KK | **upstream-now** | `Fixes: d4159963e3d`, `8dd314d2035`. Breakpoint on the waiter's `mtx_lock`: main hangs 3/3 on the first hit, fix 0/3 (95k–109k hits); plain runs ~1/7. Upstream version also uses `util/timespec.h` and bounds the wait by the caller's `submit_count` (reused batch state). `zink-unflushed-wait/` |
| 0019 | venus: submit only the renderer syncs a submission filled | `vn_queue.c` |  | needed | b39d173ca93 | none-yet | none-yet | no | standalone | guest-enhanced | **upstream-now** | upstream subject "venus: submit only the renderer syncs that were filled in"; `Fixes: 6f3a570d418` (in 26.2.0–26.2.4). Pushed for the MR as `17c1e773a30` on `venus-sync-count` (`kov/mesa`). main SIGSEGV in `virtgpu_submit` 3/3, fix 3/3, re-run on main `3b1fece6ff5`. Follow-up `fbacd346dc5` (local, on the same branch): `init_pnext` dropped a waited-and-signaled semaphore's timeline value and device index before the wait while the signal list kept it — mismatched counts to the host, and with a device group the host stops processing the ring (SIGABRT 3/3 at teardown); fixed 3/3. A CTS variant of `import_signaled_temporary` that also signals would cover both. `venus-sync-count/` |

## Send queue

One MR per row, in this order — deterministic reproducers first: 0004 (merged), 0019, 0007, 0016, 0018,
0003, 0012, the sampler-swizzle fix below, 0011, 0017. 0009 waits for its Mesa +
virglrenderer follow-up pair. Ring loss (0005) goes as an issue first. Filing is the user's; the MR and issue drafts are
in each `spikes/upstream-repro/` README, and every commit is on `upstream/guest-2026-10`
(`liminavm/mesa`).

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
  main 3 host shader errors per VPP run, fix none. Not in the guest series (our host never hit it).
  `vl-compositor-sampler-swizzle/`.
- **virgl `glTexSubImage2D` from a PBO waits for the host on every call** (found by the Firefox
  perf work on an M1 limina host): the PBO read map waits on the queued transfer of the data just written.
  Candidate "virgl: don't wait for a read-only map of a clean resource" on `wip/virgl-pbo-wait`:
  1.1 → 0.18 ms per upload on stock QEMU, all tiles verified; piglit buffer/PBO/texture-transfer
  groups show no regressions. In the guest series as 0020 (payload r33). Firefox Canvas Test on
  limina: PBO path −27% vs CPU pointer on stock Mesa, −1.2% (noise) with the fix. Re-traced: no
  remaining wait goes through the PBO read map; the sub-second stalls hit every ioctl on both arms
  (host-side, in `docs/hardening-backlog.md`). **Ready to file.**
  Opting virgl into blit-based transfers also removes the waits but loses the uploads on vrend. `virgl-pbo-upload-wait/`.
- **virgl VA post-processing on vrend still draws black after that fix**: the compositor's matrix is
  a real buffer at constant slot 0 (virgl: UBO 0), which vrend never reads (`CONST[0]` is filled
  only from inline constants). Unclaimed; a virgl/vrend matter.
- **vkr fails `vkGetMemoryFdPropertiesKHR` on a non-venus resource as a CS error** — a virglrenderer
  issue (`venus-dmabuf-import/`, `--query`).

## Patches outside the guest series

These left the guest build when guest GL moved from zink to virgl, or live only on the host
`limina-kk` branch; their upstream verdicts were made against older mains and need re-checking at
MR time.

| subject | where | verdict (checked) |
|---|---|---|
| zink nullDescriptor emulation MR37115 | tombstone pool | track !37115 (not ours); retire per consumer (c9e4f184e593) |
| mesa/fbobject: guard NULL pipe_resource in do_discard_framebuffer | tombstone pool | upstream-now, 7-line NULL guard (c9e4f184e593) |
| zink: guard dmabuf semaphore import/export when external_semaphore_fd is absent | tombstone pool (two patches, one MR) | upstream-now (c9e4f184e593) |
| zink/kopper: guard surface creation on the instance surface extensions | tombstone pool | upstream-now, could join the semaphore MR (c9e4f184e593) |
| zink: re-look-up the pipeline when vertex elements change and vertex input is static | `limina-kk` | upstream-now; deterministic repro in `spikes/notification-text-corruption/` (1a4286e1abb) |

## Enhanced-tier rubric

**(a)** Every row ships only in the guest Mesa RPMs of the enhanced tier. **(b)** A stock guest
runs Fedora's Mesa; the bugs these fix are present there and degrade (a crash in one client, a
software-decode fallback, a lost Vulkan instance), none prevents boot. **(c)** Host-side
alternatives exist only for 0006/0008 (renderer behaviour) — both are carries precisely because the
fix belongs in the renderer upstream. **(d)** Each upstream-now row retires when the Fedora base
carries it; the stock tier then absorbs the fix.
