# Does the enhanced tier still need a 16 KiB-page guest kernel?

Written 2026-08-15, prompted by the guest kernel fork going **zero-delta**: the last commit on
`liminavm/linux` `limina` (the `page_reporting` freezable-wq UAF fix) landed upstream in stable, so
the v7.1.8 rebase dropped it as "patch contents already upstream". We now build and ship an entire
kernel RPM whose *only* load-bearing difference from Fedora's is one config symbol —
`CONFIG_ARM64_16K_PAGES=y` (`scripts/provision/f44/build-kernel-rpm.sh:10`, and the `[4/6]` config
block confirms it: everything else there is build hygiene or `=y` VM drivers). That is a good moment
to ask whether the 16 KiB requirement is still real.

**Short answer: there is no hard 16 KiB requirement left.** venus was the only thing that truly
*failed* without it, and that is fixed. Everything else 16 KiB buys — `mach_vm_remap` stitching,
virtiofs DAX, balloon reclaim granularity, TLB pressure — already degrades gracefully on 4 KiB, and
in the balloon's case the coalescing is implemented and shipping. So: **keep 16 KiB as *the*
enhanced tier because it is better, not because anything is impossible without it**, and note that
a stock distro working unaided is a reachable goal gated on a short list of *userspace* upstream
work (§ "Just works on a regular distro"). The justification in `docs/roadmap.md` and older memory
is stale.

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

## What 16 KiB buys — and why none of it is a *blocker*

An earlier draft of this document listed the items below as "what still genuinely needs 16 KiB".
That was wrong, and the distinction it blurred is the important one: **venus was a hard failure,
these are all degradations that already have best-effort paths.** Several of those paths are
implemented and shipping today.

- **Balloon reclaim granularity** (`docs/design/m6-dynamic-memory.md:230,472`):
  `MADV_FREE_REUSABLE` needs 16 KiB-aligned, 16 KiB-multiple, fully-free runs — and libkrun
  **already coalesces** to get them. `virtio/balloon/device.rs:83-140` keeps a per-host-page
  sub-page bitmap with an all-free mask (`(1 << (host_page / GUEST_PAGE)) - 1` = `0b1111` on
  16K/4K) and drains only host pages whose every sub-page is free. A 4 KiB guest reclaims less of
  what it frees; it does not fail.
- **udmabuf / zero-copy video import** (`docs/roadmap.md:1825`): the roadmap says a 4 KiB guest
  "can present 4 KiB fragments that cannot be remapped individually" and falls back. That is a
  worst case being described as a necessity — **guest page size is the wrong variable**. See
  § "udmabuf: the real variable is allocation granularity" below; a 4 KiB guest can stitch fully.
- **virtiofs DAX** (`docs/roadmap.md:651`): not implemented yet — it is a listed follow-up, not a
  live dependency. When it lands, the degradation is "DAX doesn't engage, plain FUSE read/write
  does", which is where every non-DAX guest already lives.
- **TLB pressure**: a perf difference, never a correctness gate.

So, post-venus-fix, **there is no hard 16 KiB blocker left anywhere in the stack.** 16 KiB is what
makes the enhanced tier *good*, not what makes anything *possible* — which is exactly the shape the
two-tier guarantee asks for.

The one genuine hard failure was always venus, and it failed in the worst available way: without a
working blob map, `vkCreateInstance` returns `VK_ERROR_OUT_OF_HOST_MEMORY`, and the Vulkan loader
treats OOM as fatal for the whole instance chain — so it took healthy lavapipe down with it. Stock
Vulkan wasn't degraded, it was *dead*. That asymmetry is why this one item earned "THE constraint"
while the others never did.

## udmabuf: the real variable is allocation granularity, not page size

Worth deriving, because "16 KiB host pages vs 4 KiB guest pages" is the wrong frame and it leads
straight to an unnecessary fallback.

`mach_vm_remap` moves **whole host pages to host-page-aligned destinations**. We are building one
contiguous destination VA in which the fragment belonging at buffer offset `O` must land at `O`. So
a fragment is remappable exactly when:

- `O` is 16 KiB-aligned — and since the buffer is laid out linearly, `O = page_index * 4096`, so
  this just picks out every 4th guest page; **and**
- the source is 16 KiB-aligned and 16 KiB-contiguous in host VA. Guest PA → host VA is a fixed
  offset from a 16 KiB-aligned RAM base, so this means the four consecutive guest pages at `O` are
  **physically contiguous and 16 KiB-aligned**.

Which is to say: the buffer must be composed of 16 KiB-aligned, physically-contiguous quads. A
16 KiB guest gets that because every page *is* such a quad — but that is a sufficient condition,
not a necessary one, and a 4 KiB guest reaches it by ordinary means:

- Linux's buddy allocator gives **natural alignment**: any order-2 (16 KiB) allocation is 16 KiB-
  aligned and contiguous by construction.
- shmem/memfd **large folios** produce multi-page runs, and a hugetlb memfd produces 2 MiB ones.
- udmabuf is already folio-based and explicitly takes both:
  `drivers/dma-buf/udmabuf.c` pins via `memfd_pin_folios()` into a `pinned_folios` array, and
  `udmabuf_create` accepts `shmem_file(memfd) || is_file_hugepages(memfd)` (v7.2-rc7, line 276).

So a THP- or hugetlb-backed memfd on a stock 4 KiB guest stitches **exactly as well as** a 16 KiB
guest. The frame is scattered in 2 MiB runs, not 4 KiB ones.

**And the fallback need not be all-or-nothing anyway.** The destination range is ours: allocate it,
then `mach_vm_remap` (with `VM_FLAGS_OVERWRITE`) every 16 KiB slot that qualifies and `memcpy` only
the ragged remainder. libkrun already hands the worker **host-VA iovecs**
(`virtio_gpu.rs attach_backing` → `virgl_renderer_resource_attach_iov`), so the qualification test
is a cheap runtime scan of the iov list, not a guess about the guest. Degradation becomes
proportional to actual misalignment instead of binary.

One caveat that decides where the hybrid is legitimate: **a copied slot is a snapshot, not shared
storage.** For write-once media frames — the case this wave exists for — that is fine. For a buffer
the guest rewrites in place, the copied slots go stale, so either re-copy just the ragged fraction
per frame (still far cheaper than today's whole-frame upload) or gate reused buffers on
fully-remappable. Do not let the hybrid quietly break the share-don't-copy invariant.

Practical consequence: phase 1 of Wave 6 already requires a guest kernel change (teaching
drm/virtio's PRIME import to register a foreign dmabuf), so asking the allocation path for 16 KiB
granularity is a small addition in a place we are already opening — and it should be a **hint**,
with the host-side scan above as the safety net, so a guest that ignores it still works.

## "Just works on a regular distro" — the actual shopping list

Given the above, the goal of a stock distro working unaided (with the enhanced tier still being
16 KiB, and still better) is reachable, and the gate is a short list of **userspace** items:

1. **Mesa: venus degrades to its stub instance when ring setup fails.** This is our series patch
   0003 (`d517a1b49d1`) and it is the one that converts "all Vulkan dead" into "llvmpipe works" —
   the difference between a broken distro and a degraded one. Not upstream: the 26.1.7 rebase
   auto-dropped our kernel patch as "already upstream" and did *not* drop this, and
   `docs/upstreaming/ledger/mesa.md` has it queued as Wave 1. **This is the highest-value upstream
   item in the whole plan** and it is worth pursuing independently of everything else here.
2. **Mesa: query `VIRTGPU_PARAM_BLOB_ALIGNMENT` and round blob sizes to it** — unwritten anywhere
   (§ above), and the piece that makes stock-tier venus actually work rather than merely fail
   politely.
3. **Mesa: fix the param-9 collision** (`VIRTGPU_PARAM_GUEST_VRAM` vs the kernel's
   `BLOB_ALIGNMENT`), which becomes live the moment anything advertises the granule.
4. **libkrun advertises `blob_alignment = 16384`** — ours, and strictly after 2.
5. **Distro kernel ≥ 7.2** — just time.

Items 1-3 are all Mesa, all upstreamable, and 1 is independently valuable today. That is a
tractable list, and it is the honest answer to "what would it take for everything to just work on
regular distros".

## The strategy this serves: two tiers that diverge, and a stock tier that converges on "agent only"

Settled with the user 2026-08-15. This is not a plan to *drop* 16 KiB — it is a plan to stop 16 KiB
being load-bearing, so the two tiers can move in opposite directions on purpose:

- **The enhanced tier keeps the 16 KiB kernel and gets *more* custom over time**, not less. It is
  heading toward a fully-owned image — our kernel, bootloader, update mechanism, default compositor
  (see the LiminaOS work; the plan of record lives outside this repo, per the
  `limina-liminaos-prototype` memory). 16 KiB is one of many things we control there, and we keep it
  because it is better: exact 1:1 balloon reclaim, `mach_vm_remap` stitching with no hybrid path,
  matching TLB behaviour.
- **The stock tier converges on shipping nothing but `limina-agent`.** Every other component we
  install into a guest today exists to fill a gap upstream hasn't closed. As each gap closes, we
  stop shipping that component — and a stock Fedora gets the full feature set with only the agent
  added, which is the piece that is *inherently* ours (it speaks our control plane; there is
  nothing to upstream it into).

That reframes most of the work in this document as **upstreaming**, and makes the value of each
upstream patch measurable in the same unit: does it remove something from the payload?

### What we ship into a guest today, and what retires it

| component | why we ship it | what retires it |
|---|---|---|
| **`limina-agent`** + `limina-agent-session` | clipboard, dynamic memory reporting, timesync, FIDO, lifecycle — our control plane | **nothing. This is the permanent floor**, and the target end state is that it is the *only* thing we ship. |
| **`limina-kernel-16k`** | 16 KiB pages (one config symbol — the fork branch carries zero patches as of v7.1.8) | for the *stock* tier: distro kernel ≥ 7.2 plus the blob-alignment chain below. For the *enhanced* tier: nothing — we keep building it deliberately. |
| **mesa RPM** (8 venus/virgl/zink patches) | venus correctness + WSI + the CPU-write coherency fix | all 8 landing upstream and reaching a distro release. Four are already flagged **upstream-now** in `docs/upstreaming/ledger/mesa.md` (rows 0012, 0013, 0007, 0008); the rest need design work or a conversation. |
| **`guest/virtio-gpu-dkms`** | 16 KiB blob-offset lattice on stock 4 KiB guests | the blob-alignment chain (§ above). Same trigger as the kernel. Currently not even wired into the payload. |
| **`clipboard@limina`** shell extension | GNOME has no `ext-data-control`, so an unfocused agent cannot touch the clipboard | **M12 — `spice-vdagent`.** This row converges *better* than any other, because the guest component is already installed: `spice-vdagent` ships in the default Fedora Workstation set (verified in the dogfood guest's rpmdb, `0.23.0-1.fc43.aarch64`) and sits dormant purely for want of a named virtio-serial port. Waking it is **~40 lines of host-side limina code** — no guest component, and (contrary to the roadmap's original premise) no new libkrun device either, since `PortConfig::InOut` already announces `VIRTIO_CONSOLE_PORT_NAME`. Spike #1 was GREEN on an unmodified F43 guest with zero limina components. Remaining work is the host-side broker. |

Note what that last row means for the framing: the `ext-data-control` route is upstream-rejected
(ledger `mutter.md` 0003 — no implementation, no MR, a work item filed *against* adopting it), and
an earlier draft of this table therefore concluded clipboard "does not converge". **That was wrong**
— it anchored on the GNOME-native route and missed that vdagent sidesteps GNOME entirely (the guest
copy arrives as a real `CLIPBOARD_GRAB` via XWayland, verified on Wayland). The shell extension is
the **interim** tier, not the destination. Worth remembering as a shape: a rejected upstream is only
a dead end for the route it rejected.

The two live caveats on that route, both from the M12 spike: `vdagentd` is socket-activated by the
*session* agent, so it needs a **graphical** session (a headless boot has `CanGraphical=no` and it
exits); and its caps offer `CLIPBOARD_BY_DEMAND` + `CLIPBOARD_SELECTION` but **not** legacy
`CLIPBOARD`, so the broker must speak by-demand + selection.

## Open question for the same investigation: 16 KiB vs 64 KiB guest pages

Raised by the user 2026-08-15. Red Hat already ships a 64 KiB-page aarch64 kernel (`kernel-64k`),
so 64 KiB is a *standardised* option in a way 16 KiB is not — if the ecosystem converges there, the
enhanced tier might be better off following it than holding a bespoke page size. Worth measuring
before assuming either way, on three axes: **fragmentation, balloon behaviour, memory usage.**

What we can reason about up front, so the measurement targets the parts that are actually uncertain:

- **Alignment gets *easier*, not harder.** The host is 16 KiB, and 64 KiB is an exact multiple — so
  every guest page is host-page-aligned and spans exactly 4 host pages. Every lattice concern in
  this document dissolves even more thoroughly than at 16 KiB. This axis is not the risk.
- **Balloon reclaim stays exact but gets coarse.** The coalescer degenerates the other way: one
  guest page = 4 host pages, so reclaim is 1:4 and exact, with fewer reports for the same memory
  (less CPU). Fine in principle.
- **The real risk is `page_reporting_order`, and it may bite hard.** Reporting granularity follows
  `pageblock_order`, which follows the PMD size — and on arm64 the PMD is **2 MiB at 4 KiB pages but
  512 MiB at 64 KiB pages**. If reporting granularity really becomes 512 MiB, free-page reporting
  could report essentially *nothing* in a 4–8 GiB desktop VM, which would gut dynamic memory. This
  is the single most important thing to measure, and it is cheap: boot a `kernel-64k` guest and read
  `/sys/module/page_reporting/parameters/page_reporting_order`. **Measure this before anything
  else** — a bad answer here decides the whole question on its own.
- **Internal fragmentation is the classic 64 KiB tax**, and a desktop guest is close to the worst
  case for it: page-cache and small-file granularity of 64 KiB means a 1 KiB file occupies 64 KiB of
  page cache. RHEL ships 64k for large-memory/HPC workloads, not for desktops — so their shipping it
  is *not* evidence it suits our workload. Measure RSS and page-cache footprint on a real seated
  session, not a synthetic benchmark, and note that `limina-mem-overhead` already identifies page
  cache as the ratchet in `phys_footprint`.
- **A second-order balloon subtlety worth checking:** virtio-balloon's PFN unit is fixed at 4 KiB by
  the spec (`VIRTIO_BALLOON_PFN_SHIFT` = 12) regardless of guest page size, so a 64 KiB guest must
  inflate in 16-PFN groups. Confirm our inflate/deflate path handles that grouping — it is a
  plausible place for a silent off-by-16.
- **Userspace 64 KiB-cleanliness is unverified**, exactly as 16 KiB-cleanliness is only
  half-verified today (`docs/roadmap.md:2197` — the toolchain is clean, Mesa and the graphics stack
  have not been built). A page-size move re-opens that question for the whole graphics stack.

The honest prior: 64 KiB looks better on alignment, neutral-to-better on balloon *mechanics*, and
potentially much worse on memory footprint — which is one of limina's headline goals. Measure, then
decide; do not adopt it on standardisation grounds alone.

## Recommendation

**Keep the 16 KiB kernel as *the* enhanced tier** — because it is the better tier and because that
tier is getting *more* custom, not because anything requires it. Rewrite the roadmap's "THE
constraint / no host-only fix exists" paragraph accordingly, so the next reader doesn't re-derive a
solved problem.

**Treat "a stock distro needs only the agent" as an explicit goal with a scoreboard.** The table
above is that scoreboard: each upstream landing removes a row. Item 1 of the shopping list — venus
degrading to its stub instance instead of poisoning the loader — pays off on every stock guest
immediately, with or without any of the alignment work, and it is already marked **upstream-now**
in the ledger with a precedent MR cited. It is the obvious first send.

**Do not let the goal overstate itself.** Clipboard on stock GNOME does not converge (see the table
asterisk), and until a distro ships a ≥7.2 kernel *and* a Mesa carrying the rounding patch, stock
guests still need either our kernel or the DKMS module for venus. "Better and better as things land"
is the accurate framing; "works today with only the agent" is not.

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
