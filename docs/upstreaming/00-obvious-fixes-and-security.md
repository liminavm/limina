# Upstreaming triage — obvious fixes & security-relevant patches

**Purpose.** The two starting buckets for the upstreaming effort: the *obvious fixes*
(low-friction correctness patches an upstream maintainer merges near-as-is) and the
*security-relevant* patches (both bugs we already fixed that upstream shares, and risks
our own tree carries or introduced). "We'll start with those."

**Method.** Built on the per-patch upstreamability triage in
`docs/reviews/2026-07-01-full-review.md` (Class A/B/C/D), but re-read against the
**actual current diffs** — that review's virglrenderer table is stale by number (the
series was re-exported 26→24 patches) and libkrun 0041–0043 postdate it. Four parallel
deep reads (libkrun 43, virglrenderer 24, KK+imago+edk2, guest mesa/linux/mutter skim),
threat model = *the guest is the adversary against the host*. Rows are pinned to each
patch's **commit Subject**, not its ordinal (numbers drift). The two highest-severity
findings (KK transform-feedback OOB; libkrun blob-map overflow) were spot-verified
directly in the diffs. HEAD at authoring: `a20e91c`.

**Provenance note.** Two defects the 2026-07-01 review flagged are already resolved:
the 262 accidentally-committed `.cache/clangd/*.idx` blobs are gone (hygiene sprint),
and imago's `[patch.crates-io]` override is in place. Naming drift is **not** resolved:
20/24 virglrenderer patches still carry `gkvm`/`GKVM_` in subjects, branch instructions,
and env-var names — a hygiene blocker before any freedesktop MR.

---

## How to read this

Two orthogonal axes. A single patch can sit in both buckets — most of our
`panic!`→graceful patches are *obvious fixes* **and** *guest-DoS security fixes*.

- **Bucket A — Obvious fixes** (§1): send-now candidates, grouped by destination,
  ordered within each group by reviewer friction.
- **Bucket B — Security-relevant** (§2), split three ways because the *action* differs:
  - **B1 — Guest-triggerable bugs we FIXED** → upstream as hardening (overlaps A).
  - **B2 — Risks our tree CARRIES/INTRODUCED** → **fix in-tree first**; these are
    latent bugs *and* they block clean upstreaming. Ranked by severity.
  - **B3 — Disclosure assessment** → what (if anything) needs coordinated disclosure.

Destinations: libkrun → github.com/containers/libkrun · virglrenderer →
gitlab.freedesktop.org/virgl/virglrenderer · KosmicKrisp + guest mesa → Mesa
(gitlab.freedesktop.org/mesa/mesa) · mutter → GNOME GitLab · linux → dri-devel ·
imago → gitlab.com/hreitz/imago · edk2 bits → slp/edk2@krun-support.

---

## Portfolio census

| series | count | base | host-security surface |
|---|---|---|---|
| libkrun | 43 | upstream `07a3f40` (+3 newer) | **PRIMARY** — HVF, virtio devices, MMIO, vendored rutabaga |
| virglrenderer | 24 | `2048dfb` (~1.3.0) | **PRIMARY** — host GPU renderer, blob/IOSurface mapping, new APIs |
| kosmickrisp | 6 | Mesa main | HIGH — host Vulkan driver on Metal (venus backend) |
| imago | 2 | imago-0.2.2 | MED — virtio-blk storage backend |
| edk2 | overlay+scripts | edk2-stable202505 | LOW — boot-time firmware |
| mesa (guest) | 10 | 3 bases | guest-side; matters only via host coupling |
| linux (guest) | 4 | F44 kernel | guest-side; matters only via host coupling |
| mutter (guest) | 3 | 49.5 | guest-side; no host surface |

---

# §1 — Bucket A: Obvious fixes (send-now shortlist)

Small, self-evidently-correct correctness fixes. Ordered by ascending friction within
each destination. ✚ marks a patch that is *also* a security fix (see §2B1).

## libkrun → containers/libkrun

| patch (subject-pinned) | one-line | note |
|---|---|---|
| 0031 map `KRUN_DISPLAY_ERR_METHOD_UNSUPPORTED (-2)` in `into_rust_result!` | missing enum arm | trivial; send first |
| 0090 virtio-fs: report bytes-written as the used-ring length (was hardcoded 0) | correctness | ✚ **send early** — breaks all shares on Linux ≥7.1 (`virtio_fs_verify_response` → -EIO → FUSE_INIT fails → ECONNREFUSED at mount); upstream libkrun has the same bug. spikes/virtiofs-16k-share/ |
| 0091 virtio-gpu: negotiate EVENT_IDX + one interrupt per fence callback (was per retired descriptor) | perf, upstream-shaped | worker drains in a disable/enable-notification bracket, signals gated on `needs_notification` (fs already does this); measured −16% host wakeups / −41% guest GPU IRQs under a 60fps venus load. spikes/wakeup-probe/ |
| 0039 virtio-input worker blocks `epoll(-1)` instead of a 1 s timeout | drop idle wakeup | trivial |
| 0040 hvf vtimer: multiply before dividing so WFI timeout isn't ~1.6 % short | u128 mul-before-div | trivial arithmetic |
| 0037 virtio-input: return to `Inactive` on `reset()` so it can re-activate | one-liner, matches blk/console | fixes firmware→kernel input handoff |
| 0002 PL011 drops TX on `WouldBlock` instead of erroring per byte | best-effort drop | ✚ minor DoS/log-flood; upstream may prefer a bounded buffer |
| 0004 HVF: support 16-bit (halfword) MMIO writes ✚ | add `len==2` arm (was `panic!`) | ✚ **send early** — guest-triggerable VMM abort |
| 0005 FDT: mark PL011 node `arm,primecell` | real ttyAMA0 | depends on 0004 |
| 0014 don't `panic!` the GPU worker on scanout-readback failure ✚ | propagate `ErrUnspec` | ✚ guest-triggerable worker panic |
| 0041 rutabaga: balance the eager macOS blob map on `unref_resource` ✚ | `virgl_renderer_resource_unmap()` before unref | ✚ **host address-space exhaustion fix** (postdates the 2026-07-01 review) |

## virglrenderer → freedesktop/virglrenderer  *(strip `gkvm`/`GKVM_` naming first)*

| patch | one-line | note |
|---|---|---|
| 0041 vkr: ring relax backoff — one sleep per rung, not per iteration | perf; replaces upstream's literal `/* TODO do better */` | upstream ladder = ~55 timed wakeups per 1 ms ring idle window; fix = true exponential (≤7 sleeps), same 640 µs worst-case pickup. Measured 14.3k→6.4k total process wakeups/s under a 60 fps venus load. OS-agnostic, applies verbatim. spikes/wakeup-probe/RESULTS.md |
| 0020 vrend: fix WebRender/Firefox tile-displacement tear | orphan refill via `GL_MAP_INVALIDATE_BUFFER_BIT` | generic vrend correctness; clean |
| 0023 vrend: infer a map caching type for zink renderers | `MAP_CACHE_CACHED` when unset | fixes blob-map on zink hosts |
| 0021 vrend(macOS): wire the no-GBM surfaceless EGL winsys | small init-path branch | self-contained |
| 0019 vkr: seq_cst idle-check tail load, block the ring at idle ✚ | closes an ARM store-buffer race | ✚ genuine concurrency bug, **latent upstream** (acquire load); supersedes the reverted 2 ms timedwait |
| 0022 virgl: munmap a live VMM mapping on resource unref ✚ | unmap `res->mapped` before teardown | ✚ **host address-space exhaustion fix** |
| 0001 (shm_open hunk **only**) macOS: `shm_open` O_CLOEXEC via fcntl | fd-leak-across-exec fix | **split** — the ext-strip and `get_map_ptr` API halves are *not* obvious |

Borderline (clean, but macOS/limina-shaped → not near-as-is): 0015 (IOSurface
teardown-leak fix ✚), 0024 (query/import consistency).

## KosmicKrisp → Mesa  *(strategically most urgent — fastest-moving base)*

| patch | one-line | note |
|---|---|---|
| 0003 clamp attachment-less render-pass target size/samples to ≥1 ✚ | 0×0 renderArea → nil encoder assert | ✚ cleanest guest-DoS fix in the tree |
| 0002 lay out DRM-modifier attachment images tiled, not linear ✚ | flips one layout bool | ✚ nil-encoder DoS fix |
| 0004 give heap-less host-imported tiled image planes a private Metal heap ✚ | `external_memory_host` correctness | ✚ SIGABRT DoS fix (wgpu guests) |
| 0006 advertise `VK_EXT_depth_clip_enable` | impl already in tree | reword the "lifts GL 3.2" overclaim |
| 0005 advertise `VK_EXT_custom_border_color` | impl already in tree | as individual MRs |

**Do NOT** send KK 0001 (monolithic; split required) until its transform-feedback OOB
is clamped — see §2B, top item.

## guest mesa → Mesa

> **Not security fixes — do not confuse with the panic→graceful class.** These run
> *inside the guest*; a NULL deref here crashes a guest process (gnome-shell, a
> `util_queue` thread), which is the guest harming its own graphics stack — **no
> guest→host boundary is crossed** (contrast libkrun 0028, which does). And their
> trigger is largely limina-specific: the NULL paths fire *because* a Vulkan/WSI
> extension is missing, and it's missing because our venus-on-KK/Metal host stack
> doesn't expose it (0003's own message pins it to venus-on-MoltenVK's absent
> `KHR_external_semaphore_fd`); a stock desktop driver advertises these and never hits
> the path. So: clean defensive hardening upstream will take, **not** exploitable, no ✚.

| patch | one-line | note |
|---|---|---|
| 0002 fbobject: guard NULL `pipe_resource` in `do_discard_framebuffer` | NULL check | clean |
| 0003 zink: gate dmabuf-semaphore *import* on `have_KHR_external_semaphore_fd` | NULL fn-ptr guard | clean |
| 0004 same, export side | NULL fn-ptr guard | clean |
| 0006 zink/kopper: guard missing surface extensions | NULL surface-create guard | clean |
| 0011 venus WSI: drop 16-bit-unorm swapchain formats | match lavapipe (wgpu ghost-UI) | lavapipe precedent |
| 0013 venus: pin ICD with `RTLD_NODELETE` for TLS destructor | prevent post-dlclose SIGSEGV | upstream `main` still buggy |
| 0012 venus: degrade to stub instance when ring setup fails | stock-tier Vulkan floor | resilience; motivated by host page-size coupling (see §3) |
| 0001 zink nullDescriptor emulation | = Mesa **MR !37115** | **track the MR**, drop when it lands |

## mutter → GNOME GitLab

| patch | one-line | note |
|---|---|---|
| 0002 x11: survive frames-client launch failure | NULL guard early-return | trivially mergeable |
| 0001 cogl: degrade stencil clip when FB has no stencil | scissor-extents fallback | **strip live `LIMINA-CULL`/`LIMINA-CLIP` `fprintf` debug blocks first** (clutter-actor.c, clutter-stage.c, meta-stage-impl.c) — NOT sendable as-is |

## linux → dri-devel  *(verify-first, not "obvious")*

None qualify as obvious fixes as-is. `provision/f44/README.md:71` flags all four as
"may already be upstream in F44's kernel" — **check drm-misc-next before sending.**
0001/0003 are the closest (near-ready per README) but are feature-shaped and carry host
couplings (§3). 0004 is a fix in intent but upstream would want a negotiated granule,
not a hardcoded 16 KiB.

## Not upstreamable (context, not a to-do)

imago 0002 (build-graph pin), mutter 0003 (ext-data-control-v1 — GNOME rejected on
privacy grounds, mutter#524; keep downstream forever), edk2 overlay (vendored-verbatim
fork glue), and the log-taste / env-gated-policy patches.

---

# §2 — Bucket B: Security-relevant

## B1 — Guest-triggerable bugs we FIXED (upstream as hardening)

The dominant pattern across the portfolio: **a guest-reachable code path that
`panic!`/`unwrap`/`.expect()`ed the host worker or VMM → a guest→host DoS**, which we
turned graceful. This is a *class*, and its strongest single upstream story is libkrun
0028 (below). All are MED (host-process DoS, no memory corruption) unless noted.

**libkrun — panic/abort → graceful (guest-DoS fixes):**
- **0028** unknown sysreg / exception-class / PSCI fn / bad MMIO size → clean teardown
  instead of `SIGABRT`. **The broadest one, and it is reachable on *stock upstream*
  libkrun** (the `-` lines are the `07a3f40` base): `handle_psci_request`'s
  `val => panic!` fires on any PSCI/SMC function libkrun doesn't model
  (`PSCI_FEATURES`, `AFFINITY_INFO`, `SYSTEM_RESET2`, a vendor SMC) — **one guest
  `smc` instruction, no limina device or config required**; real guests probe
  `PSCI_FEATURES`. A genuine guest→host DoS. RED-first tested (`hvf_graceful.rs`).
  *This is the flagship security-fix upstream.* Ceiling is low, though: DoS-only
  (no corruption/leak/escape), blast radius = the guest's own single VMM process
  (one VM per process, no cross-tenant victim), and it only *matters* under an
  untrusted-guest deployment — libkrun's mainline container model treats the guest as
  trusted. This is the LOW–MEDIUM "guest can abort the VMM" CVE class, not escape.
  Self-contained DoS → **no embargo needed** (public PR is fine; maintainers may assign
  a CVE). Before claiming novelty, **check libkrun HEAD** — they may already have
  converted some of these panics since `07a3f40`.
- **0024** `transfer_read` (TRANSFER_FROM_HOST_3D) delegated instead of `panic!` — any
  `glReadPixels`/WebGL readback on a stock vrend guest aborted the worker + hung the guest.
- **0004** halfword MMIO write (was `panic!`). **0008** cursor-queue commands (was
  `panic!`). **0007** stop the worker on device reset (guest re-init busy-looped a freed
  ring → CPU pin). **0010** coexist fence routing drops an `.expect()` on renderer-init
  failure. **0013** SET_SCANOUT_BLOB (was `panic!` on mutter's first page-flip).
  **0014** scanout-readback failure (was `unwrap`).
- **0041 / virgl 0022 / virgl 0015** — resource-exhaustion class: guest context/window
  churn leaked host mmaps / IOSurfaces until ENOMEM collapsed the session. Now balanced
  on unref/teardown. (0041 & 0022 are also in Bucket A.)
- **0035** dirty-reset drops leaked ctx/resource ids that otherwise collided with the
  re-init guest's reused ids (UAF/id-collision hardening).

**virglrenderer:**
- **0019** seq_cst idle-check — store-buffer race → missed-notify busy-poll → `vn_relax`
  abort (self-DoS liveness); race is latent upstream. (Also Bucket A.)
- **0017** non-global scanout by default (handed to the supervisor by Mach port) —
  **closes a same-user info-leak**: any process could read the guest screen via
  `iosdump <global-id>`. MED.
- **0006** zero-init `virgl_context_blob` — prevents a stack-garbage `iosurface_id` being
  presented / looked up (info-leak/crash). MED. *(embedded in a FEATURE patch)*

**KosmicKrisp:** 0003, 0002, 0004 — guest-triggerable nil-encoder / plane-bind aborts
(all in Bucket A). 0001 additionally adds two missing host-pointer-import crash guards
(`metal_info` nil-deref; nil `newBufferWithBytesNoCopy` AGX segfault) — keep, but see B2.

**imago:** 0001 — guest `discard` reaching EOF (e.g. `mkfs` tail-discard) truncated the
backing file → shrunk virtio-blk capacity → unmountable guest fs after reboot (a
guest-triggerable *brick* / availability DoS). Fix reroutes to `F_PUNCHHOLE`. The *bug
report + reproducer* is the upstreamable artifact; the patch itself blanket-disables a
legit optimization (see B2 / reshape note).

## B2 — Risks our tree CARRIES or INTRODUCED (fix in-tree first)

These are latent bugs **and** they block clean upstreaming. Ranked by severity.

1. **HIGH — KosmicKrisp: transform-feedback OOB write (our own code). ✅ FIXED
   2026-07-04 — `patches/kosmickrisp/0007`, RED/GREEN verified.**
   `kk_CmdBindTransformFeedbackBuffersEXT` computes `idx = firstBinding + i` and writes
   `gfx->xfb.buf[idx].gpu_base = range.addr` (patch lines 1092/1101); likewise
   `kk_CmdBeginTransformFeedbackEXT` with `idx = firstCounterBuffer + i` (1124/1131) —
   into 4-element arrays (`maxTransformFeedbackBuffers = 4`, line 1698), **with no clamp
   between the index and the store** (the bounds check at line 1262 guards
   offset-*within*-buffer, not the array index). A non-conformant guest sending
   `firstBinding > 3` writes a guest-controlled buffer address/size into host memory
   adjacent to `kk_cmd_buffer` → **memory corruption / worker crash**. *Verified in the
   diff.* Fix: reject / clamp `firstBinding + bindingCount > 4` (and the counter-buffer
   equivalent) before the loop. Second, lower-confidence vector in the same patch: the
   XFB NIR lowering emits VS global stores at `xfb_base[b] + vertex_id*stride` with no
   host-side buffer-extent clamp → GPU-side OOB on large draws.
   *Upstream KK has no transform-feedback, so this is limina-introduced — no disclosure
   embargo, just fix it. It also gates upstreaming KK 0001 at all.*
   **Fixed** in `patches/kosmickrisp/0007` (clamp `idx >= ARRAY_SIZE(gfx->xfb.buf)`,
   `break` — idx is monotonic — in all three handlers). RED/GREEN verified with
   `spikes/venus-draw-probe/xfb-oob-probe.c`: pre-fix SIGBUS on a write 1.34 GB past the
   array, fixed survives (`xfb-oob-RESULTS.md`). Two verification traps worth noting for
   the rest of this bucket: the host Vulkan **loader doesn't dispatch these commands to
   KK** in a standalone process (an early probe was a silent no-op — call KK's ICD
   directly), and KK records via **vk_cmd_queue** so the handler (and the OOB) runs at
   **submit/replay**, not record. Both were caught only by instrumenting KK itself.

2. **MED (escape-adjacent) — libkrun 0012/0013: blob-map `offset+size` overflow before
   bounds check. ✅ FIXED 2026-07-04 — `patches/libkrun/0044`, RED/GREEN unit-tested.** `resource_map_blob` does `if offset + resource.size > shm_region.size`
   and then `guest_addr = shm_region.guest_addr + offset` — both **plain unchecked `u64`
   adds on a guest-controlled `offset`** (`ResourceMapBlob`). *Verified in the diff.* An
   `offset` chosen to wrap bypasses the SHM-window check and feeds a bogus address to
   `hv_vm_map`. 0012 broadens this from Apple-gated blobs to *every venus blob*. HVF
   constrains the wrapped address/size (why it's MED not CRITICAL), but this is the most
   escape-adjacent item in the tree. Fix: `checked_add`/`checked_mul`; **confirm
   exploitability before touching this path in a public libkrun PR** (see B3).

3. **MED — virglrenderer 0013: host-pointer import never clamps `allocationSize` down.
   ✅ FIXED 2026-07-04 — clamp to backing `span`/`alloc_size` at both import sites in
   `vkr_device_memory.c` (happy-path unchanged; the fragile `u.data >= 0x10000`
   ptr-vs-fd heuristic is left documented, not yet reworked).**
   The `gkvm_res_import` block does `if allocationSize <= span { allocationSize = span }`
   — bumps *up* to the backing span but never clamps a *larger* guest-declared
   `allocationSize` *down*. A guest importing a small window-buffer IOSurface while
   declaring a huge size gets a `VK_EXT_external_memory_host` import larger than its
   backing → potential host OOB under KK/Metal. Fix: clamp `allocationSize` to `span`.
   Related: the `u.data >= 0x10000` heuristic distinguishing an mmap pointer from an fd
   number (reused in 0013 and 0024) is a fragile type-confusion guard — document/harden.

4. **MED — libkrun 0033: balloon FRQ `get_host_address(desc.addr).unwrap()`.
   ✅ FIXED 2026-07-04 — `patches/libkrun/0045`, skip out-of-range descriptors.** A
   carried, pre-existing worker panic on an out-of-region **guest-controlled** FRQ
   descriptor address. (The 16 KiB/4 KiB coalescing math itself is sound and reclaim is
   self-harm-only.) Fix: `?`/`continue`, matching 0034's handling.

5. **LOW — libkrun 0027: shipped DIAG hooks on the render path.** `flush_resource` reads
   `/tmp/limina-readback-delay` (injects a `sleep`) and dumps raw guest framebuffers to
   `/tmp/limina-staging-*.raw` when `/tmp/limina-dump-staging` exists — local-user
   latency injection + guest-framebuffer capture to a world-adjacent path. Strip before
   upstream (the de-shear blit itself is correct and well-bounded). Same class: 0025's
   `/tmp/limina-*` hooks.

6. **LOW — libkrun 0030: error-masking.** Missing ctx/resource on coexist attach/detach
   → `Ok(())`. Benign today (bookkeeping only, no host state touched on the missing
   path), but it is exactly the masking pattern the threat model flags; upstream will
   want it gated on coexist mode, not unconditional.

7. **LOW — libkrun 0006: spec-enforcement leniency.** QueueReady with size 0 snaps to
   `max_size` (QEMU-compat for EDK2's spec violation). Bounded by the configured
   `max_size`, but it weakens virtio-mmio spec enforcement — note when upstreaming.

*New guest→host surfaces added, reviewed clean:* libkrun 0042 (virtio-i2c SBS battery —
chain length + slice bounds validated; a large guest read descriptor forces a
proportional but guest-bounded host alloc), 0034 (balloon control — `read_obj` bounds-
checked, `write_config` now properly `checked_add`-guarded).

## B3 — Disclosure assessment

**No item is currently embargo-grade.** No finding rises to a *confirmed* guest→host
memory-corruption/escape against an upstream project:

- The **KK 0001 XFB OOB (HIGH)** is limina-introduced code absent from upstream KK →
  fix it in-tree, no coordinated disclosure.
- The **libkrun 0012/0013 overflow (MED)** is the only escape-adjacent item that also
  touches an upstream-shared path. **Action:** confirm exploitability (does a wrapped
  `offset` actually reach `hv_vm_map` with a mapping that lands outside the SHM window,
  or does HVF/alignment reject it first?). *If it proves exploitable against upstream
  libkrun's `resource_map_blob`, it becomes CVE-class → coordinated disclosure to
  containers/libkrun (private report + embargo), NOT a public PR.* Until then, land the
  `checked_add` hardening in-tree quietly and hold the public patch.
- libkrun 0028 and virgl 0019 fix bugs latent upstream but they are **DoS/liveness**,
  not memory-safety — normal public upstreaming is fine.

---

# §3 — Cross-cutting notes for the upstreaming session

- **The panic→graceful DoS class is one story.** libkrun 0004/0007/0008/0010/0013/0014/
  0024/0028 + KK 0002/0003/0004 + virgl 0006/0019 all convert guest-reachable aborts
  into graceful handling. Framing them (esp. libkrun 0028) as "a malicious/buggy guest
  must not be able to crash the VMM" is a compelling, reviewer-friendly security
  narrative — lead the libkrun engagement with it.
- **Hygiene blocker — virglrenderer naming drift.** 20/24 patches still carry
  `gkvm`/`GKVM_` in subjects, README branch instructions, and env-var names. Rename to
  neutral (or `LIMINA_`) before any freedesktop MR; a maintainer won't take `GKVM_*`.
- **Force-advertise coupling (mechanism/policy inversion).** guest `mesa 0010`
  force-advertises `EXT_image_drm_format_modifier`/`queue_family_foreign`/dmabuf-fd that
  the host renderer doesn't natively expose, and host `kk 0002` exists to survive the
  images that lie produces. Neither is upstreamable without the other's context; do not
  send either in the obvious-fixes wave. Same for `mesa 0009`'s INVALID→LINEAR / `block_16f`
  transport policy (the deep-copy hunk alone is clean).
- **The blob-map alignment contract spans three repos.** guest `linux 0004` (align
  allocation *start* to 16 KiB) + host `libkrun 0043` (round map *size* up to host page)
  + `virgl 0023` are one coupled fix for the 16 KiB-host / 4 KiB-guest mismatch; and
  guest `mesa 0012` (stub-on-ring-failure) is the stock-tier floor that exists *because*
  of it. Sequence together; none is a standalone "obvious fix."
- **Present-pacing spans host+guest.** guest `linux 0001` (fence on blob-scanout flush)
  only means something if the host holds it (libkrun 0017/0018, default-off env gating).
  This is the Wave-3 "honest flip pacing" RFC, not an obvious fix.

---

# §4 — Recommended immediate actions

**Fix-in-tree first (before any upstreaming), in priority order:**
1. Clamp the **KK 0001 transform-feedback** array index (HIGH). RED-first test with a
   `firstBinding > 3` command.
2. `checked_add`/`checked_mul` the **libkrun 0012/0013** blob-map bounds check, and
   *decide the disclosure question* (B3) before any public libkrun PR on that path.
3. Clamp **virgl 0013** `allocationSize` to the backing span (MED).
4. `?`/`continue` the **libkrun 0033** balloon `get_host_address` unwrap (MED).
5. Strip the **libkrun 0027/0025** `/tmp/limina-*` DIAG hooks (LOW; also unblocks those
   patches for Wave 2).

**Then fire the obvious-fixes wave**, in this order of least resistance:
1. **KosmicKrisp 0003/0002/0004/0006/0005** as individual Mesa MRs — most urgent
   (fastest-moving base; every landed patch is one fewer conflict per Mesa bump).
2. **libkrun trivia**: 0031, 0039, 0040, 0037, then 0004(+0005), 0014, 0041 — lead the
   libkrun conversation with 0028 as the flagship DoS-hardening patch.
3. **guest mesa** 0002/0003/0004/0006/0011/0013 (+ track MR !37115 for 0001).
4. **virglrenderer** 0020/0023/0021/0019/0022 + the split-out shm_open hunk — *after*
   the `gkvm→` rename.
5. **mutter** 0002 (and 0001 once its debug `fprintf`s are stripped).
6. **linux** 0001/0003 — only after checking they aren't already in drm-misc-next.

**Add an upstreaming tracker** (per-patch: destination, MR/PR link, status) — the numbers
already drift; track by Subject line.
