# linux — patch-audit ledger

**Fork model since 2026-08-03.** There is no `patches/linux/` series any more: our kernel changes
are commits on **`github.com/liminavm/linux`** branch **`limina`**, base **`v7.1.8`**, pinned in
`third_party/manifest.toml`. The fork's parent is **`gregkh/linux`** (the stable-tree mirror) —
`torvalds/linux` has no stable point-release tags. Regenerate the series as a build artifact with
`scripts/export-linux-patches.sh` (`git format-patch base..rev`).

2 commits. Schema + protocol: `README.md`. Rows are keyed by SUBJECT; ordinals are informational
and drift on re-export.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | drm/virtio: expose per-scanout rects as suggested connector offsets | `drivers/gpu/drm/virtio/virtgpu_display.c`, `virtgpu_vq.c`, `virtgpu_drv.h` |  | **needed** — no driver reads the rect position; qxl is the only precedent | `v7.1.8` `virtgpu_display.c` (read 2026-08-23: still size-only, no suggested_x/y, no `hotplug_mode_update`) | none-yet | none-yet | no | standalone | guest-enhanced | **upstream-now** | the guest half of the host→guest arrangement relay; mutter gates the offsets on `hotplug_mode_update`, so the two halves must go together |
| 0002 | drm/virtio: type and attach PRIME-imported dmabufs | `drivers/gpu/drm/virtio/virtgpu_prime.c` |  | **needed** — `bo->blob_mem` is still unset on the import path and `virtgpu_dma_buf_funcs` still carries only `.free` | `v7.1.8` `virtgpu_prime.c` (read 2026-08-23) | none-yet | none-yet | no | standalone | guest-enhanced (the bug hits any guest whose glupload picks the DirectDmabuf uploader; 16 KiB pages are what make that the default here) | **upstream-now** | a plain driver bug with a self-contained fix and a measured symptom — the cleanest upstream candidate we carry; see the finding below |

## Left the series

| subject | where it went | why |
|---|---|---|
| drm/virtio: align host-visible window allocations to 16 KiB | `guest/virtio-gpu-dkms/0001-align-host-visible-allocations-to-16-KiB.patch` | **No-op on the 16k enhanced kernel** (PAGE_ALIGN is already 16 KiB) and **known-rejected upstream**, so it has no place on a branch whose purpose is upstreamable delta. Its one real deployment is the stock-4k tier via the DKMS module. Full rationale + the (not yet reachable) F_BLOB_ALIGNMENT exit: `guest/virtio-gpu-dkms/README.md`. |
| virtio_balloon: stop free-page reporting across suspend/resume | replaced by the `0b45f69` backport (row 0001) | Upstream root-caused the same UAF independently and chose the core fix over our driver-side approach. |
| drm/virtio: widen the primary plane format list | **dropped 2026-08-04** (preserved under tag `limina/2026-08-04-modifiers`) | Punted with 0003 to the future hardware-planes work — see "Why these two left" below. Never priced: the rig drives the compositor's own present path, not fullscreen *client* direct scanout, which is what this patch is actually for. |
| drm/virtio: advertise DRM_FORMAT_MOD_LINEAR on planes | **dropped 2026-08-04** (preserved under tag `limina/2026-08-04-modifiers`) | The one failure it appeared to prevent turned out to be a bug in our own Vulkan compositor, fixable in one line; stock mutter never needed it. See "Why these two left" below. |
| mm/page_reporting: use system_freezable_wq to fix UAF during suspend | **dropped on rebase 2026-08-23** (base moved `v7.1.6` → `v7.1.8`) | It was a cherry-pick of upstream `0b45f69`, carried only until the stable base caught up; `v7.1.8`'s `mm/page_reporting.c` queues on `system_freezable_wq`. The watch items it left behind are in Findings. |
| drm/virtio: fence RESOURCE_FLUSH for host3d blob scanout | **dropped 2026-08-04** (was on the branch for one day, as `bde37a06ba4d`; preserved under tag `limina/2026-08-04`) | Rewritten, shipped, then **measured**: it costs 86% of frames on the async-scanout rig. `virtio_gpu_resource_flush` *blocks* on the fence in `commit_tail`, and our host does not signal until the CA latch — so fencing blob scanout serialises every commit behind a host vblank. Not a bad idea badly implemented: a bad *place*. See Findings. |

## Findings

### Why the two DRM format/modifier patches left the branch (2026-08-04)

**Decision: dropped, punted to the future additional-"hardware"-planes work.** Recoverable from tag
`limina/2026-08-04-modifiers`. The sections below on 0002 and 0003 are kept as the research record —
the history, the upstream rationale, and the measurements are all still accurate — but the
dispositions in them are superseded by this.

Three things settled it, in order of weight:

1. **We were solving the wrong scanout.** The word does double duty. What limina actually cares
   about is getting the *VM's output onto the Mac's display* — the IOSurface/present path. These
   patches address *guest KMS plane* scanout: a client's buffer reaching the guest's primary plane.
   Both are "scanout"; only the first was ever the goal. That conflation is what put this line of
   work on the roadmap at all, and naming it is the durable lesson.
2. **The one hard failure was our own bug.** Arm D looked damning — every frame failing on
   `DRM_FORMAT_MOD_INVALID` — but that was the Vulkan renderer in *our* compositor refusing a
   modifier label it could have accepted. One line fixed it
   (`spikes/modifier-necessity/niri-mod-invalid-linear-fallback.patch`) and arm E came back at
   **1.06% missed** vs arm C's 1.33%: no pacing cost at all. Stock mutter never needed the patch;
   it renders through GL/EGL and never names a scanout modifier. Unpatched compositors broadly
   work in VMs today, which is the same evidence from the other direction.
3. **0002 was never priced, and this rig cannot price it.** The workload drives the compositor's
   own present path, never a fullscreen client's buffer going to the plane — which is exactly what
   the format widening is for. Arm E's +29% median draws / +13% median GPU is a *hint* that
   something stopped direct-scanning-out, but it moves two variables at once and settles nothing.
   Rather than carry an unmeasured patch, it goes back in the box until there is a reason and an
   oracle for it.

The natural home for both is the planned **additional hardware planes** work, where
device-advertised plane formats and host-negotiated modifiers are the real design (the same M15
protocol extension both rubrics already named as their exit). Re-derive them there against a
fullscreen-client oracle; do not simply restore the tag.

**Method lesson, and it is not the usual one.** Nothing here was wrong on the facts — the upstream
archaeology was right, the mechanism reading was right, the measurements reproduce. The failure was
upstream of all of it: a mis-scoped premise about which problem was being solved, which no amount of
careful verification *inside* the thread could catch. Ask what the patch buys the product, not only
whether the patch does what it says.

### The blob-scanout fence — absent by accident, then dropped on evidence (2026-08-03/04)

Two-act story. Act one found the patch had never been applying; act two measured what it does and
took it off the branch. **Current disposition: not carried.** The rest of this section is kept
because the mechanism is load-bearing for the M15 present work and for anyone tempted to re-add it.

#### Act two — it costs 86% of frames (2026-08-04, decisive)

Isolated on the gnome-shell-rs rig (`docs/perf/gsrs-local-rig.md`): same disk, same three windows,
`NIRI_VK_ASYNC_SCANOUT=1`, heavy profile, three kernels flipped with `grubby`.

| arm | kernel | frames | missed vblanks | miss rate |
|---|---|---|---|---|
| A | `7.1.4-limina16k` (fence silently absent) | 8730 | 77 | **0.9%** |
| B | `7.1.6-limina16k` (fence present) | 4615 | 3983 | **86.3%** |
| C | `7.1.6-limina16knf` (B minus the fence commit) | 8674 | 108 | **1.2%** |

Arm C is arm B's tree with exactly that commit reverted, so the stable delta and the two format
patches are held fixed: the fence alone moves 86.3% → 1.2%. Workload comparability held across all
three arms (median 41 elements, 35 draws); GPU time was *not* the difference (median 3.00 / 2.77 /
3.74 ms — arm B was the fastest on the GPU and still missed nearly every frame).

Mechanism, read from the source rather than inferred: `virtio_gpu_resource_flush()` calls
`dma_fence_wait_timeout(..., 50ms)` **synchronously**, and on the atomic path that runs inside
`commit_tail`. Our host does not signal the flush fence until the CoreAnimation latch, ~1 vblank
later. So fencing blob scanout does not *pace* the compositor, it *serialises* it: every commit
blocks a full refresh, and a compositor that queued its next frame ahead of time has already
missed by the time the wait returns. Arm B's "queued 13.05ms early … presented 16.67ms late" lines
are exactly that shape.

The idea it was meant to serve — real host-derived present feedback instead of
`drm_atomic_helper_fake_vblank()` — is still right, and the enhanced-tier rubric below still
describes something worth having. What is wrong is delivering it as a blocking wait in the commit
path. A redesign would hand the host fence to the atomic commit as an **out-fence** (the flip event
fires when the host latches, nothing blocks) — that is a real design task, not a rewrite of two
conditions, and it belongs with the M15 present work rather than as a carried kernel patch.

**Method lesson:** we shipped this on inference — the code plainly "should" pace better — and the
first oracle able to see it said the opposite by two orders of magnitude. A patch that has never
been observed doing its job is a hypothesis, however plausible its diff.

#### Act one — the patch had been silently absent since the 7.1.x bump (2026-08-03)

The old `patches/linux/0001` stopped applying when upstream refactored `prepare_fb` to the
per-plane-state `vgplane_st->fence` shape, and the build script's tolerant apply printed
`SKIP 0001-...` into logs nobody read. Every kernel we have shipped since therefore fences
dumb-buffer and cross-device-imported scanout, but **not** host3d blob scanout — our entire
zero-copy venus path. Read directly from `v7.1.4` and `v7.1.6` sources:

```c
if (!bo || (plane->type == DRM_PLANE_TYPE_PRIMARY && !bo->guest_blob))
        return 0;                              /* host3d blob exits here */
obj = new_state->fb->obj[0];
if (bo->dumb || drm_gem_is_imported(obj)) { vgplane_st->fence = ...; }
```

`drm_gem_is_imported()` is `!!obj->import_attach` — cross-device only — and the self-import path
(`virtgpu_prime.c`) returns the original GEM object, so a dmabuf exported and re-imported on the
same virtio-gpu device (the ordinary topology for a compositor scanning out its own render target)
never registers as imported either. Downstream the plumbing is intact: `virtio_gpu_resource_flush`
still waits on `vgplane_st->fence` when present and `cleanup_fb` still puts it — only the
allocation was missing, so the rewrite is two conditions.

Consequence beyond the fix: measured pacing on the enhanced stack came from the venus WSI fence
chain (mesa 0009/0010 + libkrun 0017/0018), never from KMS. Any compositor using the atomic flip
event as its frame clock for direct-scanout FBs has been running against `fake_vblank`
completions with no host in the loop — relevant to the gnome-shell-rs scanout work, which is also
the first oracle capable of showing this patch doing anything.

**Method lesson (added to the README protocol):** a tolerant apply is a silent-failure machine.
Under the fork model a patch is on the branch or it is not, which is why this was found at all.

Prior art (lore): Kasireddy 2021 "default synchronization mechanism for blobs" (landed ~5.15, the
dumb-path template this extends); Kasireddy's `DRM_CAP_DEFERRED_OUT_FENCE` RFC (stalled on
*new-uapi* objections from Vetter — the pitch must stay mechanism-only); Kasireddy 2024 "Import
scanout buffers from other devices" (whose fencing an earlier reading of ours mistook for full
coverage).

**Enhanced-tier rubric (of the dropped patch — kept as the spec a redesign would have to meet):** (a) host-paced flip completion for venus blob scanout — the host fence
gates the guest's flip event (no stale-frame reuse races); (b) stock guest boots and renders;
blob flushes unfenced → compositor free-runs against the presenter (occasional stale-frame
flicker on the zero-copy path); (c) host-side-only pacing proven insufficient — the host cannot
delay a completion the guest never waits on; (d) exit = rebased drm-misc submission.

### 0002 — the format restriction is a 2018 big-endian side note, not a 2017 decision (corrected 2026-08-03)

Earlier revisions of this ledger cited a 2017 kraxel series as the origin of the XRGB8888-only
primary plane. Searching the actual history shows no such commit. The real one is
**`42fd9e6c29b3`** ("drm/virtio: fix DRM_FORMAT_* handling", Gerd Hoffmann, **2018-09-21**), whose
subject is big-endian correctness; the narrowing is a paragraph inside it:

> While wading through the code I've noticed we have a little issue in virtio: We attach a format
> to the bo when it is created (DRM_IOCTL_MODE_CREATE_DUMB), not when we map it as framebuffer
> (DRM_IOCTL_MODE_ADDFB). Easy way out: Support a single format only.

It removed an eight-entry list containing `ARGB8888`, `XBGR8888` and `ABGR8888` among others — so
our patch is a partial restoration, and the create-time binding it worked around is long fixed.
Jocelyn Falempe's 2024 two-format widening carried Gerd's Reviewed-by ("Now that it's fixed, it
can support both") but never merged, so the restriction survives by inertia.

Review questions to pre-answer in the submission: opaque-alpha semantics for ARGB on the bottom
plane (may want virtio-gpu spec text: "scanout alpha is ignored"), and host capability variance
for XBGR/ABGR (the durable answer is device-advertised plane formats — the virtio protocol
extension planned with M15 overlay planes; the hardcode is sellable meanwhile as restoration).

**Method lesson:** a citation is a claim. This one survived several ledger revisions because it
was plausible and nobody ran `git log`.

**Enhanced-tier rubric:** (a) compositors' native ARGB/RGBA-order buffers hit the primary plane →
true zero-copy direct scanout; (b) stock guest fully functional via the composited/swizzled path —
correct output, extra guest GPU cost per frame; (c) host-side alternative impossible — the
rejection happens in the guest's DRM atomic check before anything reaches the host (the host
already displays RGBA-order IOSurfaces natively, `spikes/scanout-modifiers/`); (d) exit = upstream
the folded patch and/or land device-advertised formats.

### 0003 — LINEAR advertisement was deliberately *removed* upstream, not merely absent

Commit **`85faca8ca0f6`** ("drm/virtio: set fb_modifiers_not_supported", Chia-I Wu, 2022, R-b
Daniel Stone, pushed by Gerd Hoffmann) opted virtio-gpu out of the drm core's implicit LINEAR
advertisement. The reasoning: the guest cannot vouch for a layout the host chooses — on a virgl
host the resource layout is opaque and may be tiled, so a guest-side LINEAR claim is a lie that
breaks importers (it broke Chrome ozone/drm scanout). Our patch reverts exactly that: correct for
limina, whose scanout path is linear end to end; incorrect for virtio-gpu in general.

Upstream's stated bar (Gerd, 2021, reviewing the stalled set_scanout_blob-modifier series; same
direction in Julia Zhang's 2023-24 `RESOURCE_GET_LAYOUT` proposals): virtio feature flag +
host-provided modifier list (capset) + spec text. Sending plain LINEAR to dri-devel would get the
2022 rationale quoted back. So: **carry**, and fold the exit into the M15
device-advertised-plane-formats protocol extension — one protocol story retires this and the
format-widening patch's host-variance caveat.

**Not a hard dependency of our own compositor.** The kernel gate matters because compositors gate
direct scanout on the buffer's explicit modifier appearing in the plane's `IN_FORMATS`. A
compositor we control can instead drop the modifier and use legacy `ADDFB2` without
`DRM_MODE_FB_MODIFIERS`, which works with `fb_modifiers_not_supported` set. The patch stays
because it is what keeps *stock* mutter/niri guests on the zero-copy path.

**Enhanced-tier rubric:** (a) completes the zero-copy chain — IN_FORMATS LINEAR lets
mutter/niri engage direct scanout at all; (b) ~~stock guest composites fullscreen — correct, one
extra guest-GPU pass per frame~~ **FALSIFIED 2026-08-04 for a Vulkan-renderer compositor — see
below**; (c) no host-side alternative — the gate is the guest kernel's
plane property enumeration, which the host cannot inject (patching N compositors would be
strictly worse than 1 driver); (d) exit = host-negotiated modifiers with M15.

**Measured 2026-08-04 (`spikes/modifier-necessity/`): there is no graceful degradation.** A kernel
built from this branch minus 0002+0003 (`7.1.6-limina16knm`), A/B'd against arm C on one rig clone
with `grubby`, does not fall back to compositing — it renders **nothing**: 47239 frames with
**0 draws** and no scanout, against arm C's 8351 frames at 27–57 draws. The guest names it:

```
error rendering frame: present-blit target (DrmFourcc(XR24)): this device does not support
DRM modifier 0x00ffffffffffffff for B8G8R8A8_UNORM (it enumerates others)
```

`0x00ffffffffffffff` = `DRM_FORMAT_MOD_INVALID`: with nothing in `IN_FORMATS` to name, niri falls
back to MOD_INVALID and its Vulkan renderer refuses the import. Human oracle: window frozen.
(The 0% missed-vblank figure is an artifact — you cannot miss a vblank you never aim for.)

Scope it precisely, because it is *not* "no patch, no desktop": stock Fedora kernels lack these
patches too and stock guests are fine, since stock mutter goes through GL/EGL and never names a
scanout modifier — the two-tier guarantee holds. Not isolated from 0002 (removed together); the
error names a modifier, so 0003 is near-certainly the whole cause, but the separating arm was not
run.

**Arm E, same day: that failure is compositor policy, and one line fixes it — so 0003 is
droppable.** niri refuses `MOD_INVALID`; every check and import in its Vulkan renderer reads a
single `modifier` binding, and labelling it LINEAR when the plane advertises no modifiers restores
a fully working session on the *same* patch-less kernel: 8339 frames, 88 missed (**1.06%**, vs arm
C's 1.33%), zero render errors, scanout on every frame. Truthful on this stack — niri already
asserts LINEAR carries the features it needs, because LINEAR is all this driver exposes. Diff:
`spikes/modifier-necessity/niri-mod-invalid-linear-fallback.patch`.

This row's verdict therefore softens from **carry** to **carry-or-drop, a product call**: dropping
costs nothing measurable in frame pacing, but means only compositors *we* patch keep the modifier
path — stock mutter and upstream niri guests lose it. Weigh against "limina runs any Linux desktop
well".

**0002 is a separate question that none of this settles.** Arm E shows median draws 35 → 45 (+29%)
and median GPU 3.09 → 3.48 ms (+13%) — the signature of content no longer reaching the plane
directly, which is exactly what the narrowed format list predicts (niri's own scanout comment: an
RGBA-order client "falls back to compositing"). The rig never exercises fullscreen *client* direct
scanout, so 0002 stays unpriced; the test that would settle it is a fullscreen ARGB8888 /
RGBA-order-swapchain client with 0002 out and 0003 in.

### 0001 — independently rediscovered and fixed upstream, in a file we never patched

Link Lin (Google) hit the identical free-page-reporting-vs-suspend UAF in their fleet (same stack:
`page_reporting_process` on the non-freezable system `events` workqueue racing
`virtballoon_freeze`'s `remove_common()`). Their 2026-07-09 RFC was *our exact* driver-side
unregister/re-register approach; Hildenbrand and MST steered it to the core fix — queue reporting
on `system_freezable_wq` so the PM freezer parks it before driver freeze callbacks — which landed
as **`0b45f69`** (2026-07-29, Cc: stable back through the 36e66c5 Fixes tag).

Upstream's fix is strictly better than ours was: freezing the worker covers every page-reporting
user and the shrinker-during-hibernation-image-save path (the trace in their commit message),
instead of depending on driver-callback ordering. We therefore carry *their* commit, not ours.

**Method lesson (in the README protocol): supersession can land in a file the patch never
touches — judge against the bug, not the diff's paths.** And a "superseded upstream" verdict is
not a licence to drop: `0b45f69` is Cc: stable but had **not** reached `v7.1.6` when we rebased,
so dropping without backporting would have shipped the s2idle UAF back into the enhanced image.

Host-relevant watch item: the `VIRTIO_BALLOON_F_REPORTING_PM_SAFE` (Bit 6) RFC — a
hypervisor-side gate to offer FPR only to fixed guests. If it lands in the virtio spec, limina's
libkrun balloon device should offer Bit 6; and until stock kernels absorb the stable backport,
not offering `F_REPORTING` to old-kernel guests is a valid stock-tier safety valve (degrades
reclaim, not the VM).

### The alignment patch's exit is merged upstream but not yet reachable

Two independent confirmations of this ledger's design assumptions. (1) Sasha Finkelstein (Asahi —
the same 16k-host case our commit message cited) posted a near-identical hardcoded-alignment patch
in Jan 2025 and it was **declined**: Zimmermann ("per-architecture code does not belong in a DRM
driver"), Osipenko (allocator-only alignment doesn't round the BO), Rob Clark ("we should add this
to the virtgpu protocol"). (2) That protocol now exists and is **merged**: Sergio Lopez's
`VIRTIO_GPU_F_BLOB_ALIGNMENT` (device advertises `blob_alignment`; guest userspace rounds blob
sizes; `verify_blob` rejects misaligned sizes with -EINVAL) — applied to drm-misc-next 2026-05-20,
in 7.2-rc, backed by a ratified virtio-spec change (oasis-tcs f9abfd55). Lineage: slp's 2024
HOST_PAGE_SIZE → 2025 generic SHM RFC → F_BLOB_ALIGNMENT.

**Why the module cannot retire yet:** the negotiated chain needs the device to advertise (ours),
the guest kernel to be ≥ 7.2 (stock Fedora's), and guest Mesa to query
`VIRTGPU_PARAM_BLOB_ALIGNMENT` and round (stock Fedora's, on this tier). Two of three are not ours
to ship, and advertising before the third turns working odd-size allocations into clean
`-EINVAL`s. Details and the internal action items: `guest/virtio-gpu-dkms/README.md`.

### The PRIME-import fix, and why the guest tier needs it (2026-08-23)

**(a) Capability.** A dmabuf imported into virtio-gpu — every frame GStreamer's `glupload`
sends through its DirectDmabuf uploader, i.e. all software-decoded video — is reported to the
host as a guest blob *and* attached to the importing render context, so the host learns the
frame's format and strides and can sample it.

**(b) Stock guest without it.** Boots and runs; video players draw stale framebuffer garbage on
this tier. A 4 KiB guest hides the bug because `glupload` picks a different uploader there — the
page size is the trigger, not the fault.

**(c) Host-side alternative.** There is none that is honest. Without `SET_TYPE` the host has the
pages and nothing else — no format, no stride, no plane offsets — so any host-side guess is a
heuristic over raw bytes. The renderer additionally *rejects* the resource as unknown to the
context, which `vrend_report_context_error` makes permanent: the player's GL context is dead at
its first frame. Both halves are guest-kernel facts; neither is inferable host-side.

**(d) Exit.** It is a plain driver bug with a self-contained fix, so the exit is upstream
acceptance: once a released kernel carries it, the enhanced-tier requirement dissolves into "a
new enough kernel" and the stock tier absorbs it. Nothing in the fix is limina-specific.

**The lesson the fix carries** is that it was one of *four* faults on a single path, each hiding
the next — two here, two in guest Mesa (`docs/upstreaming/ledger/mesa.md`), plus the host half in
vrend. Fixing any one alone changes the symptom and not the outcome, which is exactly how a
partial fix reads as a wrong theory. Measure the *next* hop after every fix, not the pixels.

### Load-bearing references

- `42fd9e6c29b3` — the format narrowing, read from the tree 2026-08-03 (supersedes this ledger's earlier 2017 citation)
- `85faca8ca0f6` — `fb_modifiers_not_supported`, read from the tree 2026-08-03
- `0b45f69` — the page-reporting suspend fix, fetched and diffed 2026-08-03
- `v7.1.4`/`v7.1.5`/`v7.1.6` `virtgpu_plane.c` + `mm/page_reporting.c` — fetched from kernel.org, 2026-08-03/04
- https://lore.kernel.org/dri-devel/20240903075414.297622-2-jfalempe@redhat.com/ (Falempe widening, Reviewed-by Gerd, unmerged)
- https://lore.kernel.org/dri-devel/YRI5PZiGXjbjlBO2@phenom.ffwll.local/ (Vetter on DEFERRED_OUT_FENCE — the objection profile to avoid)
