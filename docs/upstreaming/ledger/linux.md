# linux — patch-audit ledger

6 patches; `UPSTREAM_BASE` `floating — see the series README`. Schema + protocol: `README.md`.
Rows are keyed by SUBJECT; ordinals are informational and drift on re-export.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | drm/virtio: attach a fence to blob-scanout RESOURCE_FLUSH (limina #8 half 2) | `drivers/gpu/drm/virtio/virtgpu_plane.c` |  | needed — series-README "superseded" verdict OVERTURNED 2026-08-03, see Findings | master file-touch `a48bbcc7` 2026-08-03 | n/a (dri-devel is patch-first) | none-yet | no | standalone | guest-enhanced | upstream-after-cleanup | rewrite vs the vgplane_st prepare_fb refactor; pitch = mechanism-only, no uapi (avoid the DEFERRED_OUT_FENCE objection profile) |
| 0002 | drm/virtio: accept ARGB8888 on the primary plane (limina direct scanout) | `drivers/gpu/drm/virtio/virtgpu_plane.c` |  | needed — primary plane still XRGB-only upstream | master `075b748` (7.2-rc6) 2026-08-03 | none-yet | none-yet | no | fold-into:0006 (one widen-primary-formats patch) | guest-enhanced | upstream-after-cleanup | restores part of the pre-2017 list Gerd removed as an "easy way out"; Falempe 2024 widening got his Reviewed-by (unmerged) |
| 0003 | drm/virtio: advertise DRM_FORMAT_MOD_LINEAR on planes (limina direct scanout) | `drivers/gpu/drm/virtio/virtgpu_plane.c`, `drivers/gpu/drm/virtio/virtgpu_display.c` |  | needed — compositors gate direct scanout on an explicit LINEAR modifier | master `075b748` (7.2-rc6) 2026-08-03: `fb_modifiers_not_supported` still set | none | none; prior art = stalled 2021 set_scanout_blob-modifier series | low (guest-side; no new data crosses the virtio boundary) | standalone | guest-enhanced | **carry** — reverts a deliberate upstream decision (85faca8, 2022); upstreamable only via host-negotiated modifiers (feature flag + capset + spec), a rider on the M15 device-advertised-formats work | series-README "near-ready" claim is too optimistic — corrected 2026-08-03; rebase watch: display.c churned by 2025 refactors |
| 0004 | drm/virtio: align host-visible window allocations to 16 KiB | `drivers/gpu/drm/virtio/virtgpu_vram.c` |  | needed for 4 KiB-page guests; NO-OP on the 16k enhanced kernel — mechanism superseded by merged `VIRTIO_GPU_F_BLOB_ALIGNMENT` (7.2-rc) | master `075b748` (7.2-rc6); `virtgpu_vram.c` file-touch `4c26e16` 2026-05-13; checked 2026-08-03 | none to file; INTERNAL action: libkrun advertises `blob_alignment=16384` + verify guest-Mesa rounding | **do NOT send** — the identical hardcoded shape (Finkelstein 2025-01) was declined (Zimmermann/Osipenko/Rob Clark: protocol, not per-arch driver code); slp's F_BLOB_ALIGNMENT is that protocol, merged | no (bounded guest-side slack only) | standalone (functional pair = libkrun 0043, different tree; dual delivery incl. `guest/virtio-gpu-dkms`) | **stock/4k-guest enabler via DKMS**; enhanced-series copy is forward insurance (future 4k/FEX kernel) — README tier label corrected | carry until KVER ≥ 7.2 AND libkrun advertises blob_alignment, then drop (+ eventually the DKMS module) | 7.2's `verify_blob` returns -EINVAL on misaligned sizes — do NOT flip the feature on before guest Mesa queries `VIRTGPU_PARAM_BLOB_ALIGNMENT` and rounds; rebase hunk past the 2026-05 deferred-mapping refactor for KVER ≥ 7.2 |
| 0005 | virtio_balloon: stop free-page reporting across suspend/resume | `drivers/virtio/virtio_balloon.c` |  | **superseded-upstream@0b45f69** — fixed differently in `mm/page_reporting.c` (system_freezable_wq), Cc: stable, Fixes: 36e66c5 | file-touch `47c81da` + mm fix `0b45f69` (2026-07-29), checked 2026-08-03 | n/a — upstream root-caused it independently | n/a — thread lore.kernel.org/virtualization/20260721005603.1710551-1-linkl@google.com | no (guest self-oops only) | standalone | guest-enhanced (bug also afflicts stock guests until the stable backport reaches them) | **drop-on-rebase** once the base contains 0b45f69 or its stable backport; do NOT send upstream | check `mm/page_reporting.c` for `system_freezable_wq` when judging a base, NOT the balloon file; watch: restore-error-path UAF follow-up, stats-work series, and the F_REPORTING_PM_SAFE Bit 6 RFC (host-relevant: our libkrun balloon could offer Bit 6) |
| 0006 | drm/virtio: accept XBGR8888/ABGR8888 on the primary plane (limina direct scanout) | `drivers/gpu/drm/virtio/virtgpu_plane.c` |  | needed — not upstream, no pending lore patch | master `075b748` (7.2-rc6) 2026-08-03 | none-yet | none-yet | no | fold-into:0002 (declares the dependency; hunks overlap) | guest-enhanced | upstream-after-cleanup | LE-only fourccs (no `HOST_` alias — flag big-endian in the submission); durable fix = device-advertised plane formats (planned with M15 overlay planes) |

## Findings

### 0001 — the "superseded on 7.1.x" verdict rested on a false premise (2026-08-03)

`patches/linux/README.md` (2026-07-30) claimed upstream's refactored prepare_fb —
fence iff `bo->dumb || drm_gem_is_imported(obj)` — covers compositor scanout FBs
because "they arrive as imported dmabufs". One hop of verification breaks that:
`drm_gem_is_imported` is `!!obj->import_attach` (cross-device only), and the
**self-import path short-circuits** (`virtgpu_prime.c:301-310`): a dmabuf exported
and re-imported on the *same* virtio-gpu device returns the original GEM object —
no `import_attach`, `guest_blob` false, `host3d_blob` true — so it is filtered by
the `!bo->guest_blob` primary-plane early-return and its RESOURCE_FLUSH goes out
**unfenced on master**. That "residual theoretical gap" is the mainline
single-device venus scanout topology. The README's *empirical* half stands: the
deployed `7.1.4-limina16k` paces correctly without 0001, most plausibly because the
enhanced stack paces via the venus WSI fence chain (mesa 0009/0010 + libkrun
0017/0018), making the KMS fence belt-and-braces there — but the upstream delta is
real for any guest leaning on KMS flush fencing.

Prior art (lore): Kasireddy 2021 "default synchronization mechanism for blobs"
(landed ~5.15, the dumb-path template our patch extends); Kasireddy's
`DRM_CAP_DEFERRED_OUT_FENCE` RFC (stalled on *new-uapi* objections from Vetter —
our pitch must stay mechanism-only); Kasireddy 2024 "Import scanout buffers from
other devices" (the series whose fencing the README mistook for full coverage).
Method lesson recorded in README protocol: a `need` verdict resting on semantic
equivalence must record its premise chain, not just a SHA.

### 0002 + 0006 — widen primary-plane formats (fold pair)

The XRGB8888-only primary plane is a 2017 expedient, not a capability statement:
Gerd Hoffmann's "virtio restrict to XRGB8888" (2017-04-24) *removed* an 8-format
list — including ARGB8888/XBGR8888/ABGR8888 — because dumb-create bound the format
at BO creation ("Easy way out: support a single format only"). That underlying
problem is long fixed, and Jocelyn Falempe's 2024 two-format widening carried
Gerd's Reviewed-by ("Now that it's fixed, it can support both") but never merged —
so the restriction survives by inertia, and our patches partially restore the
pre-2017 list. Review questions to pre-answer in the submission: opaque-alpha
semantics for ARGB on the bottom plane (may want virtio-gpu spec text: "scanout
alpha is ignored"), and host capability variance for XBGR/ABGR (the durable answer
is device-advertised plane formats — the virtio protocol extension planned with
M15 overlay planes; the hardcode is sellable meanwhile as restoration). Rebase
watch: 2026 master refactors shift context lines when KVER bumps.

**Enhanced-tier rubric (pair):** (a) compositors' native ARGB/RGBA-order buffers
hit the primary plane → true zero-copy direct scanout; (b) stock guest fully
functional via the composited/swizzled path — correct output, extra guest GPU cost
per frame; (c) host-side alternative impossible — the rejection happens in the
guest's DRM atomic check before anything reaches the host (host already displays
RGBA-order IOSurfaces natively, `spikes/scanout-modifiers/`); (d) exit = upstream
the folded patch and/or land device-advertised formats; tolerant apply then skips.

**Enhanced-tier rubric (0001):** (a) host-paced flip completion for venus blob
scanout — the host fence gates the guest's flip event (no stale-frame reuse races);
(b) stock guest boots and renders; blob flushes unfenced → compositor free-runs
against the presenter (occasional stale-frame flicker on the zero-copy path);
(c) host-side-only pacing proven insufficient — the host cannot delay a completion
the guest never waits on; (d) exit = rebased drm-misc submission; then stock
kernels ≥ X fence natively.

### 0003 — LINEAR advertisement was deliberately *removed* upstream, not merely absent

Commit **85faca8** ("drm/virtio: set fb_modifiers_not_supported", Chia-I Wu, Sep
2022, R-b Daniel Stone, pushed by Gerd Hoffmann) opted virtio-gpu out of the drm
core's implicit LINEAR advertisement — a guest-side LINEAR claim is a lie on virgl
hosts whose resource layouts are opaque/tiled (it broke Chrome ozone/drm scanout).
Our patch reverts exactly that: correct for limina (our host layout genuinely is
linear), incorrect for generic virtio-gpu. Upstream's stated bar (Gerd, 2021,
reviewing the stalled set_scanout_blob-modifier series; same direction in Julia
Zhang's 2023-24 RESOURCE_GET_LAYOUT proposals): virtio feature flag +
host-provided modifier list (capset) + spec text. Sending plain LINEAR to
dri-devel would get the 2022 rationale quoted back. So: **carry**, and fold the
exit into the M15 device-advertised-plane-formats protocol extension — one
protocol story retires both 0003 and the 0002+0006 pair's variance caveat.

**Enhanced-tier rubric (0003):** (a) completes the zero-copy chain — IN_FORMATS
LINEAR lets mutter/niri engage direct scanout at all; (b) stock guest composites
fullscreen — correct, one extra guest-GPU pass per frame; (c) no host-side
alternative — the gate is the guest kernel's plane property enumeration, which the
host cannot inject (patching N compositors would be strictly worse than 1 driver);
(d) exit = host-negotiated modifiers with M15; the 5-line patch is cheap to carry
tolerantly until then.

### 0005 — independently rediscovered and fixed upstream, in a file we don't patch

Link Lin (Google) hit the identical free-page-reporting-vs-suspend UAF in their
fleet (same stack: `page_reporting_process` on the non-freezable system `events`
workqueue racing `virtballoon_freeze`'s `remove_common()`). Their 2026-07-09 RFC
was *our exact* driver-side unregister/re-register approach; Hildenbrand and MST
steered it to the core fix — queue reporting on `system_freezable_wq` so the PM
freezer parks it before driver freeze callbacks — which landed as **`0b45f69`**
(2026-07-29, 3 Acks + akpm, Cc: stable back through the 36e66c5 Fixes tag).
`virtio_balloon.c` on master is untouched; the fix is invisible in that file's
history. **Method lesson (now in the README protocol): supersession can land in a
file the patch never touches — judge against the bug, not the diff's paths.**

Host-relevant watch item: the `VIRTIO_BALLOON_F_REPORTING_PM_SAFE` (Bit 6) RFC —
a hypervisor-side gate to offer FPR only to fixed guests. If it lands in the
virtio spec, limina's libkrun balloon device should offer Bit 6; and until stock
kernels absorb the stable backport, not offering `F_REPORTING` to old-kernel
guests is a valid stock-tier safety valve (degrades reclaim, not the VM).

**Enhanced-tier rubric (0005):** (a) ballooning + FPR survive s2idle without a
guest oops/wedged resume; (b) a stock guest negotiating F_REPORTING carries the
same upstream bug — probabilistic oops under host-sleep until its distro kernel
backports the fix (boots fine; two-tier guarantee holds); (c) host-side
alternative = suppress F_REPORTING (loses M6 FRQ reclaim — valid safety valve,
wrong as the permanent answer; Bit 6 is that dilemma productized); (d) exit
already in motion — drop our patch at the next base bump containing the mm fix.

### 0004 — upstream merged the negotiated mechanism; the hardcoded shape is known-rejected

Two independent confirmations of the ledger's design assumptions. (1) Sasha
Finkelstein (Asahi — the same 16k-host case our commit message cites) posted a
near-identical hardcoded-alignment patch in Jan 2025 and it was **declined**:
Zimmermann ("per-architecture code does not belong in a DRM driver"), Osipenko
(allocator-only alignment doesn't round the BO), Rob Clark ("we should add this
to the virtgpu protocol"). (2) That protocol now exists and is **merged**:
Sergio Lopez's `VIRTIO_GPU_F_BLOB_ALIGNMENT` (device advertises `blob_alignment`;
guest userspace rounds blob sizes; `verify_blob` rejects misaligned sizes with
-EINVAL) — applied to drm-misc-next 2026-05-20, in 7.2-rc, backed by a ratified
virtio-spec change (oasis-tcs f9abfd55). Lineage: slp's 2024 HOST_PAGE_SIZE →
2025 generic SHM RFC → F_BLOB_ALIGNMENT.

**Migration plan (internal action items, not upstream submissions):** teach
limina's libkrun virtio-gpu device to advertise `blob_alignment = 16384`;
confirm guest-Mesa venus queries `VIRTGPU_PARAM_BLOB_ALIGNMENT` and rounds
(flipping the feature on without that turns working odd-size allocations into
clean -EINVALs); drop 0004 and eventually the DKMS module once fleet guest
kernels are ≥ 7.2. **Tier correction:** 0004 is a no-op on the 16k enhanced
kernel (PAGE_ALIGN is already 16k) — its load-bearing deployment is the STOCK
4k tier via `guest/virtio-gpu-dkms`; the enhanced-series copy is insurance for
a future 4 KiB (FEX) kernel.

**Enhanced-tier rubric (0004):** (a) stable venus blob mapping on a 16k host
with a 4k-page guest (the offset half; libkrun 0043 is the size half);
(b) stock 4k guest without it: first odd-sized blob poisons window offsets →
mid-session `vkMapMemory` failures; still boots, 2D/llvmpipe fine — which is
why the DKMS delivery exists; (c) host-side alternative measured and rejected
(vkr requirements-rounding re-poisons offsets → mid-run OOMs, see
`limina-blob-map-16k-alignment`); the allocator lives guest-side; (d) exit =
F_BLOB_ALIGNMENT migration above — merged, concrete, ours to adopt.

### Load-bearing references

- https://lore.kernel.org/dri-devel/20170424062532.26722-7-kraxel@redhat.com/ (the 2017 restriction's origin)
- https://lore.kernel.org/dri-devel/20240903075414.297622-2-jfalempe@redhat.com/ (Falempe widening, Reviewed-by Gerd, unmerged)
- https://lore.kernel.org/dri-devel/YRI5PZiGXjbjlBO2@phenom.ffwll.local/ (Vetter on DEFERRED_OUT_FENCE — the objection profile to avoid)
- master `virtgpu_plane.c` fence logic + `virtgpu_prime.c` self-import short-circuit + `drm_gem_is_imported` (raw.githubusercontent.com, 2026-08-03)

