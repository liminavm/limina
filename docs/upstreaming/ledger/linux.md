# linux — patch-audit ledger

**Fork model since 2026-08-04.** There is no `patches/linux/` series any more: our kernel changes
are commits on **`github.com/liminavm/linux`** branch **`limina`**, base **`v7.1.6`**, pinned in
`third_party/manifest.toml`. The fork's parent is **`gregkh/linux`** (the stable-tree mirror) —
`torvalds/linux` has no stable point-release tags. Regenerate the series as a build artifact with
`scripts/export-linux-patches.sh` (`git format-patch base..rev`).

4 commits. Schema + protocol: `README.md`. Rows are keyed by SUBJECT; ordinals are informational
and drift on re-export.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | mm/page_reporting: use system_freezable_wq to fix UAF during suspend | `mm/page_reporting.c` |  | **backport** — cherry-pick of upstream `0b45f69` (Link Lin, 2026-07-29, 3 Acks + akpm, Cc: stable, Fixes: 36e66c5); replaced our own driver-side patch | NOT in `v7.1.6` (checked `mm/page_reporting.c` at v7.1.5 + v7.1.6, 2026-08-04: still `schedule_delayed_work`) | n/a — upstream root-caused it independently | n/a — thread lore.kernel.org/virtualization/20260721005603.1710551-1-linkl@google.com | no (guest self-oops only) | standalone | guest-enhanced (bug also afflicts stock guests until the stable backport reaches them) | **drop-on-rebase** once the base carries the stable backport; nothing to send | judge a base by `mm/page_reporting.c`, NOT `virtio_balloon.c` — the fix is invisible in the file we used to patch; watch: restore-error-path UAF follow-up, and the `F_REPORTING_PM_SAFE` Bit 6 RFC (host-relevant: our libkrun balloon could offer Bit 6) |
| 0002 | drm/virtio: fence RESOURCE_FLUSH for host3d blob scanout | `drivers/gpu/drm/virtio/virtgpu_plane.c` |  | needed — **rewritten** 2026-08-04 against the `vgplane_st` prepare_fb refactor (the old patch had silently stopped applying) | `v7.1.4`/`v7.1.6` read directly 2026-08-04: `prepare_fb:369` still early-returns on `!bo->guest_blob` before the fence alloc | n/a (dri-devel is patch-first) | none-yet | no | standalone | guest-enhanced | upstream-after-cleanup | mechanism-only, no uapi (avoids the DEFERRED_OUT_FENCE objection profile); the rewrite is the first version that actually ships — see Findings |
| 0003 | drm/virtio: widen the primary plane format list | `drivers/gpu/drm/virtio/virtgpu_plane.c` |  | needed — primary plane still XRGB-only upstream | master `075b748` (7.2-rc6) 2026-08-03 | none-yet | none-yet | no | **folded** (was 0002 ARGB + 0006 XBGR/ABGR — merged into one commit 2026-08-04) | guest-enhanced | upstream-after-cleanup | restores part of the list removed by `42fd9e6c29b3` (2018, not 2017 — see Findings); Falempe 2024 widening got Gerd's Reviewed-by (unmerged); LE-only fourccs for the RGBA pair (no `HOST_` alias — flag big-endian in the submission) |
| 0004 | drm/virtio: advertise DRM_FORMAT_MOD_LINEAR on planes | `drivers/gpu/drm/virtio/virtgpu_plane.c`, `drivers/gpu/drm/virtio/virtgpu_display.c` |  | needed — compositors gate direct scanout on an explicit LINEAR modifier | master `075b748` (7.2-rc6) 2026-08-03: `fb_modifiers_not_supported` still set | none | none; prior art = stalled 2021 set_scanout_blob-modifier series | low (guest-side; no new data crosses the virtio boundary) | standalone | guest-enhanced | **carry** — reverts a deliberate upstream decision (`85faca8ca0f6`, 2022); upstreamable only via host-negotiated modifiers (feature flag + capset + spec), a rider on the M15 device-advertised-formats work | the commit message now says NOT FOR UPSTREAM inline, so the verdict travels with the code; rebase watch: `display.c` churned by 2025 refactors |

## Left the series

| subject | where it went | why |
|---|---|---|
| drm/virtio: align host-visible window allocations to 16 KiB | `guest/virtio-gpu-dkms/0001-align-host-visible-allocations-to-16-KiB.patch` | **No-op on the 16k enhanced kernel** (PAGE_ALIGN is already 16 KiB) and **known-rejected upstream**, so it has no place on a branch whose purpose is upstreamable delta. Its one real deployment is the stock-4k tier via the DKMS module. Full rationale + the (not yet reachable) F_BLOB_ALIGNMENT exit: `guest/virtio-gpu-dkms/README.md`. |
| virtio_balloon: stop free-page reporting across suspend/resume | replaced by the `0b45f69` backport (row 0001) | Upstream root-caused the same UAF independently and chose the core fix over our driver-side approach. |

## Findings

### 0002 — the fence patch had been silently absent since the 7.1.x bump (2026-08-04)

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

**Enhanced-tier rubric:** (a) host-paced flip completion for venus blob scanout — the host fence
gates the guest's flip event (no stale-frame reuse races); (b) stock guest boots and renders;
blob flushes unfenced → compositor free-runs against the presenter (occasional stale-frame
flicker on the zero-copy path); (c) host-side-only pacing proven insufficient — the host cannot
delay a completion the guest never waits on; (d) exit = rebased drm-misc submission.

### 0003 — the format restriction is a 2018 big-endian side note, not a 2017 decision (corrected 2026-08-04)

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

### 0004 — LINEAR advertisement was deliberately *removed* upstream, not merely absent

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
mutter/niri engage direct scanout at all; (b) stock guest composites fullscreen — correct, one
extra guest-GPU pass per frame; (c) no host-side alternative — the gate is the guest kernel's
plane property enumeration, which the host cannot inject (patching N compositors would be
strictly worse than 1 driver); (d) exit = host-negotiated modifiers with M15.

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

### Load-bearing references

- `42fd9e6c29b3` — the format narrowing, read from the tree 2026-08-04 (supersedes this ledger's earlier 2017 citation)
- `85faca8ca0f6` — `fb_modifiers_not_supported`, read from the tree 2026-08-04
- `0b45f69` — the page-reporting suspend fix, fetched and diffed 2026-08-04
- `v7.1.4`/`v7.1.5`/`v7.1.6` `virtgpu_plane.c` + `mm/page_reporting.c` — fetched from kernel.org, 2026-08-03/04
- https://lore.kernel.org/dri-devel/20240903075414.297622-2-jfalempe@redhat.com/ (Falempe widening, Reviewed-by Gerd, unmerged)
- https://lore.kernel.org/dri-devel/YRI5PZiGXjbjlBO2@phenom.ffwll.local/ (Vetter on DEFERRED_OUT_FENCE — the objection profile to avoid)
