# mesa — patch-audit ledger

17 patches; `UPSTREAM_BASE` `floating — see the series README`. Schema + protocol: `README.md`.
Rows are keyed by SUBJECT; ordinals are informational and drift on re-export.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | zink nullDescriptor emulation MR37115 | `docs/drivers/zink.rst`, `src/gallium/drivers/zink/zink_context.c`, `src/gallium/drivers/zink/zink_context.h` +4 |  | needed — backport of icenowy's MR !37115, still OPEN (stalled ~5 months, 43 notes), in no release | main branch-tip `c9e4f184e593` 2026-08-03 | #15649 (open, same ask, PowerVR) + #14228 (dozen) | !37115 (not ours — track it); rejected lookalike !34934 (zmike dummy-surface draft, closed); competing mechanism !37659 (common NIR lowering, open) | no | standalone | guest-enhanced | carry + track !37115; retire per consumer when base ≥ its merge release | PROBE ITEM: KK now advertises robustness2 upstream (!41313) — 0001's trigger may be moot at runtime on current hosts; a supportive venus/KK test report on !37115 could help it land |
| 0002 | mesa/fbobject: guard NULL pipe_resource in do_discard_framebuffer | `src/mesa/main/fbobject.c` |  | needed — main fbobject.c:5647-5650 still derefs `att->Renderbuffer->texture` unguarded | main `c9e4f184e593` 2026-08-03 | none-yet | none-yet | no | standalone | guest-enhanced (consumed only by build-mesa-zink.sh, not the shipping RPMs) | **upstream-now** — 7-line NULL guard; precedent bails in the same function (!2071, !15308) | no venus/macOS specificity at all |
| 0003 | zink: guard dmabuf semaphore import when external_semaphore_fd is absent | `src/gallium/drivers/zink/zink_screen.c` |  | needed — main zink_screen.c:2543 still calls GetSemaphoreFdKHR unconditionally; caller in zink_synchronization.cpp:478 also unguarded | main `c9e4f184e593` 2026-08-03 | none-yet | none-yet | no | fold-into:0004 (one MR: same file, same flag, import+export halves) | guest-enhanced | **upstream-now** (as the 0004 MR) | `have_KHR_external_semaphore_fd` gates caps but not these call sites — generic to any driver lacking the ext; defensive not load-bearing for limina today (shipping RPMs omit these) |
| 0004 | zink: guard dmabuf semaphore export when external_semaphore_fd is absent | `src/gallium/drivers/zink/zink_screen.c` |  | needed — main zink_screen.c:2487-2500 unconditional export path remains | main `c9e4f184e593` 2026-08-03 | none-yet | none-yet | no | standalone (carrier of the 0003 fold) | guest-enhanced | **upstream-now** | caller already tolerates VK_NULL_HANDLE — upstream-friendly shape; MR honesty note: original trigger (venus-on-MoltenVK) is retired, bug is generic |
| 0006 | zink/kopper: guard surface creation on the instance surface extensions | `src/gallium/drivers/zink/zink_kopper.c` |  | needed — main zink_kopper.c:103/111 call Create*SurfaceKHR gated only by compile-time platform defines | main `c9e4f184e593` 2026-08-03 | none-yet | none-yet | no | standalone; could join the 0003/0004 MR series ("zink: gate optional-extension entrypoints") | guest-enhanced | **upstream-now** | adjacent open !33060 (Metal WSI for kopper) is context, not overlap |
| 0009 | venus wsi present fix | `src/virtio/vulkan/vn_wsi.c`, `src/vulkan/wsi/wsi_common.h`, `src/vulkan/wsi/wsi_common_wayland.c` |  | needed on pre-backport bases only (26.1.0–26.1.3, 26.2.0; consumer = build-venus.sh @ 26.1.0); rect-clone hunks superseded | main raw files 2026-08-03 | n/a | superseded-in-part by !42528 (merged 2026-07-01: 311a437c deep copy + 1dbb4d0d NULL-pRegions skip) | no | rest is byte-identical to 0015 (the fold target) | guest-enhanced | carry short-term; **retires entirely with the F43/base repoint** (README backlog) | GAP: 0009 lacks 1dbb4d0d's NULL-pRegions skip — if it lives on, cherry the one-liner in |
| 0010 | venus image physdev native modifier | `src/virtio/vulkan/vn_image.c`, `src/virtio/vulkan/vn_physical_device.c` |  | **RETIRED WHOLE 2026-08-04 — dropped from the F44 spec.** (a) dead once vkr advertises `VK_EXT_external_memory_dma_buf` (virgl f2f038a3; upstream's own branch fires, proven by reading handleTypes back as DMA_BUF). (b) dead once the renderer advertises the modifier ext for real: KK implements `VK_EXT_image_drm_format_modifier` LINEAR-only + `EXT_queue_family_foreign` (mesa-fork `befa0f2731e`/`d918b98d869`) and vkr passes modifier creates through verbatim (virgl `0cc513fd`) — upstream venus's passthrough gate lights up and the host answers layout queries TRUTHFULLY, where (b) fabricated tight-packed pitches (wrong whenever width*4 misses the 16-byte alignment; KK absorbs the stale lie gracefully during any mixed-version window, `[KK-MODIFIER]` log). The durable fix the old verdict asked for ("implement the ext for real in virgl/KK") is what shipped. See `docs/design/drm-format-modifier-for-real.md` + `spikes/modifier-necessity/RESULTS.md`. | main vn_physical_device.c/vn_image.c 2026-08-03 | none | none | n/a — retired | .py re-encodings retire with it | guest-enhanced | **retired** — the enhanced guest requires the current host (which every enhanced guest runs against by definition) | the "honest lie" is over: the renderer negotiates for real |
| 0011 | venus wsi drop 16bit unorm swapchain | `src/vulkan/wsi/wsi_common_wayland.c` |  | needed — main wsi_common_wayland.c still exposes R16G16B16A16_UNORM (608-615) | main 2026-08-03 | none | none | no | standalone | guest-enhanced | carry; upstream = a conversation, not a clean send — unconditional deletion in COMMON code; the upstreamable reshape is host-capability-dependent format filtering (bigger design) | root cause is a wgpu client-side limitation (Rgba16Unorm legal but not wgpu-color-renderable); "matches lavapipe" UNVERIFIED; re-probe whether the high-bit stride bug still exists on current KK — may retire the block_16f half independently |
| 0012 | venus degrade to stub instance when ring setup fails | `src/virtio/vulkan/vn_instance.c` |  | needed — main vn_instance.c:318-324 unchanged: ring-setup failure still hard-errors the whole vkCreateInstance (kills lavapipe too) | main 2026-08-03 | none-yet | none-yet; precedent = !34613 (graceful-decline, merged 2025-04) + the in-tree version-mismatch stub path | no (fail-safe direction) | standalone | guest-enhanced delivery, **STOCK-tier purpose** — protects the stock-kernel GRUB-fallback boot; truly-stock guests only benefit when it lands upstream | **upstream-now** — generic loader-semantics bug, small, precedent-backed; the diff's own prose is MR-ready; no Fixes: target (cite !34613) | limina's original trigger (16k/4k ring mmap) is host-fixed since (libkrun 0043 trio) — defense-in-depth for us, real protection for stock |
| 0013 | venus pin icd for tls destructor | `src/virtio/vulkan/vn_common.c` |  | needed — main vn_tls_key_create_once (vn_common.c:377-385) still unpinned; no RTLD_NODELETE anywhere in the file | main 2026-08-03 | class precedent: #13571 (sysprof TLS-dtor segfault, closed) + !36978 (closed linker-flag attempt) | none-yet — **ACTION: file the MR** | no (pin = RTLD_NOLOAD|NODELETE self-reopen) | standalone | guest-enhanced — but the bug bites ANY venus user on any host | **upstream-now — the cluster's best candidate**: 14-line self-contained fix, deterministic repro (spikes/egl-tsd-repro), upstream already met the class | anticipated pushback: "-Wl,-z,nodelete on the ICD" (!36978's shape) — code pin only activates when the key exists; add Fixes: at the vn_tls_key introduction |
| 0014 | zink: fix lost-wakeup deadlock in the multi-context unflushed-batch wait | `src/gallium/drivers/zink/zink_batch.c` |  | needed — bug byte-for-byte on main: unlocked unflushed=false (:657), un-mutexed cnd_broadcast (:845), no-recheck no-loop wait + epoch-absolute {0,10000} timespec (:1210-1216, so trywait has silently NEVER waited upstream) | main `c9e4f184e593` 2026-08-03; only post-base touch = !37835 (ALWAYS_INLINE removal, no supersession) | none-yet — **ACTION: file issue + MR** | none-yet | no | standalone | guest-enhanced + **host zink-on-KK (HOST GAP CLOSED 2026-08-04: applied as `47308c0f026` on limina-kk, pinned in third_party/manifest.toml)**; carried without a reachability proof on purpose — a race cannot be disproven by not observing it (see docs/design/gl-path-vrend-vs-zink.md) | **upstream-now — flagship of the series**: canonical condvar fix + live /proc/mem+gdb forensic proof + a second self-evident bug in the same diff; zero limina-specific content | forensics = spikes/venus-replay-zink-hang-2026-07-12/RESULTS.md |
| 0015 | venus wsi present fix post rect clone | `src/virtio/vulkan/vn_wsi.c`, `src/vulkan/wsi/wsi_common.h`, `src/vulkan/wsi/wsi_common_wayland.c` |  | needed — this IS the live residual (F44 RPM); every hunk absent from main | main 2026-08-03 | n/a | n/a | no | fold target of 0009; never apply both (README rule confirmed) | guest-enhanced | carry — SLIMMED 2026-08-04: the vn_wsi_create_image DRM_MOD→OPTIMAL rewrite hunk is DELETED (premise dead — KK implements the modifier ext for real now, and the hunk actively SIGSEGVed wsi_create_native_image_mem once 0010's fabricated query stopped masking it; RPM 26.1.5-6.limina). Residual = vn_wsi_init flags + wsi_common(.h)/wayland plumbing (treat_invalid_modifier_as_linear, block_16f); shrinks further with the M15 device-advertised story | stale "IOSurface→ANGLE" comments predate Metal scanout; block_16f is self-labeled diagnostic — re-probe on current KK |
| 0016 | pre venus ring get submit freelist scan backport | `src/virtio/vulkan/vn_ring.c` |  | mechanical — verbatim upstream 2cf1f6cb508 backported so the F43 pinned-main base accepts 0016/0017 anchors | 2026-08-03 | n/a | n/a — it IS upstream | no | chained under 0017 | guest-enhanced (F43 build only) | retire-on-rebase — upstream has since REVERTED this very shape (09fb7ca8 undoes the scan): a future rebase drops 0016-pre AND 0017 together and takes 09fb7ca8 | also retires when the F43 RPM repoints at the F44 SRPM base (README backlog) |
| 0016 | venus ring loss device lost not abort | `src/virtio/vulkan/vn_common.c`, `src/virtio/vulkan/vn_common.h`, `src/virtio/vulkan/vn_query_pool.c` +3 |  | needed — main still aborts on ring FATAL (vn_common.c:270, vn_ring.c:466, qfb vn_query_pool.c:340); our ffb/sfb hunks target code DELETED upstream (fence-feedback deprecated 2026-07-20, 4c1938c8) | main 2026-08-03 | n/a | WATCH !42501 (open third-party no-abort-on-ring-fatal via VN_DEBUG=no_abort — real demand, stalled; notes 401-gated) | no | standalone | guest-enhanced | carry; upstream attempt = medium effort — re-cut against the reworked sync code, propose default DEVICE_LOST or extend no_abort to ring-FATAL, coordinate with !42501 | M9 resume-hardening motivation ("ring loss is a legitimate runtime event") is the novel argument; companion virgl 0040 |
| 0017 | venus fix ring submit freelist capacity | `src/virtio/vulkan/vn_ring.c` |  | **still needed on the shipping base** — upstream main has 09fb7ca8d824 (MR !43229, merged 2026-07-27, Fixes: 2cf1f6cb; different, better mechanism: bounded cache at retire, O(1) pop, no walk), but the `mesa-26.1.5` tag = the Fedora SRPM base does NOT contain it (verified 2026-08-05: tag code still matches on caller-overwritten shmem_count, no capacity field, `git merge-base --is-ancestor` NO; the 08-03 note "stable pick 9b3c5935 → Fedora 26.1.5" was WRONG — that sha is not reachable from the tag). Twice proven live: 0017 applied clean at the 26.1.5-6 respin | mesa-26.1.5 tag directly, 2026-08-05 | n/a | !43229 (theirs, merged); rejected lookalike !41904 | no | standalone | guest-enhanced | carry (fork commit `limina-guest` 0006) until the base tarball contains 09fb7ca8 — **no MR to file**; the %prep apply-failure at a future SRPM bump IS the retirement signal | CLOSE the limina-venus-submit-freelist "OPEN: upstream MR" item |

## Findings

### Series verdict (all 15 rows researched 2026-08-03, vs main `c9e4f184e593`)

This series holds the audit's richest MR queue — **six upstream-now candidates**:
0014 (zink lost-wakeup deadlock, byte-for-byte on main + a second self-evident bug:
the trywait timespec is epoch-absolute so it has silently never waited), 0013 (venus
ICD TLS-destructor pin, deterministic repro, upstream already met the class in
#13571), 0012 (stub-instance degrade — enhanced delivery, STOCK-tier purpose),
0002 (fbobject NULL guard), and 0003+0004 as one MR (+0006 could join as "gate
optional-extension entrypoints"). Two supersessions close open memory items: 0017's
freelist fix landed upstream 07-27 by a better mechanism (bounded cache — on main only;
the 26.1.5 base does NOT have it, see the corrected row), and 0009's rect-clone hunks
landed 07-01 (!42528). The venus WSI
residual (0015 + 0010 + 0011) is limina-shaped — every hunk works around the
renderer lacking real modifier support — and shrinks toward zero with the M15
device-advertised story, not with MRs.

Work items surfaced: cherry 1dbb4d0d's NULL-pRegions skip into 0009 if it lives on;
scope 0010's unconditional GetImageSubresourceLayout override to WSI/modifier images
(it dead-codes the AHB path); re-probe the block_16f high-bit diagnostic on current KK;
401-gated MR notes (!37115 stall reason, !42501 pushback) need a logged-in browser pass at MR time.

**Resolved 2026-08-04** (docs/design/gl-path-vrend-vs-zink.md): 0014 is applied to the host
zink-on-KK build (`47308c0f026`). The "does KK's robustness2 (!41313) moot 0001" probe is
**answered for the host**: KK advertises `.nullDescriptor = true` + `EXT_robustness2`/`KHR_robustness2`
(`kk_physical_device.c:146,182,358`), so 0001's trigger is absent host-side — it does not need to
follow guest GL to the host. Same for 0003/0004 (KK advertises `.KHR_external_semaphore_fd = true`,
`:195`) and 0006 (host zink is surfaceless; the `Create*SurfaceKHR` calls sit behind
XCB/Wayland/Win32 ifdefs). These four retire with the guest zink deployment rather than relocating.
Note this settles the *host* question only — while guest zink still ships, the guest-side verdicts
in the table above stand unchanged.

**Drop-guest-zink EXECUTED (2026-08-04, later the same day):** zink-as-guest-GL is no longer a
supported configuration — `install-enhanced.sh` selects virgl for GL (venus stays the Vulkan
side), both F44 enhanced images repassed, both tiers smoke-tested. Consequences for this series:
**0001, 0003, 0004, 0006 are DEAD in the guest** (their zink/kopper code paths no longer run in
any supported config) — physically drop them at the next guest-mesa respin (the F43 base
repoint), which also shrinks the pool ahead of the limina-guest fork migration (task #24).
**0014**'s guest trigger is likewise gone (it lives on in the host zink-on-KK build, and stays
an upstream-now MR — the bug is real upstream). **0002 stays**: the fbobject NULL guard is core
GL-frontend code (its observed trigger was the zink session, but the guard is generic, free,
and still an upstream-now candidate). The venus rows (0009–0013, 0015–0017) are untouched —
venus is now the *only* guest consumer of these patches. `scripts/build-mesa-zink.sh` archived
(`scripts/archive/`). Upstream-MR verdicts above are unaffected: dead-in-guest ≠ not-worth-sending.

**Enhanced-tier rubric (series):** all patches ship only in the guest mesa RPMs; a
stock guest degrades to llvmpipe rather than breaking (0012 exists precisely to keep
that degradation non-fatal on the GRUB-fallback path). Exit strategy is the README's
own documented-debt plan: each MR that lands shrinks the pool, and the F43 base
repoint retires 0009 + 0016-pre outright.

**F43 repoint EXECUTED (2026-08-05):** the F43 RPM now builds from the F44 koji
`mesa-26.1.5-1.fc44` SRPM in the fc43 container; both families ship the identical
venus-only set (0015 + 0011–0013 + 0016 + 0017). Consequences for rows above:
**0016-pre is deleted from the pool** (verbatim upstream, zero consumers);
**0009** is retired from every RPM (file remains only for the stale
`scripts/build-venus.sh` dev vehicle, with 0010); **0001/0014** now ship only in the
F44 RPM until its next respin physically drops them. Upstream-MR verdicts unchanged.
The pool is one respin away from the venus-only set everywhere — the precondition
for the limina-guest fork migration.

**limina-guest fork migration EXECUTED (2026-08-05, task #11):** the shipping venus set now
lives as commits on `liminavm/mesa`'s `limina-guest` branch (base `mesa-26.1.5`, pinned by
`third_party/manifest.toml [mesa-guest]`; worktree `/Volumes/mesa-cs/mesa-guest`), exported
by `scripts/export-mesa-guest-patches.sh` into the committed `patches/mesa-guest/` series
that BOTH RPM tracks apply. Row→fork-commit map (new series ordinals): 0015→0001
`77a9a216d0b`, 0011→0002 `a4b71ec449e`, 0012→0003 `0bea34bcca9`, 0013→0004 `03f36b269f8`,
0016→0005 `c2c0706e85b`, 0017→0006 `5f910f4188c`. The old `patches/mesa/` pool is retired
as a build input (tombstone README; the migrated diffs deleted, the dead-in-guest
upstream-queue rows 0001/0002/0003/0004/0006/0014 + the historical 0009/0010 remain as
files). `scripts/build-venus.sh` archived with it. Rows above stay keyed by their OLD
subjects; upstream-MR verdicts unchanged — dead-in-guest ≠ not-worth-sending.
| 0007 | venus: allocate dma-buf import memory synchronously | `src/virtio/vulkan/vn_device_memory.c` |  | needed — main vn_device_memory.c still routes dma-buf import through `vn_device_memory_alloc_simple` (async), so a host refusal is invisible to the caller | main `2b7a72457a5` 2026-08-13 | none-yet | none-yet — **ACTION: file the MR** | no | standalone | guest-enhanced, but the bug bites ANY venus guest whose host refuses an import | **upstream-now** — small, and the argument is upstream's own: import failure is expected runtime state (unlike a plain alloc, where failure means OOM), so the error must reach the caller that can fall back. Cite the `VN_PERF(NO_ASYNC_MEM_ALLOC)` precedent | trigger = a udmabuf that is legitimately unattachable on a macOS host; without this the guest holds a ghost handle and the next command naming it kills the whole context ([[limina-venus-ghost-tombstone]]) |
| 0008 | zink: don't recurse forever populating a shadow attachment | `src/gallium/drivers/zink/zink_render_pass.c` |  | needed — byte-identical on main: `zink_render_attachment_shadow` still masks every attachment's clears EXCEPT the one being shadowed, and still sets `transient->valid` only after the blit | main `2b7a72457a5` 2026-08-13 (fetched, diffed) | none-yet | none-yet — **ACTION: file issue + MR** | no | standalone | guest-enhanced **+ host zink-on-KK** (same fix on `limina-kk` `3c759eecf59`) | **upstream-now** — zero limina-specific content, deterministic pixel-checked reproducer, and u_blitter's own "Caught recursion. This is a driver bug." names it | reachable on ANY driver without VK_EXT_multisampled_render_to_single_sampled; found as an Epiphany/WebKit stack overflow. Reproducer + trap notes: `spikes/zink-shadow-recursion/` |
