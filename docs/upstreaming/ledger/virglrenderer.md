<!-- ledger-check: dep=virglrenderer manifest=third_party/virglrs/third_party/manifest.toml base=aafa9bd234a43c31004ec768ce000b21cf7b99ca paths=src,server,meson.build,meson_options.txt,config.h.meson,tests -->
# virglrenderer — patch-audit ledger

181 C-tree commits; base **`aafa9bd2`** on upstream `main` ("venus: fix the vulkan_metal.h
include on macOS", 2026-09-28, the merge-base after the 2026-10-02 rebase — see *Rebase onto
`aafa9bd2`* in Findings). Upstream's `master` branch is stale (2023, `d74a494a`); `main` is the
live one. Schema + protocol: `README.md`.
Rows are keyed by SUBJECT; ordinals are informational and drift on re-export.

**Scope.** The `limina` branch carries 616 commits over the base; this ledger tracks only the
181 that touch the C tree (`src/`, `server/`, `meson.build`, `meson_options.txt`,
`config.h.meson`, `tests/`, the paths named in the marker on line 1). The other 435 are the
virglrs Rust rewrite, its harness and docs (`virglrs/`, `harness/`, `docs/`, `CLAUDE.md`):
history of the rewrite, not a delta against upstream virglrenderer, so they get no rows.

**Coverage.** The main table holds the 58 commits audited on 2026-08-03; *Post-audit commits*
in Findings lists 36 more with first guesses; *Unaudited commits (skeleton, 2026-10-16)* at the
end lists the remaining 87 with unresearched first guesses.

**Where the pin lives.** limina no longer pins the C tree: the pin is `[virglrenderer]` in
virglrs's `third_party/manifest.toml` (rev `a649897f`, the branch head), because virglrs
generates code from this tree and records its harness goldens against it. This ledger stays in
limina until someone moves it next to that pin.

> **Fork model since 2026-08-04.** There is no `patches/virglrenderer/` any more — our delta is the
> commits on the `limina` branch of `github.com/liminavm/virglrenderer`, pinned (since the virglrs
> split) by virglrs's `third_party/manifest.toml`. Read a "patch" here as "the commit with this subject". The branch is
> rewritten as patches merge upstream or get dropped, so **tag before every rewrite** — every rev
> ever pinned in the manifest must stay reachable. The two commits that joined at migration have
> skeleton rows: `7682afc3` (supersedes mesa 0010(a); upstream-now candidate) and `7b872e76`
> (gated off by default, needs KK's MTLTEXTURE handle type).

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | venus/macOS: fix shm_open O_CLOEXEC, filter host-unsupported device exts, add get_map_ptr | `src/mesa/util/anon_file.c`, `src/venus/vkr_device.c`, `src/virglrenderer.c` +1 |  | hunk1 (shm_open O_CLOEXEC) **superseded-upstream@058959ed** (virgl!1634, 2026-06-16, identical); hunks 2-3 (ext-filter, get_map_ptr) needed | tip `956b034f` 2026-08-03 | none-yet | !1634 (merged, hunk1 only); ext-filter vehicle = osy draft !1586 | no | **split**: drop anon_file hunk on rebase; ext-filter + get_map_ptr are separate concerns | host | hunk1 drop-on-rebase; hunk2 upstream-after-cleanup (via !1586 "emulate unsupported extensions"); hunk3 carry | 07-04 "send-now" on shm_open is DONE (upstream fixed independently 9 days after ours, same shape). Ext-filter: upstream instead advertise-only-injects KHR_external_memory_fd + relies on guest mesa!40478 not enabling it — old guests that DO enable still need our strip. get_map_ptr only meaningful with the (non-landed) map_ptr model |
| 0002 | venus/macOS: emulate eventfd (kqueue) + pass fence eventfd by value in same-process mode | `server/render_context.c`, `server/render_protocol.h`, `src/proxy/proxy_context.c` +1 |  | needed — tip virgl_util.c has NO __APPLE__ eventfd path; has_eventfd() still false on macOS so upstream venus proxy async-fence is dead there | tip `956b034f` 2026-08-03 | none-yet | none found (no eventfd/kqueue MR) | no | standalone (by-value-fd hunk rides with it) | host | **upstream-now candidate** — clean, small, benefits krunkit | ENABLE_SAME_PROCESS_RENDER_SERVER (render-server-mode=thread) IS upstream-supported at tip (meson.build:423) so the by-value hunk gates on an upstream mechanism, not a limina invention; good first-wave MR |
| 0003 | vkr: vkr_mtl_iosurface_alloc — IOSurface-backed MTLTexture for zero-copy scanout | `meson.build`, `src/venus/vkr_metal_helpers.h`, `src/venus/vkr_metal_helpers.m` |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | Foundation of the IOSurface scanout stack (0004/0006/0011/0053 build on it); limina-only zero-copy present model |
| 0004 | vkr: back guest "external" scanout images with an IOSurface | `src/venus/vkr_image.c`, `src/venus/vkr_image.h`, `src/venus/vkr_metal_helpers.m` |  | needed (chained) | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone (absorbs 0005, 0008) | host | carry | MVK-era but introduced the mtl_iosurface tracking the KK path uses; retire only with re-validation (series README) |
| 0005 | vkr: IOSurface-backed scanout — also normalize DRM_FORMAT_MODIFIER scanout images | `src/venus/vkr_image.c` |  | needed (chained) | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: back guest "external" scanout images with an IOSurface | host | carry | Continuation of fix A; same MVK-era caveat |
| 0006 | venus(macos): IOSurface-backed exportable scanout memory + resource->IOSurface resolve | `src/venus/vkr_device_memory.c`, `src/venus/vkr_device_memory.h`, `src/venus/vkr_image.c` +4 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | mtl_shm carrier export mechanism; embeds the old zero-init virgl_context_blob sec fixlet (07-04 MED info-leak, already squashed in) |
| 0007 | vkr: thread the scanout IOSurface id through the render-server proxy | `server/render_context.c`, `server/render_protocol.h`, `server/render_state.c` +4 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | Marshals a limina-only blob field through the proxy; inert off-macOS; chained to 0006 |
| 0008 | venus(macOS): back fix-A scanout images via useIOSurface (VkImportMetalIOSurfaceInfoEXT) | `src/venus/vkr_device_memory.c`, `src/venus/vkr_image.c` |  | needed (chained) | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: back guest "external" scanout images with an IOSurface | host | carry | Pure MoltenVK; MVK retired 2026-06-13; keep only while fix A lives |
| 0009 | venus(macOS): share HOST_VISIBLE memory by pointer (map_ptr), not imported shm | `server/render_context.c`, `server/render_protocol.h`, `server/render_state.c` +9 |  | needed | tip `956b034f` 2026-08-03 | none-yet | none — rejected lookalikes !1582/!1583 (closed); upstream shipped the OPPOSITE (fd) model !1602 | no | standalone (head of the same-process/pointer-sharing alt architecture) | host | **carry** — architecturally divergent from upstream's merged fd model; NOT upstreamable wholesale | PIVOTAL: krunkit/slp map_ptr model did NOT land and is not pending — upstream merged SHM-fd-backed MTLBuffer via VK_EXT_external_memory_metal (multi-process workers). PREMISE TENSION to resolve before ever upstreaming: #28 root-caused that exact double-mmap as CPU/GPU-incoherent on our stack, yet upstream validated it at ~1770 FPS on M4 Max — either our same-process/MVK/16k-specific, or a latent upstream bug — CONVERGENCE PATH (2nd pass): upstream's in-flight !1617 adds a first-class non-fd `VIRGL_RESOURCE_METAL_HEAP` type (CFRetained MTLHeap, one driver-owned mapping = our coherence answer); zzyiwei receptive ("behind a new option"), stalled ~07-10 needing rebase. WORK ITEM: add VK_EXT_external_memory_metal (MTLHEAP) to KK (we own it), then align on !1617 — do NOT upstream the raw-VA diff |
| 0010 | venus: hide index_type_uint8 on __APPLE__ — Metal-backend uint8 index conversion corrupts quads | `src/venus/vkr_common.c` |  | needed (as policy) | tip `956b034f` 2026-08-03 — advertises KHR/EXT_index_type_uint8 unconditionally (vkr_common.c:98,171), no Metal carve-out | none-yet | none | no | standalone | host | carry, **pending the KK probe** | no uint8-index fix anywhere in our KK series or upstream; the hide was never re-validated vs KK emulation — probe quads-with-u8-indices through KK before dropping; if KK is broken the right fix is in KK, not vkr |
| 0011 | venus: KosmicKrisp winsys enablement — zero-copy IOSurface scanout via linear images | `src/venus/vkr_command_buffer.c`, `src/venus/vkr_device.c`, `src/venus/vkr_device_memory.c` +5 |  | needed | tip `956b034f` 2026-08-03 — vkr has NO handling for drivers lacking external-fd handle types beyond the MoltenVK metal-handle path; a KK-class driver is unsupported upstream | none-yet | none (interest: closed !1583, open !1617 iOS, !1627 Neptune, draft !1586) | no | **strip-diag-first** (LIMINA_KK_RTLOG in vkr_command_buffer.c/vkr_pipeline.c) | host | carry core; IFP2 strip-and-synthesize sub-part = same idea as osy draft !1586 — engage there | !1586 "emulate the parts of these extensions Venus uses in virglrenderer" explicitly names KK and asks for feedback — the upstream path for the synthesize piece — 2nd pass: !1617 + !1618 (VMM-facing Metal-texture scanout = our IOSurface-scanout analog) are building this capability in MTLHeap/MTLTexture shape; !1582 pre-argued against IOSurface global ids (deprecated, needs XPC cross-process). Contribute to !1617/!1618 hw-WSI review rather than sending 0011 |
| 0012 | venus: log per-pipeline dynamic-state list + static blend write masks (LIMINA_KK_RTLOG) | `src/venus/vkr_pipeline.c` |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | strip-diag-first | host | carry (drop-candidate) | LIMINA_KK_RTLOG instrumentation only; its finding (zink 32b4412d regression) fixed upstream mesa — drop candidate at next re-export |
| 0013 | venus(macOS): cross-context dmabuf import — compositor can import client window buffers | `server/render_context.c`, `server/render_protocol.h`, `server/render_state.c` +11 |  | needed | tip `956b034f` 2026-08-03 — vkr_get_fd_info_from_resource_info handles only DMABUF/OPAQUE (vkr_device_memory.c:29-38), default:return false → VK_ERROR_INVALID_EXTERNAL_HANDLE | **none-yet — genuine upstream gap, issue-worthy** | none | no | standalone | host | upstream-after-cleanup (the SHM/host-pointer generalization); carry the IOSurface/map_ptr specifics | on upstream's OWN merged macOS model venus blobs are VIRGL_RESOURCE_FD_SHM → a compositor importing a client wl_buffer hits this silent-failure path on krunkit TODAY (our ring-poison → gnome-shell SIGABRT). Well-shaped issue+MR would serve krunkit too; our specific plumbing depends on 0009 → carry — 2nd pass: !1617 adds a typed METAL_HEAP branch to the res_info import path — superseded-in-flight; never upstream the ptr-vs-fd `u.data>=0x10000` heuristic |
| 0014 | venus(macOS): gate the IFP2 external-image synthesize to KosmicKrisp hosts | `src/venus/vkr_physical_device.c` |  | needed (chained) | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:venus: KosmicKrisp winsys enablement — zero-copy IOSurface scanout via linear images | host | carry | Protects retired MoltenVK from 0011's IFP2 synthesize; dies with the MVK legs |
| 0015 | vkr: release the backing IOSurface on device/context teardown, not just vkDestroyImage | `src/venus/vkr_device.c`, `src/venus/vkr_image.c`, `src/venus/vkr_image.h` |  | **no upstream need — bug is limina-only**: tip vkr_image has zero Apple/IOSurface state; vkr_device_object_destroy already releases mtl_shm/gbm_bo/udmabuf (pattern is upstream-correct) | tip `956b034f` 2026-08-03 | none | none | no (exhaustion of limina-introduced IOSurface refs; no upstream code path) | carry as-is; **rename** the 2 limina: comment tokens | host | carry — becomes upstreamable only if the IOSurface image backing (0003/0004/0011) goes up, then folds INTO that series | the leak exists only because our 0003/0004 hung mtl_iosurface off vkr_image |
| 0016 | vkr: present fences — reserved ring 63 retires at true GPU completion | `src/venus/vkr_common.h`, `src/venus/vkr_context.c`, `src/venus/vkr_context.h` +4 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | Mechanism clean but the trigger/consumer is limina's RESOURCE_FLUSH present model (libkrun injects the fence); Class C per full review |
| 0017 | vkr: make LIMINA_GLOBAL_SCANOUT additive for venus scanouts | `src/venus/vkr_metal_helpers.m` |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | yes:info-leak-fix | strip-diag-first | host | carry | Mach-port scoping = limina process glue; closes same-user screen read via global IOSurface ids; the debug flag is the iosdump oracle — keep, env-gated |
| 0018 | vkr: read a scanout IOSurface into a CPU buffer for the headless capture sink | `src/venus/vkr_metal_helpers.h`, `src/venus/vkr_metal_helpers.m`, `src/virglrenderer.c` +1 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | Headless capture-sink readback API; limina test oracle (window-capture / L2) |
| 0019 | vkr: seq_cst idle-check tail load — block the ring at idle (0 wakeups), fix limina#30 at root | `src/venus/vkr_ring.c` |  | **needed — race verbatim on tip**: idle-check tail load still memory_order_acquire (vkr_ring.c:75,275); IDLE store seq_cst; idle wait plain cnd_wait | tip `956b034f` 2026-08-03 (branch-tip) | none-yet (SB race unreported) | none competing; adjacent merged !1610 (spurious-wakeup, in base) fixes a DIFFERENT lost-pickup mode, leaves the acquire load | yes:DoS/liveness (guest-fatal vn_relax abort) | standalone; strip limina comment + trim the litmus essay to MR prose | host (OS-agnostic; worst on weakly-ordered ARM) | **upstream-now — series flagship** | ~3-line MR: seq_cst tail load in the idle check ONLY (data path untouched); frame as weak-memory lost-wakeup letting a guest abort in vn_relax; cite !1610 as sibling fix, cc digetx/ryanneph, Apple-Silicon/LDAPR repro hardware |
| 0020 | vrend: fix WebRender/Firefox tile-displacement tear — orphan refill via GL_MAP_INVALIDATE_BUFFER_BIT | `src/vrend/vrend_renderer.c` |  | needed — race verbatim on tip (vrend_renderer_transfer_write_iov GL_MAP_UNSYNCHRONIZED_BIT at 9311) | tip `956b034f` 2026-08-03 | none | none | no | standalone | host (vrend/GL) | **send-now** — generic GL correctness, OS-agnostic, strongest of the cluster | the fix infers orphan intent from box.x==0 — frame the heuristic-vs-protocol-plumbing tradeoff in the MR (upstream may ask the virgl transfer protocol to carry the guest invalidate bit instead) |
| 0021 | vrend(macOS): wire the no-GBM surfaceless EGL winsys for GBM-less hosts | `src/vrend/vrend_winsys.c` |  | needed — tip vrend_winsys_init only wires GBM; the non-GBM virgl_egl_init variant sits defined-but-unreachable (vrend_winsys_egl.c:304) | tip `956b034f` 2026-08-03; file-touch `fedc27e1` 2025-04-19 (dir move) | none | none; macOS enablement active (!1600 merged) | no | pairs with 0057 as a "vrend on macOS" series | host (vrend/GL) | **send-now** — pure wiring of an existing upstream path | drop the "limina:" comment prefix; benefits any GBM-less EGL host, not just macOS |
| 0022 | virgl: munmap a live VMM mapping on resource unref | `src/virglrenderer.c` |  | **yes — structural upstream bug**: tip virgl_resource_destroy_func (virgl_resource.c:43) unrefs pipe_resource + close(fd) but NEVER touches res->mapped → any resource unref'd while mapped leaks the host mapping for every caller; crosvm's unref is bare, balance unenforced | tip `956b034f` 2026-08-03 | none open | none — **FILE MR** (precedent: !1570/686b63e3, same reviewer Yiwei Zhang, adjacent map/unmap-leak, reasons about QEMU vs crosvm callers) | yes:dos (host address-space exhaustion → ENOMEM via blob create/destroy churn); public MR OK | one-liner; drop the limina "#28 borrowed pointer" phrasing | host | **upstream** | placement question for reviewers: virgl_renderer_resource_unref (where we put it) vs virgl_resource_destroy_func (owns fd close but no map knowledge) |
| 0023 | vrend: infer a map caching type for zink renderers | `src/vrend/vrend_renderer.c` |  | needed — tip is_mesa = Mesa/DRM/llvmpipe only, "zink" appears nowhere in tip vrend_renderer.c → blob maps fail on any zink host GL stack | tip `956b034f` 2026-08-03 | none | none; direct precedent = !889 "Specify memory caching type for llvmpipe" (merged) | no | standalone | host (vrend/GL) | **send-now** — !889 makes it an easy review | **RENAME: in-code comment says "(limina)"**; keep the commit-msg rationale for setting caching type AFTER the ARB_BUFFER_STORAGE cap check (gives zink map_info without flipping guests onto persistent buffers) |
| 0024 | vkr: vkGetMemoryResourcePropertiesMESA answers IOSurface/shm resources | `src/venus/vkr_device_memory.c` |  | needed (chained) | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | Query/import consistency for the limina import architecture (0009/0013); 07-04: borderline, macOS/limina-shaped; revisit if Cluster A finds an upstream import model |
| 0025 | vkr: clamp host-pointer import allocationSize to the backing size | `src/venus/vkr_device_memory.c` |  | needed (chained) | n/a-local (triage 2026-08-03) | n/a | n/a | yes:guest-OOB (fixed; limina-introduced code — no disclosure per 07-04 B3) | fold-into:venus(macOS): cross-context dmabuf import — compositor can import client window buffers | host | carry | Hardens our own import paths (0009/0011/0013); must ride any upstream submission of those |
| 0026 | venus: never poison the ring on a missing resource id; log every FATAL site | `src/venus/vkr_common.c`, `src/venus/vkr_common.h`, `src/venus/vkr_context.h` +2 |  | **needed — both poison sites verbatim on tip** (vkr_device_memory.c:24, :477 set_fatal) | tip `956b034f` 2026-08-03 (branch-tip) | none-yet (CREATE_BLOB fire-and-forget race undiscussed; rejected lookalike #536 = different bug) | none; adjacent merged !1378/!1379/!1390 (ring-fatal robustness, in base); softening precedent !1299 | yes:guest-process-fatal from a legit race | split: the two demotions = upstreamable core; the set_fatal_at/log site-layer folds with 0029/0031 | host | upstream-after-cleanup | needs the fire-and-forget narrative airtight (guest ioctl returns before host response ⇒ dead-id import is legal); expect pushback that FATAL is deliberate anti-corruption policy; clean of naming |
| 0027 | venus: make ring-monitor health observable; raise its QoS on macOS | `src/venus/vkr_context.c` |  | partially — tip monitor stamps once per full report_period with no lateness telemetry, no QoS (vkr_context.c:529-566) | tip `956b034f` 2026-08-03 (branch-tip) | **none-yet — the ~480ms margin is upstream-latent**: mesa vn_common.c:417 hardcodes 3.4816s warn vs 3.0s period, 250ms safety_margin; we saw a 489ms-late stamp near-idle | none; rejected lookalike mesa!34088 (owner-switch false expiry, already fixed) | borderline (guest-fatal on host scheduler jitter) | fold-into:0028 (0028 rewrites both hunks); QoS pin = separate macOS hunk | host; QoS hunk macOS-only | carry as merged-with-0028 | limina: comments to strip |
| 0028 | venus: demote client-teardown ring FATALs to INFO; oversample the ALIVE stamp | `src/venus/vkr_context.c`, `src/venus/vkr_context.h` |  | **needed — tip vkr_context_destroy_resource still set_fatal on teardown detach (vkr_context.c:399), fires on every venus client exit**; stamping still 1×period | tip `956b034f` 2026-08-03 (branch-tip) | none-yet | none | no (noise/robustness) | absorbs 0027; three separable units: (a) teardown demotion (b) period/3 oversampling (c) macOS QoS pin | host; (c) macOS-only | **(a)+(b) upstream-after-cleanup; (c) carry** | oversampling is protocol-compliant on its face (field is maxReportingPeriodMicroseconds — an upper bound); widens slack ~480ms→~2.5s for stock guests too; cite mesa's own safety_margin assert as the framing |
| 0029 | venus: attribute every ring FATAL and log the silent host-side failures that cause them | `src/venus/vkr_context.c`, `src/venus/vkr_context.h`, `src/venus/vkr_cs.c` +3 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | Observability layer on the 0026 FATAL sites; parts could accompany 0026 upstream but the logging shape is limina's |
| 0030 | venus: close the export-side SHM carrier fd; free path inferred mmap from fd_type | `src/venus/vkr_context.c`, `src/venus/vkr_context.h`, `src/venus/vkr_device_memory.c` |  | **yes — and our "upstream not affected" note is now FALSE at tip**: since e526766d (MR 1602, 2026-04) + 8cebba7f (dropped the __APPLE__ guard, 2026-06), vkr_device_memory_export_blob returns FD_SHM-with-owned-fd → upstream munmaps the fd NUMBER and leaks the fd, one per exportable VkDeviceMemory | tip `956b034f` 2026-08-03 | none | none — **FILE MR** (Fixes: e526766d) | yes:dos (fd exhaustion → EMFILE). NOT memory-safety: the munmap'd "pointer" is a small fd int, never page-aligned, low addr unmapped → EINVAL/no-op. public MR OK | **split 3 ways**: (a) upstream = u_is_fd field + free path (~10 lines, no limina); (b) limina = the limina_res plausibility-hack cleanups; (c) diagnostics = ctx-destroy INFO + mtl_shm leak canary (optional) | host | **upstream (a); carry (b); optional (c)** | strongest most-attributable virgl MR in the cluster; already-engaged audience (Amaral, Zhang), live macOS/iOS venus effort !1617/!1618; 9 limina tokens to scrub |
| 0031 | venus: attribute the silent failure classes — ringmon confession, ghost-alloc logging, fd tracing, IOSurfaceRef leak | `src/venus/vkr_common.h`, `src/venus/vkr_context.c`, `src/venus/vkr_device_memory.c` +1 |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | strip-diag-first | host | carry | Confession logging + macOS fd audit, chained to 0029/0030; the IOSurfaceRef leak fixlet inside is real — note for the fold plan |
| 0032 | venus: close the received fd on cross-context SHM blob import (the attach-side carrier-fd ratchet) | `src/venus/vkr_context.c`, `src/virglrenderer.c` |  | **yes — latent upstream, all platforms**: tip vkr_context_import_resource_from_shm (vkr_context.c:257) mmaps and returns true WITHOUT closing fd; render_context_dispatch closes req_fds only on the fail: path. Reachable on Linux (blob_id==0 mappable shm blobs to a 2nd context) | tip `956b034f` 2026-08-03 | none | none — **FILE MR** (Fixes: 26393fb8) | yes:dos (fd exhaustion → EMFILE, VM-lifetime-persistent; the leaked fd is in no context scope). public MR OK | **split**: upstream = 1-line close(fd) + comment; the VKR_FD_TRACE tracing is limina diag — keep local (holds the 1 limina token) | host | **upstream (fix only)** | BONUS upstream bug for the same MR: vkr_renderer_import_resource still assert(DMABUF||OPAQUE) (vkr_renderer.c:223) — SHM attach aborts debug builds, evidence the path is untested upstream |
| 0033 | venus: limina M9.3 diagnostics — thread-safe vkr context-table dump | `server/render_state.c`, `server/render_state.h`, `src/venus/vkr_renderer.c` +3 |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | strip-diag-first | host | carry | virgl_renderer_limina_* API + LIMINA_GPU_TRACE_VKR; M9.3 diagnostics |
| 0034 | vkr: snapshot-replay re-creation journal (limina M9.3 seamless resume, P0) | `src/meson.build`, `src/venus/venus-protocol/vn_protocol_renderer_dispatches.h`, `src/venus/vkr_buffer.c` +8 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone (journal head; absorbs 0036/0037/0040/0050) | host | carry | Limina snapshot/resume feature; upstream venus has no snapshot story; VKR_JOURNAL kill switch |
| 0035 | vkr: journal export + replay API for venus snapshot restore (limina M9.3 P1) | `server/render_state.c`, `server/render_state.h`, `src/venus/vkr_context.h` +9 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | limina-named FFI; chained to the journal (0034) |
| 0036 | vkr: journal-key descriptor updates by their referenced objects (limina M9.3) | `src/venus/vkr_descriptor_set.c` |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: snapshot-replay re-creation journal (limina M9.3 seamless resume, P0) | host | carry | Correctness fixup on the journal |
| 0037 | vkr: pin blob-backing alloc journal entries until the GLOBAL resource unref (limina M9.3) | `server/render_state.c`, `server/render_state.h`, `src/venus/vkr_context.c` +9 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: snapshot-replay re-creation journal (limina M9.3 seamless resume, P0) | host | carry | Correctness fixup on the journal |
| 0038 | vkr: full device-memory content capture for snapshot restore (limina M9.3 P2) | `server/render_state.c`, `server/render_state.h`, `src/venus/vkr_device_memory.c` +5 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | limina snapshot FFI; chained to the journal (0034) |
| 0039 | vkr: sync-object fast-forward across snapshot restore (limina M9.3 P2.1) | `server/render_state.c`, `server/render_state.h`, `src/venus/vkr_context.c` +7 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | limina snapshot mechanism; chained to the journal (0034) |
| 0040 | vkr: journal create-arg closure — pin every CREATE's decoded handle refs (limina M9.4) | `src/venus/vkr_cs.h`, `src/venus/vkr_journal.c`, `src/venus/vkr_journal.h` |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: pin blob-backing alloc journal entries until the GLOBAL resource unref (limina M9.3) | host | carry | Generalizes 0037's pin machinery |
| 0041 | vkr: ring relax backoff — one sleep per rung, not per iteration | `src/venus/vkr_ring.c` |  | needed — `/* TODO do better */` loop verbatim on tip (vkr_ring.c:191); only churn since = our upstreamed nanosleep hunk (in base) | tip `956b034f` 2026-08-03 (branch-tip) | none-yet | no competing backoff MRs; adjacent merged !1045/!1046 are lock/space-wait, not the relax ladder | no | **squash head of 0041+0043+0044+0049 → ONE upstream artifact** | host (OS-agnostic perf) | upstream-after-cleanup (squashed) | judge the SQUASH, not 0041 alone — 0041-as-shipped regressed vkmark 2760→1193 until 0043/0049 recovered it; never send standalone |
| 0042 | vkr: LIMINA_WAKE_TRACE ring wake accounting (diagnostic, env-gated) | `src/venus/vkr_ring.c` |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | strip-diag-first | host | carry (drop-candidate) | Pure env-gated diagnostic; the wakeup-attribution oracle (use FIRST per limina-overhead-trim) — keep while perf work lives |
| 0043 | vkr: adaptive relax plateau — recover ring throughput, keep the idle-wakeup win | `src/venus/vkr_ring.c` |  | needed (as part of squash) | tip `956b034f` 2026-08-03 (branch-tip) | n/a | n/a | no | fold-into:0041 (squash) | host | squash | adaptive relax plateau |
| 0044 | vkr: adaptive relax plateau DEPTH — regime-classified per ring (limina wakeup round 2) | `src/venus/vkr_ring.c`, `src/venus/vkr_ring.h` |  | needed (as part of squash) | tip `956b034f` 2026-08-03 (branch-tip) | n/a | n/a | no | fold-into:0041 (squash) | host | squash | **rename LIMINA_RELAX_WARM_MAX/WARM_MIN/LONG_IDLE_US/WINDOW_MS/COARSEN_PCT → VKR_-prefixed or constants for the MR** — env surface is the main cleanup cost |
| 0045 | vkr: sanitize guest empty VkClearRects in vkCmdClearAttachments | `src/venus/vkr_command_buffer.c` |  | **yes, but expect pushback**: tip vkr_dispatch_vkCmdClearAttachments (vkr_command_buffer.c:397) is a bare VKR_CMD_CALL — no rect validation anywhere in the decode layer | tip `956b034f` 2026-08-03 | none | none — **FILE MR** (guest-triggerable host-driver abort, defense-in-depth) | yes:dos (guest-triggerable host abort). public MR OK | already mechanism-only and limina-free; keep as-is | host | **upstream** | counter-signal: upstream dropped venus-validate (4df196e3) leaning on render-server process isolation — but limina runs vkr IN-process so an abort kills the VMM. NOT redundant with the mesa/vk_meta fix (that fixes one driver; this closes the trust boundary for all). Per-incident validation precedent exists (144da865, d584798c, 058fa04b). 2026-10-16 audit: a follow-up on the branch, `1a9b057b` *"vkr: also drop negative-offset and i32-overflow VkClearRects"*, extends this sanitize (negative offsets and int32 overflow reached the host driver's meta-clear asserts), so "keep as-is" predates it; whether the two go up as one MR is for a human. Upstream `main` `07a5bc39` still has the bare `VKR_CMD_CALL` |
| 0046 | vkr: LIMINA_RING_WAKE_PROFILE — split the parked-ring wake chain into hops | `src/venus/vkr_ring.c`, `src/venus/vkr_ring.h` |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone (absorbs 0047/0048) | host | carry | Pure diagnostic; answered the venus-wake-chain question |
| 0047 | vkr: don't count a raced notify as this park's wake (RINGWAKE underflow) | `src/venus/vkr_ring.c` |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: LIMINA_RING_WAKE_PROFILE — split the parked-ring wake chain into hops | host | carry | Fixup on the profiler |
| 0048 | vkr: LIMINA_RING_WAKE_PROFILE=0 must turn the profile off | `src/venus/vkr_ring.c` |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: LIMINA_RING_WAKE_PROFILE — split the parked-ring wake chain into hops | host | carry | Fixup on the profiler |
| 0049 | vkr: graduated responsive relax ladder — recover the plateau-cliff throughput | `src/venus/vkr_ring.c` |  | needed (final shape of the squash: 12×10→8×20→8×40→hold 80µs; +7.3% vkmark) | tip `956b034f` 2026-08-03 (branch-tip) | n/a | n/a | no | fold-into:0041 (squash) — this is the end-state the MR presents | host | squash | rename LIMINA_RELAX_FINE_RUNGS/MID_RUNGS; pair 0041's wakeup numbers (15.5k→1.9k/s) with 0049's throughput as two-sided evidence |
| 0050 | vkr: VKR_JOURNAL=norecord — attribution knob for the RECORDING lane | `src/venus/vkr_journal.c` |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: snapshot-replay re-creation journal (limina M9.3 seamless resume, P0) | host | carry | Measurement mode only; snapshots deliberately broken under it |
| 0051 | vkr: move journal retention off the decode path (two-lane journal) | `src/venus/vkr_journal.c` |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone (journal-perf head; absorbs 0052, 0054 journal hunks, 0055) | host | carry | Decode-path cost fix for a limina feature; motivated by present-misses §17.3 |
| 0052 | vkr: journal queue-depth gauge — peak + instantaneous in the dump line | `src/venus/vkr_journal.c` |  | needed (diag) | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: move journal retention off the decode path (two-lane journal) | host | carry | Backlog oracle on 0051's queue |
| 0053 | vrend: zero-copy scanout — IOSurface + pinned-PBO blit for SCANOUT-bound textures | `src/venus/vkr_metal_helpers.h`, `src/venus/vkr_metal_helpers.m`, `src/virglrenderer.c` +3 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | standalone | host | carry | Plan A1; presents vrend scanouts through limina's VMM exactly like venus blobs; chained to 0003 |
| 0054 | venus: decode-lane cost fixes — cached rtlog env, journal msg-inline + batched push | `src/venus/vkr_command_buffer.c`, `src/venus/vkr_common.h`, `src/venus/vkr_context.c` +4 |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: move journal retention off the decode path (two-lane journal) | host | carry | Two concerns in one patch: journal batching (fold into 0051) + the LIMINA_KK_RTLOG getenv cache (must move with 0011's diag strip) |
| 0055 | venus: journal recording block lane — zero-alloc, near-lockless decode capture | `src/venus/vkr_journal.c` |  | needed | n/a-local (triage 2026-08-03) | n/a | n/a | no | fold-into:vkr: move journal retention off the decode path (two-lane journal) | host | carry | Perf iteration on the journal |
| 0056 | venus: object-lookup generation cache — lock-free repeat lookups | `src/venus/vkr_common.h`, `src/venus/vkr_context.h`, `src/venus/vkr_cs.c` +1 |  | **needed — per-command locked lookup verbatim on tip** (vkr_cs.h:311, mutex+hash per command; the lock was added upstream 944c8ca9 for a real race) | tip `956b034f` 2026-08-03 (branch-tip) | none-yet | no lookup-cache/lock-free MRs; adjacent !630/!1045 show upstream cares about this cost | no (correctness argument — generation tagging — must survive adversarial review) | standalone | host (OS-agnostic, ~10% decode lane) | **upstream-after-cleanup — strong second-wave candidate** | MR burden = the id-reuse safety argument (generation captured under mutex, relaxed fast-path read, stale ⇒ locked path); already in the commit message; clean of naming. 2026-10-16 audit: upstream `795d2475` *"vkr: avoid TOCTOU in object id validation"* (!1676, touches `vkr_context.h`, `vkr_physical_device.c`, `vkr_queue.c`) is now in the base; the id-reuse safety argument has not been re-read against it. `vkr_cs_decoder_lookup_object` still takes `object_mutex` per lookup at `07a5bc39` (`vkr_cs.h:302-311`) |
| 0057 | vrend: never use EGL native-fence syncs on macOS | `src/vrend/vrend_renderer.c` |  | needed — tip use_egl_fence = virgl_egl_supports_fences(egl) unconditionally (7746); supports_fences is advertisement-only (925-928); on macOS init itself hits the blocking-export fallback | tip `956b034f` 2026-08-03 | none | none; gating precedent !441 (gles-only 2020) + !1168 (GL re-enable 2023) | no | fold with 0021 into a macOS-vrend series | host (vrend/GL) | **send-now** — a !defined(__APPLE__) gate is a natural third refinement | a runtime probe is infeasible (the failure mode IS a blocking export), so the __APPLE__ gate is the honest shape |
| 0058 | vkr: a slow ring wait is not a fatal error (it was poisoning the context) | `src/venus/vkr_context.c` |  | **NOT needed upstream — the bug is OURS**: tip vkr_context_wait_ring_seqno uses plain cnd_wait, no timedwait/STUCK branch; the poisoning was introduced by our 0039 diagnostic | tip `956b034f` 2026-08-03 (branch-tip) | none-yet | none (self-inflicted) | no upstream exposure | **fold-into:0039** (fixes 0039's own diagnostic) | host | carry (folded) | the trap it documents IS real upstream (vendored c11 shim maps ETIMEDOUT→thrd_busy, non-conforming; tip monitor already codes to the quirk at vkr_context.c:538) — at most a comment-grade upstream courtesy; LIMINA_RING_WAIT_WARN_MS has no upstream analog, stays limina |

## Findings

### Post-audit commits (unaudited — added after the 2026-08-03 pass)

The table's 58 rows are the audited series. The 36 commits below have landed on the branch since,
and none has been researched against upstream yet. Listed so the drift is visible rather than
implied by a stale count. SHAs are those on the current branch (after the 2026-10-02 rebase); the
rest of the post-audit commits, including the VideoToolbox decode stack, are in *Unaudited commits
(skeleton, 2026-10-16)* at the end.

| commit | subject | first guess at disposition |
|---|---|---|
| `98fb854f` | vrend: add the classic snapshot-replay re-creation journal (P0: record + census) | carry (limina snapshot model) |
| `57b596c9` | vrend: journal export + replay for classic snapshot restore (P1) | carry |
| `86c60065` | vrend: host-side resource content capture for snapshot restore (P2) | carry |
| `ade06a04` | vrend: VREND_CONTENT=0 kill switch for the content export | carry (chained) |
| `6adbc74f` | vrend: back VIRGL_BIND_SHARED resources with IOSurfaces too | carry — chains onto the IOSurface backing (0053) and the fd-less classic attach; upstreamable only if that stack goes up |
| `3738a2a4` | venus: bound the host GPU memory a guest can hold | **send-later, standalone** — the accounting half is host-agnostic and generally useful (any VMM can be killed by a guest that over-allocates); the ledger + histogram would need no limina context to explain. The `LIMINA_GPU_MEM_BUDGET_MIB` name and the default-off policy are ours and would need renaming; splitting accounting from enforcement is the natural shape upstream |
| `c738e545` | venus: periodic GPU-memory census, and read the modifiers before the cap | chains onto `3738a2a4`; goes up or stays with it |
| `9ab89040` | budget: charge classic resources to their own bucket, not a venus context | chained (accounting half of `3738a2a4`) |
| `535137e4` | budget: report lifetime charges, not just what is live | chained |
| `d814b4b4` | vkr: tag scanout traces by context, log lifetime totals at every teardown | diag — strip or drop before any send |
| `d69264c7` | vkr: census the +1/-1 balance of every host reference taken on an IOSurface | diag — same |
| `3614d8f7` | venus: count DEALLOCs, not releases, for scanout surfaces and textures | chained to the IOSurface stack |
| `f8c4f822` | venus: reject a zero-size vkCreateBuffer at the trust boundary | **send-later, standalone** — a guest-triggerable host-side fault at a trust boundary, no limina context needed; classify for disclosure before filing |
| `049cc4cf` | venus: answer VK_EXT_memory_budget from the GPU-memory ledger | chained to `3738a2a4`; the natural second half of that MR |
| `800c1358` | venus: poison a cmd_buf's replay after a failed RECORDING entry | unassessed |
| `8cb5678f` | venus: contain ghost objects instead of killing the context | unassessed — related in spirit to mesa 0016 (ring loss is not an abort) and to the open !42501 |
| `5ec40a96` | vrend: force the Metal linear row pitch on classic IOSurfaces | carry (macOS/IOSurface-specific) |
| `99793b3d` | vkr: default the KK MTLTEXTURE scanout path ON, one parse for all sites | carry (chained to the KK scanout stack) |
| `d5a7941a` | vrend: complete a CPU transfer into a shared IOSurface before returning | carry |
| `7cf65acf` | vkr: log the pre-clamp memory budget, so a moving number can be attributed | diag |
| `f73e6a95` | vkr: let the supervisor ask for a scanout IOSurface back | carry (limina supervisor protocol) |
| `3dda8478` | vrend: sample guest-memory blobs from the guest's own pages | **carry** — it exists because macOS has no dmabuf, so a `blob_mem=GUEST` resource can only be sampled by copying the guest pages into the texture. On a dmabuf host the import aliases the pages and no copy is wanted, so the mechanism is not upstream-shaped as written; the generalizable part is "a guest blob with no importable fd is not automatically untypeable" |
| `85851981` | vrend: sample a planar-YUV guest blob by converting it to RGBA | **carry** — chained to `3dda8478`, and the CPU YUV to RGBA conversion stands in for sampler-side conversion the host GL cannot do here. What *is* upstream-shaped is the narrow half: advertising NV12/NV21/IYUV/YV12 as sampler-only formats so the guest stops taking the per-plane "lowered" path (the guest half is mesa 0019 — send the pair or neither) |
| `ea0131dc` | vrend: back a decode target's planes with one IOSurface | **carry** — the IOSurface allocation and the plane-carrying EGL import are macOS-only by construction. Two halves are upstream-shaped and would go as one: binding the aux plane image *before* the texture-view branch (upstream reaches its aux bind only via the `else if`, so a host that keeps `GL_IMMUTABLE` on an EGLImage-backed planar resource gets a refused zero-layer view and a poisoned context), and applying the view's swizzle on the aux bind, which upstream omits. The plane-0-by-format signal changes GBM behaviour too — a component-format view of plane 0 now routes to `aux[0]` rather than the base texture, which is arguably more correct and is untested on a GBM host; say so when this goes up rather than letting upstream CI find it |
| `18c03b35` | vrend: drop the iovecs when the VMM's resource is destroyed | **carry, chained to `3dda8478`** — the dangling `res->iov` exists upstream too (`virgl_resource_destroy_func` never calls `detach_iov`), but nothing upstream reads it after the unref, so there is no bug to report and nothing to disclose. It became reachable only through our own guest-pixels refresh. Sending it alone would be a fix for a latent pointer with no failing case |
| `a548ef17` | venus: size host-visible allocations for the blob the guest will make of them | **carry, but the underlying gap is upstream-shaped** — a guest turns host-visible memory into a virtio-gpu blob and the guest *kernel* `PAGE_ALIGN`s it to the guest's page size, so a 1280-byte `VkDeviceMemory` arrives back as a 4096-byte blob and the export refuses it (the guest then holds a dead resource id and the client dies — measured on stock Fedora 44 with `vkcube`). We pad to 64 KiB because the host cannot ask the guest its page size; the *right* upstream fix is for venus to learn the guest page size (or for the export to tolerate a page-aligned overshoot), not a fixed pad. `LIMINA_BLOB_SIZE_ALIGN` would need renaming either way |
| `5f792568` | macOS: composite a stock guest's Vulkan window from the bytes it blits | **carry, chained to `3dda8478`** — it is the same idea (a blob with no importable fd is sampled rather than declared untypeable) reaching a second source of bytes: an SHM carrier instead of guest iovecs. Neither half is upstream-shaped as written. Backing a stripped export's memory with its own carrier only makes sense on a host with no dmabuf *and* no working export, and the host-visible gate stands in for a `vkGetMemoryHostPointerPropertiesEXT` KK does not expose. What generalises is the observation, worth an upstream issue on its own: on the merged SHM-fd macOS model, a stock guest's WSI prime-blit staging buffer reaches the compositor with no way to read it, so krunkit composites Vulkan windows black today for the same reason we did. The durable fix is guest-side (virgl enumerating modifiers, so no staging buffer is allocated at all) — see `spikes/stock-venus-black-windows/` |
| `59029c94` | virgl_resource: lock the resource table | **send-later, standalone** — the table is lock-free upstream because upstream only touches it from the entry-point thread, so there is no upstream bug to report; the lock becomes upstream-shaped only if the IOSurface-forget path (which is what put a second thread on it) goes up. `virgl_fence.c` already carries the same global-lock pattern in-tree, so the shape needs no argument |
| `e5c82bbb` | vrend: do not write a fresh resource's shadow into the guest backing on attach | **send now, standalone** — upstream `main` has the identical unconditional write-back in `vrend_pipe_resource_attach_iov`, and the guest kernel never waits for ATTACH_BACKING; the `ptr_valid` gate is host-agnostic and needs no limina context. Repro for the MR: any VA-API bitstream buffer (PIPE_BIND_CUSTOM) written right after creation |
| `4555c856` | tests: attaching backing must not overwrite the guest's bytes | **send now, with `e5c82bbb`** — the check-suite regression test for the attach write-back (fails without the gate) plus the clang `-Wno-gnu-zero-variadic-macro-arguments` the tests need under `-Werror=pedantic`. |
| `b6d4e8c3` | vrend: keep video codecs and buffers across a snapshot restore | **split** — the journal half is ours (upstream has no re-creation journal). The backend half sends standalone: `vrend_decode_create_video_{codec,buffer}` returning EINVAL when nothing was created, the rate-limited lookup-miss log in `vrend_video.c`, and the fresh-codec keyframe gate — upstream's libva backend has the same silent return-0 and would also decode a mid-stream re-created codec against an empty DPB. `tests/test_virgl_journal.c` stays with the journal. |
| `de33f490` | video: freeze the picture while a re-created codec awaits its keyframe | **split** — the `copy_picture` callback and the VideoToolbox freeze are ours (upstream has no VideoToolbox backend and no re-creation journal to create the window); the vrend implementation of a target-to-target picture copy could send standalone if upstream's libva backend ever needs one. |
| `57915fc0` | av1: code a global-motion parameter without the reference's sub term | **carry, chained to the AV1 serializer** — a fix inside our own OBU writer, which upstream does not have (its libva backend is handed OBUs by the guest and writes none). Standalone in shape, but there is nothing upstream to apply it to. |
| `03aebe20` | av1: deliver a shown frame at the wall now, claim its slot later | **carry, chained to the AV1 serializer** — the hold it fixes exists only because our OBU writer has to invent `refresh_frame_flags`; upstream's libva backend is handed OBUs and holds nothing. The one separable piece is the `ensure_session` change letting a target-less unit keep the session's pixel format, which is a real fix on its own but touches a file upstream does not have. |
| `649c4071` | av1: a switch to force the old hold-everything behaviour | **limina-only, drop** — `LIMINA_AV1_HOLD_SHOWN` exists to A/B the delivery-timing regression against one build. Fold away before any submission. |
| `98c4de94` | vrend: destroy a video plane's EGLImage instead of the null one | **carry, upstream takes it whole** — the inverted test is upstream's own, added in `a92fed82` (2022) and untouched since; nothing limina-specific. Dead on macOS (VideoToolbox delivers mapped planes, so the image is never created), live for the VA-API backend. Send it standalone, not chained to anything. |

`6adbc74f` is the allocation-side half of the fd-less classic-resource attach: without it
only `VIRGL_BIND_SCANOUT` resources got an IOSurface, so a Vulkan compositor could import
its own framebuffer but none of its clients' window buffers, and could not run on the
virgl GL default at all. Carries a `LIMINA_VREND_SHARED_IOSURFACE=0` kill switch (one more
env for the fold plan's getenv-consistency item, alongside 0011/0054).

`3738a2a4` adds `src/venus/vkr_budget.{c,h}`, a ledger of host memory allocated on the
guest's behalf, with an optional cap. Two things about it are load-bearing for anyone
auditing it later. Charging happens at each **host allocator** call site (the driver
allocation, our IOSurfaces, the mtl_shm carriers), not at the Vulkan entry point, so an
import that aliases already-charged bytes is not billed twice. And enforcement **kills the
context** instead of returning an error, because venus submits `vkAllocateMemory`
asynchronously and discards the host's `VkResult` — a fact worth re-verifying against
mesa's `vn_device_memory_alloc_simple` before reworking that decision. One more env for
the getenv-consistency item.

### Rebase onto `aafa9bd2` (2026-10-02)

The branch was rebased from `f2945d39` onto upstream `main` at `aafa9bd2`, 41 upstream commits
later; every commit on it carries a committer date of 2026-10-02 or 2026-10-03. The pre-rebase
head `017b7e15` is kept by the tag `limina-pre-rebase-2026-10-02`. Other tags on the fork that
keep earlier heads reachable: `limina-2026-08-05-pre-gkvm-rename` (`bc03f705`),
`limina-2026-08-05-pre-tip-rebase` (`96c6e99c`), `pre-zerobuf-20260807` (`b3a40d2f`),
`virgl-pre-rewrite-2026-08-31` (`24384536`), `pre-resource-lock-fixup-2026-08-31` (`9140325b`),
`limina-pre-virglrs-pin-2026-10-01` (`c59a1067`), `limina-2026-09-06-virglrs-split`
(`017b7e15`, the same commit as the pre-rebase tag) and `limina/2026-08-04-mtltexture`.

- Dropped at this rebase: *"vkr: fix the vulkan_metal.h include for the venus-protocol
  subproject"*, superseded by `aafa9bd2` (see the 2026-08-05 section).
- Added on top: `ad8e1b75` (fork code moved to upstream's `virgl_iov.h` and
  `vkr_pnext_set_next()`) and `eeb66943` (fork-only video cap bits moved to the top of the
  word).
- virglrs's `[virglrenderer-upstream]` rev `cf6c62da` is an ancestor of the base; it is that
  repo's reference-leg pin, not this ledger's base.
- 2026-10-16 spot re-check at upstream `main` `07a5bc39`: the `need` evidence cited for 0002,
  0010, 0019, 0020, 0021, 0022, 0023, 0026, 0028, 0030, 0032, 0041, 0045 and 0057, and for
  `e5c82bbb` and `98c4de94`, is still present upstream. The `checked` columns were not updated.

### Series verdict (all 58 rows researched 2026-08-03, vs tip `956b034f`)

**Upstream-now / send candidates (7):** cluster B's **0019** (seq_cst idle-check — the
flagship: a weak-memory lost-wakeup that aborts a guest in vn_relax, verbatim on tip,
sibling to the already-merged !1610); cluster C's **0022 / 0030 / 0032 / 0045** (four
guest-triggerable DoS/exhaustion fixes, all with clean `Fixes:` targets — 0030's is the
strongest, upstream landed the leaking construct itself in MR 1602 and un-guarded it);
cluster D's **0020 / 0021 / 0023 / 0057** (generic vrend correctness + macOS enablement,
precedent-backed); cluster A's **0002** (kqueue eventfd — a real macOS hole upstream will
hit). Second-wave after cleanup: **0026** (poison demotion), **0028** (watchdog
oversampling — protocol-compliant by the field's own name), **0056** (lock-free lookup),
the **0041+0043+0044+0049 relax squash** (one artifact, never 0041 alone), and imago-style
splits of 0001/0027/0028.

**The macOS-architecture carries (cluster A):** upstream chose typed Metal handles
(MR !1617's `VIRGL_RESOURCE_METAL_HEAP`) over the krunkit raw-`map_ptr` model, so
0009/0011/0013 are a coherent alternative to carry — the convergence work item is *add
VK_EXT_external_memory_metal (MTLHEAP) to KosmicKrisp, then align on !1617*, not an MR of
our diff. Premise tension logged on 0009 (our #28 called the fd double-mmap incoherent;
upstream ships it at 1770 FPS).

**Verdict inversions worth noting:** 0001's shm_open hunk landed upstream independently
(!1634, **dropped at the 2026-08-05 rebase**); 0058's poisoning branch is *ours* (from our
0039 diagnostic), not an upstream bug — folds into 0039.

### Deps-to-tip rebase executed 2026-08-05

Base `2048dfb7` → upstream tip `f2945d39`; head `09e2a0b7` → `c902f922` (69 commits);
old head tagged `limina-2026-08-05-pre-tip-rebase` (pushed); fork `main` fast-forwarded to
tip. Only 5 conflicts across the replay. Validated: build green, poke VM (seated venus,
health-check clean, user-stressed), suite 77/79 + the 2 net failures re-run green on a
quiet host (they raced the poke VM's gvproxy/port during the run).

- **0001 hunk 1 (shm_open O_CLOEXEC) DROPPED** — upstream `058959ed` (!1634) is in the base.
- **venus-protocol subproject** (131e5a72/7f9a6279 at tip): our delta to the generated
  headers is now ZERO. The journal dispatch tee moved to the four `vn_dispatch_command`
  call sites — new commit *"vkr: move the journal dispatch tee out of the generated
  protocol headers"* (validated against the snapshot gate tests before the rebase, 4/4).
  `cargo xtask vendor` materializes the revision-pinned wrap; no venus-protocol fork.
- **Maintenance 8–11** grew the dispatch table 346 → 347; the journal's class-table bound
  is now `ARRAY_SIZE(vn_dispatch_table)` (a hardcoded bound would silently strand new
  vkCmd* outside RECORDING and snapshot replay would skip them — folded into the tee-move
  commit).
- **NEW upstream fixlet, send-now candidate:** *"vkr: fix the vulkan_metal.h include for
  the venus-protocol subproject"* — upstream `b47495fb` broke their own macOS build
  (`vkr_metal_helpers.m` includes a path that no longer resolves; no macOS CI to see it).
  Trivial MR, good-citizen opener for the send queue.
  *2026-10-16 audit: superseded upstream.* matthewperiut's `aafa9bd2` (!1689, "venus: fix the
  vulkan_metal.h include on macOS", 2026-09-28) makes the same fix and is now the base; our
  commit (`c902f922` on the pre-rebase branch) was dropped at the 2026-10-02 rebase and is no
  longer on `limina`.
- The Metal-shm restructure (`8cebba7f`/`3b61c967`) is superseded inside our alloc-path
  rewrite (0006/0009 territory) — and with `8cebba7f` now in-base, 0030's fd-leak MR is
  directly demonstrable at tip, exactly as its row predicted.
- The `checked` columns below predate the rebase (tip `956b034f`); every "needed" verdict
  was implicitly re-tested by the replay (a superseded patch conflicts or empties — only
  0001's hunk did).

**Hygiene blockers before ANY freedesktop MR:** (1) ~~the `gkvm`/`GKVM_` naming~~ **DONE
2026-08-05**: history-wide `git filter-repo` rename (gkvm→limina, GKVM_→LIMINA_, subjects
reworded, Claude-Session trailers stripped, `#N`→`limina#N`); branch rewritten
bc03f705→`09e2a0b7`, old head reachable via tag `limina-2026-08-05-pre-gkvm-rename`. The
`limina`-named comments/symbols that remain in 0015/0019/0023/0027/0030/0032 still get
neutralized per-row at MR time, but nothing embarrassing is left; (2) 0011's `LIMINA_KK_RTLOG` logging
mixed into the KK winsys core (0054 caches that getenv — the fold plan must keep them
consistent). **Security: nothing blocked-on-disclosure** — the one guest-OOB (0025)
hardens limina-introduced code; every SEC row in clusters B/C is DoS/exhaustion/liveness,
so public MRs are fine.

### Triage 2026-08-03 (local, pre-browser)

Full 58-row triage from local sources only (patch headers, series README, the 2026-07-04
inventory, full-review Part III). Buckets: **33 carry-obvious** (rows filled above),
**25 research-needed** (rows left empty for the browser units) in four clusters —
A macOS/venus enablement (0001, 0002, 0009, 0010, 0011, 0013), B ring
concurrency/poison/relax-perf (0019, 0026, 0027, 0028, 0041, 0043, 0044, 0049, 0056,
0058), C guest-input robustness / exhaustion, SEC-flagged (0015, 0022, 0030, 0032,
0045), D vrend/GL tier (0020, 0021, 0023, 0057). 13 fold-candidates (named in the
`fold` column / cluster notes): 0005+0008→0004, 0014→0011, 0025→0013,
0036/0037/0040/0050→0034, 0043/0044/0049→0041, 0047/0048→0046, soft
0052/0054/0055→0051. Ordinal re-keying note: the 2026-07-04 triage used the old
24-patch ordinals (e.g. its "virgl 0013" = today's 0025, "0024" = 0019, "0025" = 0020,
"0017" = 0013); the first 24 ordinals of the current series happen to match the 07-04
series, but rows here are subject-keyed per the schema.

### Cross-cutting flags (triage 2026-08-03)

- **No `/tmp/limina-` file hooks anywhere in this series** (that pattern is
  libkrun-only). The `diag` column stays empty.
- **15 patches carry getenv-style knobs**, in three classes:
  - *pure-diag* (strip or drop before upstreaming): 0012 (LIMINA_KK_RTLOG dump), 0042
    (LIMINA_WAKE_TRACE), 0046/0047/0048 (LIMINA_RING_WAKE_PROFILE), 0050
    (VKR_JOURNAL=norecord).
  - *testability knobs / feature kill-switches* (note, don't strip): 0034
    (VKR_JOURNAL=0), 0044 + 0049 (LIMINA_RELAX_* tunables), 0058
    (LIMINA_RING_WAIT_WARN_MS — makes the fixed path testable).
  - *oracle flags* (keep, env-gated): 0017 (LIMINA_GLOBAL_SCANOUT — iosdump), 0033
    (LIMINA_GPU_TRACE_VKR).
- **0011 strip-diag blocker**: the KK winsys core patch has LIMINA_KK_RTLOG logging mixed
  into load-bearing code — must be split out before any upstream shape; and **0054
  caches that same getenv** (vkr_kk_rtlog()), so the fold plan must move the cache
  wherever 0011's diag strip lands or the series breaks.
- **Security: no disclosure blockers** (reconfirms 07-04 §B3). The one guest-OOB
  (0025) hardens limina-introduced import paths — no upstream body to disclose to; the
  rest of the SEC set (0015, 0017, 0022, 0030, 0032, 0045, 0019's latent-upstream
  liveness race) is DoS/exhaustion/info-leak class — public MRs fine.
- **Hygiene blocker for any freedesktop MR — RESOLVED 2026-08-05** (history-wide
  filter-repo rename, branch head now `09e2a0b7`): `gkvm`/`GKVM_` naming previously lived in
  subjects and env vars (0003–0008, 0011, 0012, 0016, 0024) — all renamed to
  neutral/`LIMINA_` shapes; subject keys in this ledger updated to match the rewritten
  branch.

## Unaudited commits (skeleton, 2026-10-16)

Phase-0 output of `scripts/patch-audit/check-ledger.sh --skeleton virglrenderer`: the 87 C-tree
commits in `aafa9bd2..a649897f` that had no row, grouped by theme, in branch order within each
group. The last column holds unresearched first guesses (disposition, then a sec flag) from the
2026-10-16 audit; none has been checked against upstream beyond the notes inside it.

### KosmicKrisp, IOSurface and scanout, and the classic/venus blob bridge

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `7682afc3` | vkr: advertise VK_EXT_external_memory_dma_buf alongside the injected fd extension | `src/venus/vkr_physical_device.c` |  | upstream-now candidate (supersedes mesa 0010(a)); sec: no |
| `7b872e76` | vkr: back KK scanout images with the IOSurface's MTLTexture (both ends) | `src/venus/vkr_device_memory.c`, `src/venus/vkr_image.c`, `src/venus/vkr_image.h` +2 |  | carry; sec: no |
| `b97a657d` | vkr: probe what the guest negotiates for DRM format modifiers | `src/venus/vkr_image.c` |  | carry (diag); sec: no |
| `8464bd07` | vkr: pass DRM-format-modifier creates through to KosmicKrisp verbatim | `src/venus/vkr_device.c`, `src/venus/vkr_image.c`, `src/venus/vkr_physical_device.c` +1 |  | carry; sec: no |
| `84f2294d` | vrend: never poison a context on an untypeable blob; log decode-level SET_TYPE rejects | `src/vrend/vrend_decode.c`, `src/vrend/vrend_renderer.c` |  | carry; the no-permanent-poison idea may be upstream-after-cleanup; sec: no |
| `b8a2bdd1` | vrend: adopt venus blobs' IOSurfaces as compositor textures (zero-copy import) | `src/vrend/vrend_renderer.c`, `src/vrend/vrend_winsys_egl.c`, `src/vrend/vrend_winsys_egl.h` |  | carry; sec: no |
| `734589d0` | vrend: EGLImage-backed scanout — render straight into the display IOSurface | `src/venus/vkr_metal_helpers.h`, `src/venus/vkr_metal_helpers.m`, `src/vrend/vrend_renderer.c` |  | carry; sec: no |
| `3a2b7172` | venus: attach IOSurface-backed classic virgl resources by surface id | `server/render_context.c`, `src/proxy/proxy_context.c`, `src/venus/vkr_context.c` +1 |  | carry; sec: no |
| `c7747ee5` | limina: forget a resource's IOSurface id when the surface is freed | `src/venus/vkr_metal_helpers.m`, `src/virgl_resource.c`, `src/virgl_resource.h` |  | carry (fixes a fork-only IOSurface id lifetime bug); sec: no |
| `4f2eded8` | venus: capture a scanout memory's pixels from its IOSurface, not vkMapMemory | `src/venus/vkr_device_memory.c`, `src/venus/vkr_metal_helpers.h`, `src/venus/vkr_metal_helpers.m` |  | carry; sec: no |
| `8fcc3705` | venus: put the per-import MTLTexture prints behind a switch | `src/venus/vkr_device_memory.c`, `src/venus/vkr_image.c` |  | carry (diag); sec: no |
| `d14ed091` | vrend: finish the context that rendered before a scanout is read | `harness/replay/fixtures/vrend.score`, `src/vrend/vrend_renderer.c` |  | carry (the bug is in the fork-only IOSurface sync path); sec: no |

### Trust boundary and robustness

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `1a9b057b` | vkr: also drop negative-offset and i32-overflow VkClearRects | `src/venus/vkr_command_buffer.c` |  | upstream, folded into 0045's MR; sec: yes (guest-triggerable host DoS) |
| `f311c517` | vrend: guard glTexBuffer on a sampler view over a buffer | `src/vrend/vrend_renderer.c` |  | upstream-now (upstream `main` still calls `glTexBuffer`/`glTexBufferRange` unguarded); sec: yes (guest-reachable VMM abort on a GLES 3.1 host) |
| `d28ffdd4` | vrend: refuse a transfer on a multi-plane format | `src/vrend/vrend_renderer.c` |  | carry (`yuv_planar_formats` is fork-only); sec: yes (guest-triggerable OOB in fork-introduced code) |
| `3fa8a312` | vrend, proxy: fix the two data races ThreadSanitizer reports on every boot | `src/proxy/proxy_context.c`, `src/vrend/vrend_renderer.c` |  | upstream-now (generic data-race fix in `proxy_context.c` and `vrend_renderer.c`); sec: no |
| `aa01b79d` | venus: a ring FATAL names the command that raised it | `src/venus/vkr_cs.c`, `src/venus/vkr_cs.h`, `src/venus/vkr_journal.c` |  | upstream-after-cleanup, only together with the generated-decoder shim it touches; sec: no |

### Snapshot, journal and restore

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `c68a573c` | vkr: move the journal dispatch tee out of the generated protocol headers | `src/venus/vkr_context.c`, `src/venus/vkr_journal.c`, `src/venus/vkr_journal.h` +2 |  | carry (limina snapshot model); sec: no |
| `27cd55f3` | vrend: a poisoned replay entry must not brick the context forever | `src/vrend/vrend_decode.c`, `src/vrend/vrend_renderer.c`, `src/vrend/vrend_renderer.h` |  | carry (limina snapshot model); sec: no |
| `7c30cbe1` | vrend: report content that a snapshot left behind at warn, not info | `src/vrend/vrend_renderer.c` |  | carry (limina snapshot model); sec: no |
| `c0bceba8` | vrend: warn only on content the export actually lost | `src/vrend/vrend_renderer.c` |  | carry (limina snapshot model); sec: no |
| `a4cc369f` | venus: a restored context's missing objects must not kill its ring | `src/venus/vkr_context.h`, `src/venus/vkr_cs.c`, `src/venus/vkr_cs.h` +1 |  | carry (limina snapshot model); sec: no |
| `657929e2` | venus: name the restore containment for what it is | `src/venus/vkr_context.h`, `src/venus/vkr_cs.c`, `src/venus/vkr_cs.h` +1 |  | carry (limina snapshot model); sec: no |
| `ee13315e` | venus: say what an uncapturable allocation actually is | `src/venus/vkr_device_memory.c` |  | carry (limina snapshot model); sec: no |
| `f3e0f0b4` | vrend: rebuild a context from its journal, and say what would not go back | `harness/abi/symbols.txt`, `src/virglrenderer.c`, `src/virglrenderer.h` +4 |  | carry (limina snapshot model); sec: no |

### vrend tracer and replay recorder (reference-leg instrumentation for virglrs's harness)

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `2b4b41f4` | limina: an in-memory tracer for everything vrend is asked to do | `src/meson.build`, `src/vrend/vrend_decode.c`, `src/vrend/vrend_renderer.c` +2 |  | carry; sec: no |
| `44e6ce9d` | limina: record what a replay needs -- resource creates and transfer bytes | `src/virglrenderer.c`, `src/vrend/vrend_renderer.c`, `src/vrend/vrend_trace.c` +1 |  | carry; sec: no |
| `1424a5a2` | limina: arm the tracer on first ask, and say which rejection this is | `src/vrend/vrend_renderer.c`, `src/vrend/vrend_trace.c` |  | carry; sec: no |
| `f5881d56` | limina: say which of copy_transfer3d's two rejections fired | `src/vrend/vrend_renderer.c` |  | carry; sec: no |
| `0e5e90cd` | venus: record the full ring stream, so a corpus has a beginning | `harness/README.md`, `harness/replay/vkr-record-decode.py`, `src/meson.build` +3 | DIAG | carry; sec: no |
| `957e7d46` | venus: name a recorded context by id AND generation | `harness/replay/vkr-record-decode.py`, `harness/vm/capture.sh`, `harness/vm/dump.sh` +3 |  | carry; sec: no |
| `631c26f6` | vkr_record: record the control path, not only the command stream | `harness/replay/vkr-record-decode.py`, `src/venus/vkr_record.c`, `src/venus/vkr_record.h` +1 |  | carry; sec: no |
| `7c43f2d5` | harness: a VM-free venus replayer, and three recorder bugs it found | `harness/replay/rs/.gitignore`, `harness/replay/rs/Cargo.lock`, `harness/replay/rs/Cargo.toml` +9 |  | carry; sec: no |
| `7ba688dd` | vkr_record: stamp every event with an execution clock (format v4) | `harness/replay/rs/src/corpus.rs`, `harness/replay/rs/src/main.rs`, `harness/replay/vkr-record-decode.py` +3 |  | carry; sec: no |
| `5189a678` | harness: a desktop corpus with its wallpaper, and the first pixel gate | `docs/rust-rewrite.md`, `harness/README.md`, `harness/replay/fixtures/vrend-desktop.score` +3 |  | carry; sec: no |
| `8436e6c3` | harness: let the tracer's resource log grow past a stock desktop | `src/vrend/vrend_trace.c` |  | carry; sec: no |
| `7fd005e7` | harness: capture the bytes behind an imported blob | `harness/replay/vrend-replay.c`, `src/vrend/vrend_renderer.c`, `src/vrend/vrend_renderer.h` +2 |  | carry; sec: no |
| `dba2364c` | harness: replay a blob's recorded pixels, and score them | `harness/README.md`, `harness/replay/fixtures/vrend-vkclient.score`, `harness/replay/vrend-replay.c` +2 |  | carry; sec: no |
| `23522e2b` | harness: say which blob went unfed, and when the cap bit | `harness/replay/vrend-replay.c`, `src/vrend/vrend_trace.c` |  | carry; sec: no |

### Diagnostics and logging

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `6494bb5a` | vrend: A/B levers and probes for the notification text-corruption spike | `src/vrend/vrend_renderer.c` |  | drop (spike levers); sec: no |
| `7710171a` | venus: log the clamp event, not every budget sample | `src/venus/vkr_physical_device.c` |  | carry, chained to the GPU-memory budget commits; sec: no |
| `795f0a70` | vrend: say why a command submission was refused, and allow sizing the latch | `src/virglrenderer.c`, `src/vrend/vrend_decode.c` |  | carry (diag); sec: no |
| `cc184642` | video: say what each bitstream read gave, and which client and session it fed | `src/virglrenderer.c`, `src/vrend/virgl_video_vt.c`, `src/vrend/vrend_video.c` |  | carry (diag); sec: no |
| `7a02e9de` | video: check the bytes handed to the decoder, not just their length | `src/vrend/virgl_video_vt.c` |  | carry (diag: adds a trace, no check); sec: no |

### Video: VideoToolbox, AV1, H.264, HEVC and planar decode targets

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `f13577cf` | video: a VideoToolbox backend, so macOS hosts can decode for the guest | `meson.build`, `src/meson.build`, `src/vrend/virgl_video_vt.c` |  | carry (macOS VideoToolbox backend); sec: no |
| `f8133f4f` | video: let a backend deliver a picture it cannot export as a dmabuf | `src/vrend/virgl_video.h`, `src/vrend/vrend_renderer.c`, `src/vrend/vrend_video.c` |  | carry (macOS VideoToolbox backend); sec: no |
| `d9d21a85` | vrend: H.264 decode on the VideoToolbox backend | `src/meson.build`, `src/vrend/virgl_video_h264_ps.c`, `src/vrend/virgl_video_h264_ps.h` +1 |  | carry (macOS VideoToolbox backend); sec: no |
| `7cfd58f1` | vt: decode H.264 correctly against a changing parameter set | `src/vrend/virgl_video_h264_ps.c`, `src/vrend/virgl_video_vt.c` |  | carry (macOS VideoToolbox backend); sec: no |
| `d222d528` | vt: assert hardware acceleration on the session, not the codec | `src/vrend/virgl_video_vt.c` |  | carry (macOS VideoToolbox backend); sec: no |
| `7f009188` | vrend: synthesize HEVC parameter sets for the VideoToolbox backend | `src/meson.build`, `src/vrend/virgl_video_bitstream.h`, `src/vrend/virgl_video_h264_ps.c` +2 |  | carry (macOS VideoToolbox backend); sec: no |
| `8b3d11f7` | vt: HEVC hardware decode | `src/vrend/virgl_video_vt.c` |  | carry (macOS VideoToolbox backend); sec: no |
| `7fabf760` | video: advertise AV1 and record what the guest sends, without decoding | `src/vrend/virgl_video_vt.c` |  | carry, chained to the AV1 serializer; sec: no |
| `4b57c4a5` | video: rebuild an AV1 frame header from the parsed picture descriptor | `src/vrend/virgl_video_av1_obu.c`, `src/vrend/virgl_video_av1_obu.h` |  | carry, chained to the AV1 serializer; sec: no |
| `3e1f0aaf` | video: assign AV1 reference slots ourselves, and take the sequence flags from the guest | `src/vrend/virgl_video_av1_obu.c`, `src/vrend/virgl_video_av1_obu.h` |  | carry, chained to the AV1 serializer; sec: no |
| `fe4588f5` | vrend/av1: hold hidden frames so reference slots are assigned exactly | `src/vrend/virgl_video_av1_obu.c`, `src/vrend/virgl_video_av1_obu.h` |  | carry, chained to the AV1 serializer; sec: no |
| `9d8404de` | vrend/av1: drop the state the round-robin cursor left behind | `src/vrend/virgl_video_av1_obu.c`, `src/vrend/virgl_video_av1_obu.h` |  | carry, chained to the AV1 serializer; sec: no |
| `057b8b84` | vrend/av1: hold a hidden frame only when every slot is live | `src/vrend/virgl_video_av1_obu.c` |  | carry, chained to the AV1 serializer; sec: no |
| `ede63abd` | vrend/av1: never let two temporal units share one buffer | `src/vrend/virgl_video_av1_obu.c`, `src/vrend/virgl_video_av1_obu.h` |  | carry, chained to the AV1 serializer; sec: no |
| `80be0acc` | vrend/av1: decode AV1 through the synthesized bitstream | `src/meson.build`, `src/vrend/virgl_video_av1_obu.c`, `src/vrend/virgl_video_av1_obu.h` +1 |  | carry, chained to the AV1 serializer; sec: no |
| `1187c5d9` | video: decode AV1 super-resolution in software | `config.h.meson`, `meson.build`, `src/meson.build` +3 |  | carry, chained to the AV1 serializer; sec: no |
| `b346a68e` | video: decode AV1 in software where the host has no AV1 silicon | `src/vrend/virgl_video_vt.c` |  | carry, chained to the AV1 serializer; sec: no |
| `a823b28c` | video: let the software AV1 decoder use more than one thread | `src/vrend/virgl_video_dav1d.c` |  | carry, chained to the AV1 serializer; sec: no |
| `635c518f` | video: hold a shown AV1 frame too when every reference slot is live | `src/vrend/virgl_video_av1_obu.c`, `src/vrend/virgl_video_vt.c` |  | carry, chained to the AV1 serializer; sec: no |
| `4920ae81` | video: do not advertise AV1 on a host with no AV1 silicon | `src/vrend/virgl_video_vt.c` |  | carry, chained to the AV1 serializer; sec: no |
| `a649897f` | video: advertise AV1 on a host with dav1d and no AV1 silicon | `src/vrend/virgl_video_dav1d.h`, `src/vrend/virgl_video_vt.c` |  | carry, chained to the AV1 serializer; sec: no |
| `8238b55c` | video: dump every reconstructed AV1 unit on request | `src/vrend/virgl_video_vt.c` |  | carry (diag), drop before any send; sec: no |
| `5632a388` | video: track mesa's pipe_video_profile, which the protocol carries raw | `src/gallium/include/pipe/p_video_enums.h`, `src/vrend/virgl_video.c` |  | upstream-now (wire enum drift against mesa that stock guests hit; verify upstream's `p_video_enums` copy first); sec: no |
| `6478578b` | vrend: make mesa's VA post-processing draws render | `src/vrend/vrend_renderer.c`, `src/vrend/vrend_shader.c`, `src/vrend/vrend_shader.h` |  | upstream-after-cleanup (check whether VA-API hosts are affected); sec: no |
| `d16547d7` | vrend: a sampler view's plane index is not a layer range | `src/vrend/vrend_renderer.c` |  | carry (no-GBM plane path); sec: no |
| `c2451f23` | vrend: the lowered plane path is workable, not merely undesirable | `src/vrend/vrend_formats.c` |  | carry; sec: no |
| `e36102da` | vrend: tear down a resource's plane images whether or not it has a base image | `src/venus/vkr_metal_helpers.h`, `src/venus/vkr_metal_helpers.m`, `src/vrend/vrend_renderer.c` |  | carry (fork-only planar IOSurface code); sec: possible (leak, DoS) |
| `6c56f314` | vrend: size a plane copy from the source, not from the resource | `src/vrend/vrend_video.c` |  | carry (VideoToolbox path); sec: possible (host OOB read of a CoreVideo mapping) |
| `8594b8ef` | vrend: read the bridged constants from the guest backing, not the GL buffer | `src/vrend/vrend_renderer.c` |  | carry; sec: no |
| `7c3b9eb7` | vrend: write decoded frames into guest-backed video planes | `src/virgl_hw.h`, `src/vrend/vrend_renderer.c`, `src/vrend/vrend_video.c` |  | carry; sec: no |
| `424bbaea` | vrend: a decode target that is not guest-backed is normal, not an error | `src/vrend/vrend_video.c` |  | carry; sec: no |
| `85a89645` | vrend: let the IOSurface EGL import name a plane | `src/vrend/vrend_renderer.c`, `src/vrend/vrend_winsys_egl.c`, `src/vrend/vrend_winsys_egl.h` |  | carry; sec: no |
| `1515fae8` | vrend: advertise that the host takes a composite planar decode target | `src/virgl_hw.h`, `src/vrend/vrend_renderer.c` |  | carry; sec: no |
| `e15b2391` | vrend: address a decoded plane by index, not by "one plane, one resource" | `src/vrend/vrend_renderer.c`, `src/vrend/vrend_renderer.h`, `src/vrend/vrend_video.c` |  | carry; sec: no |
| `76439a8c` | vrend: refuse a composite target we cannot back with a planar surface | `src/vrend/vrend_renderer.c` |  | carry; sec: no |
| `e8ff855e` | vrend: name the plane from the view format on a two-plane surface | `src/vrend/vrend_renderer.c` |  | carry; sec: no |
| `d71266d3` | vrend: name the surface a video plane view samples, and the write that fills it | `src/vrend/vrend_renderer.c`, `src/vrend/vrend_video.c` |  | carry; sec: no |
| `6b5ece16` | vrend: advertise as samplable only the planar formats a composite target can be backed with | `src/vrend/vrend_renderer.c` |  | carry; sec: no |
| `28156a24` | vrend: say when a planar IOSurface cannot be allocated, and how many are live | `src/venus/vkr_metal_helpers.h`, `src/venus/vkr_metal_helpers.m`, `src/vrend/vrend_renderer.c` |  | carry; sec: no |
| `11be9647` | vrend: fill a planar decode target's base texture for its composite view | `src/vrend/vrend_blitter.c`, `src/vrend/vrend_blitter.h`, `src/vrend/vrend_renderer.c` +2 |  | carry; sec: no |
| `8754cf90` | vrend: LIMINA_VIDEO_WRITEBACK_TRACE — say why a decode target was skipped | `src/vrend/vrend_video.c` |  | carry (diag); sec: no |

### Rebase maintenance

| commit | subject | files | diag | first guess (unresearched) |
|---|---|---|---|---|
| `eeb66943` | virgl_hw: move the fork-only video cap bits to the top of the word | `harness/README.md`, `src/virgl_hw.h`, `virglrs/src/vrend/caps.rs` |  | carry (virglrs's `build.rs` reads these cap bits); sec: no |
| `ad8e1b75` | vrend, venus: build the fork's code against upstream's iov and pNext helpers | `src/venus/vkr_device_memory.c`, `src/vrend/vrend_renderer.c`, `src/vrend/vrend_video.c` |  | carry; its message says it belongs folded into the commits that introduced the calls; sec: no |
