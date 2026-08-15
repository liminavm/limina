# Does the enhanced tier still need a 16 KiB-page guest kernel?

Written 2026-08-15, prompted by the guest kernel fork going **zero-delta**: the last commit on
`liminavm/linux` `limina` (the `page_reporting` freezable-wq UAF fix) landed upstream in stable, so
the v7.1.8 rebase dropped it as "patch contents already upstream". We now build and ship an entire
kernel RPM whose *only* load-bearing difference from Fedora's is one config symbol —
`CONFIG_ARM64_16K_PAGES=y` (`scripts/provision/f44/build-kernel-rpm.sh:10`, and the `[4/6]` config
block confirms it: everything else there is build hygiene or `=y` VM drivers). That is a good moment
to ask whether the 16 KiB requirement is still real.

**Short answer: the venus reason is gone, the page-size reasons are not.** 16 KiB is no longer the
only way to make venus work — but it is still the only way to make `mach_vm_remap` stitching,
virtiofs DAX, and balloon reclaim work at their natural granularity. Keep 16k; retire the
*justification*, which is stale in `docs/roadmap.md` and in older memory.

## What actually drove the requirement

venus host-visible memory becomes a virtio-gpu blob that the host maps with `hv_vm_map`, which
demands host address, guest address, **and** size all at 16 KiB granularity. A 4 KiB guest violates
that two independent ways:

| half | who breaks it | status |
|---|---|---|
| **size** — a blob sized `0x21000` is not a 16 KiB multiple | host passed the size verbatim | **fixed host-side 2026-07-03**, libkrun `0043` rounds map/unmap identically (+ virglrenderer `0023` for the zink map-info gate) |
| **offset** — two 4 KiB-packed blobs share one host page, so neither can be mapped alone | guest `virtgpu` vram allocator packs at 4 KiB | **not host-fixable**; needs the guest to keep the lattice |

Only the second half ever implied a page size, and it does not imply it *specifically* — it implies
an **aligned offset lattice**. A 16 KiB guest gets that for free (`PAGE_ALIGN` is 16 KiB, so blobs
are both 16 KiB-sized and 16 KiB-spaced). That is the whole of why the enhanced tier runs 16k.

`docs/roadmap.md:482` still says "No host-only fix exists" and calls this "THE constraint". That
sentence is now stale twice over — the size half *was* fixed host-only, and the offset half has two
guest-side answers that don't involve page size.

## The offset lattice without 16 KiB pages

**Today: `guest/virtio-gpu-dkms/`.** One patch gives every vram node a 16 KiB *start* alignment
(sizes stay exact — rounding them guest-side breaks `virtgpu_vram_mmap`, which requires mapping
length == node size). Delivered out-of-tree so it shadows the in-tree module. Validated on stock
4 KiB F44: `Virtio-GPU Venus (Apple M1 Max)` enumerated for the first time on the stock tier
(`spikes/venus-4k-dkms/RESULTS.md`). **Still not wired into the guest-tools payload** — grep says
neither `build-all.sh` nor `install-enhanced.sh` mentions it, so it remains a hand-installed spike
artifact.

**Upstream: `VIRTIO_GPU_F_BLOB_ALIGNMENT`.** This is exactly the "host-side padding" idea we
discussed, formalized as a negotiated protocol, and it is **merged** — verified in v7.2-rc7, not
recalled:

- `include/uapi/linux/virtio_gpu.h:74` — `#define VIRTIO_GPU_F_BLOB_ALIGNMENT 5`
- `include/uapi/linux/virtio_gpu.h:376` — `__le32 blob_alignment` in the device config
- `include/uapi/drm/virtgpu_drm.h:101` — `VIRTGPU_PARAM_BLOB_ALIGNMENT 9`
- `drivers/gpu/drm/virtio/virtgpu_ioctl.c:499` — `if (has_blob_alignment && !IS_ALIGNED(params->size, blob_alignment)) return -EINVAL;`

Note what the kernel does and does not do. It **rejects** misaligned blob sizes; it does **not**
round them. And `virtgpu_vram.c:163` still calls `drm_mm_insert_node` with no alignment argument.
So the lattice is maintained purely by every size being a multiple of the granule: aligned sizes
packed contiguously from an aligned base can only ever produce aligned starts, and holes freed from
such a lattice are themselves aligned multiples. That is why the *rounding* has to happen in guest
userspace — and why the ordering warning in `guest/virtio-gpu-dkms/README.md` is the load-bearing
part of this whole design: **advertising the granule before the guest's Mesa rounds converts working
odd-size allocations into clean `-EINVAL` failures.**

### The chain, and who owns each link

1. **libkrun advertises `blob_alignment = 16384`** — ours. Small: feature bit 5 into `AVAIL_FEATURES`
   (`third_party/libkrun/src/devices/src/virtio/gpu/device.rs:35`) and a fifth `__le32` on the
   config struct built in `read_config` (`device.rs:393`, currently only `events_read`,
   `events_clear`, `num_scanouts`, `num_capsets`).
2. **Guest kernel ≥ 7.2** — 7.2 is at rc7, so weeks away. For the *enhanced* tier this is ours (we
   build the kernel). For the *stock* tier it is Fedora's, and F44 currently ships 6.19.10 — a long
   way back.
3. **Guest Mesa queries the param and rounds** — **this does not exist anywhere yet.** Not in
   26.1.7, and not on Mesa `main` (`git grep BLOB_ALIGNMENT origin/main -- src/virtio` is empty;
   the only params venus queries are listed at `vn_renderer_virtgpu.c:1023-1061`). Somebody has to
   write it, and it is squarely upstreamable.

So the honest status is that link 3 is *unwritten*, not merely unshipped — the README's "not ours to
ship" is true for the stock tier but understates it: nobody has shipped it for any tier.

### A landmine found while verifying link 3

Mesa hardcodes `#define VIRTGPU_PARAM_GUEST_VRAM 9` (`vn_renderer_virtgpu.c:31`, a downstream
ChromeOS param number that never went upstream). Upstream 7.2 has now allocated **9** to
`VIRTGPU_PARAM_BLOB_ALIGNMENT`. On a 7.2 kernel whose device advertises the granule, Mesa asking for
"guest vram" gets `16384` back and reads it as *yes*.

It is latent for us — the collision sits in the `else` branch at `vn_renderer_virtgpu.c:1044`, only
reached when `VIRTGPU_PARAM_HOST_VISIBLE` is falsy, and our blobs are host-visible — but it is a
real upstream bug for any host-visible-less configuration, and it is worth reporting alongside
whatever we write for link 3. It also means link 1 and link 3 must not be considered independent:
advertising the granule perturbs an unrelated Mesa code path.

## What still genuinely needs 16 KiB pages

None of the above touches these. They need **guest page size == host page size**, which no protocol
negotiation can supply:

- **udmabuf / zero-copy video import** (`docs/roadmap.md:1825`): `mach_vm_remap` works at the host's
  16 KiB granularity, so a 4 KiB guest presents fragments that cannot be remapped individually and
  falls back to a copy.
- **virtiofs DAX** (`docs/roadmap.md:651`): the FUSE_SETUPMAPPING/SHM window wants guest-page ==
  host-page; stock-4k DAX is explicitly an untested separate case.
- **Balloon reclaim granularity** (`docs/design/m6-dynamic-memory.md:230,472`): `MADV_FREE_REUSABLE`
  needs 16 KiB-aligned, 16 KiB-multiple, fully-free runs. Host-side coalescing makes 4 KiB *work*,
  but at a measurable loss — the whole `limina-hv-ledger-gap` 2× ledger story lives here.
- Plus the ordinary TLB win of matching the host.

## Recommendation

**Keep the 16 KiB kernel for the enhanced tier.** Nothing here argues for dropping it; the venus
argument simply stops being the reason. Rewrite the roadmap's "THE constraint / no host-only fix
exists" paragraph accordingly, so the next reader doesn't re-derive a solved problem.

**The question worth actually pursuing is the adjacent one: can the enhanced tier stop shipping a
custom kernel at all?** With the fork at zero delta we are building a multi-GB kernel for one config
symbol, and the two-tier guarantee means a stock-kernel enhanced tier would be a large maintenance
win. That is blocked on the same chain — a stock guest needs ≥7.2 *and* a rounding Mesa before venus
survives on stock 4 KiB pages — and it costs the four page-size items above. It is a real trade, not
a free win, and it should be decided deliberately rather than by drift.

**Concrete, in dependency order:**

1. *(now, cheap)* Fix the stale roadmap paragraph and file the Mesa param-9 collision upstream.
2. *(now, cheap, no behaviour change)* Wire `guest/virtio-gpu-dkms/` into the guest-tools payload if
   we want stock-tier venus to be a shipped capability rather than a spike — it has been validated
   since 2026-07-03 and is still hand-installed.
3. *(when someone writes link 3)* The venus rounding patch for our `limina-guest` Mesa branch:
   query `VIRTGPU_PARAM_BLOB_ALIGNMENT`, round `vkAllocateMemory` blob sizes up to it. Upstreamable,
   and the natural companion to the collision report.
4. *(only after 3 ships, and gated)* libkrun advertises `blob_alignment = 16384`. **Do not do this
   before 3**, and note that advertising unconditionally would break a DKMS-equipped stock guest on
   a 7.2 kernel: that module aligns starts but leaves sizes exact, which is precisely what
   `verify_blob` rejects. Whatever gate we choose, it has to distinguish "guest rounds" from "guest
   merely has a 7.2 kernel", and the virtio feature bit cannot make that distinction — the kernel
   ACKs it, and the kernel is not the component that rounds.

Step 4 has **no beneficiary today**: 16k enhanced guests do not need it, and stock guests cannot
round yet. It is correct to leave it unbuilt until 3 exists.
