# Does the enhanced tier still need a 16 KiB-page guest kernel?

**No. Nothing in the stack requires it, and venus — the one thing that ever truly failed without
it — is fixed host-side.** 16 KiB stays as *the* enhanced tier because it is better, not because
anything is impossible without it. A stock distro working unaided is now gated on one piece of
*userspace* upstream work rather than a chain of five (§ "Just works on a regular distro").

The enhanced kernel RPM's only load-bearing difference from Fedora's is one config symbol,
`CONFIG_ARM64_16K_PAGES=y` (`scripts/provision/f44/build-kernel-rpm.sh:10`): the `liminavm/linux`
`limina` branch went zero-delta when the `page_reporting` freezable-wq UAF fix landed in stable. So
the honest question is whether that one symbol still earns a multi-GB build, and the answer is that
it earns it on performance alone.

## What actually drove the requirement, and what closed it

venus host-visible memory becomes a virtio-gpu blob that the host maps with `hv_vm_map`. Every
operand of that call — host address, guest address, size — must be a multiple of the VM's **stage-2
granule**, and macOS pins that granule to the host page size unless asked otherwise: 16 KiB on Apple
silicon. A 4 KiB guest violates it two independent ways:

| half | who breaks it | status |
|---|---|---|
| **size** — a blob sized `0x21000` is not a granule multiple | host passed the size verbatim | fixed host-side 2026-07-03; libkrun rounds map/unmap identically, **to the granule in force** since 2026-08-27 |
| **offset** — blobs are packed at 4 KiB, so one starts inside a 16 KiB page | guest `virtgpu` vram allocator packs with alignment 0 | fixed host-side 2026-08-27: create the VM with a 4 KiB granule |

`hv_vm_config_set_ipa_granule` (macOS 26+) chooses the granule at VM creation, and limina now
creates every VM at 4 KiB unless its definition says `ipa_granule = "16k"`. Measured on a stock
Fedora 44 guest — stock kernel, stock Mesa, no limina components — venus enumerates and `vkcube`
runs; the same clone with the coarse granule fails `RESOURCE_MAP_BLOB` and reports
`ERROR_OUT_OF_HOST_MEMORY` (`spikes/hv-ipa-granule/RESULTS.md`). The cost is 4-8% on guest
CPU-bound work and nothing where the work is GPU-bound (`perf/2026-08-27-ipa-granule.md`).

**The rule this bought, which generalises past this bug:** every workaround here — rounding blob
sizes in the guest, aligning vram nodes with a DKMS module, negotiating a granule over the virtio
protocol, pooling host-visible memory into one mapped heap — was downstream of a single unexamined
premise, that the stage-2 granule is pinned to the host page size. None of them was wrong given the
premise, and none of them was needed. Before building machinery to work around a platform
constraint, check that the constraint is one.

### The one guard that survives

virglrenderer refuses to map a blob larger than the allocation it was created from
(`vkr_device_memory.c`), rather than publishing whatever host memory follows into the guest. That
was written as the safety half of the size-rounding scheme; it stands on its own as a bound on a
broken or hostile guest, so it stayed when the rounding went.

### A landmine found while the negotiated design was still live

Mesa hardcodes `#define VIRTGPU_PARAM_GUEST_VRAM 9` (`vn_renderer_virtgpu.c:31`, a downstream
ChromeOS param number that never went upstream). Upstream 7.2 has now allocated **9** to
`VIRTGPU_PARAM_BLOB_ALIGNMENT`. On a 7.2 kernel whose device advertises the granule, Mesa asking for
"guest vram" gets `16384` back and reads it as *yes*.

Latent for us — the collision sits in the `else` branch at `vn_renderer_virtgpu.c:1044`, only
reached when `VIRTGPU_PARAM_HOST_VISIBLE` is falsy, and our blobs are host-visible — and we no
longer intend to advertise anything. It is still a real upstream bug for any host-visible-less
configuration, and worth reporting on its own.

## What 16 KiB buys — and why none of it is a *blocker*

The distinction that matters: **venus was a hard failure; everything below is a degradation with a
best-effort path already implemented.**

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
- **TLB pressure**: a perf difference, never a correctness gate — and now a *measured* one, since
  the granule default puts every guest on the fine stage-2 mapping regardless of its page size:
  4-8% on guest CPU-bound work, nothing when GPU-bound (`perf/2026-08-27-ipa-granule.md`). This is
  the whole of what 16 KiB buys today, and it is why a guest that has 16 KiB pages should say so.

So **there is no hard 16 KiB blocker left anywhere in the stack.** 16 KiB is what makes the enhanced
tier *good*, not what makes anything *possible* — which is exactly the shape the two-tier guarantee
asks for.

The one genuine hard failure was venus, and it failed in the worst available way: without a working
blob map, `vkCreateInstance` returns `VK_ERROR_OUT_OF_HOST_MEMORY`, and the Vulkan loader treats OOM
as fatal for the whole instance chain — so it took healthy lavapipe down with it. Stock Vulkan
wasn't degraded, it was *dead*. That asymmetry is why this one item earned "THE constraint" while
the others never did, and the loader's amplification outlives the trigger: see `docs/graphics.md`
§3.3, still open.

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

A stock distro now boots to venus with **nothing installed into it** — the granule is a host
setting. What remains on the list is one item, and it is about resilience rather than capability:

1. **Mesa: venus degrades to its stub instance when ring setup fails.** Our series patch 0003
   (`d517a1b49d1`), the one that converts "all Vulkan dead" into "llvmpipe works". Not upstream;
   `docs/upstreaming/ledger/mesa.md` has it queued as Wave 1. The granule fix removed the failure
   that used to trigger this on every stock guest, so it is no longer *load-bearing* for the stock
   tier — but the loader still amplifies any venus failure into total Vulkan loss, and this is the
   patch that stops it. It remains the highest-value upstream item here.

Three items that were on this list are gone rather than done: rounding blob sizes in guest Mesa,
advertising `blob_alignment` from libkrun, and waiting for a distro kernel ≥ 7.2. They existed to
give a 4 KiB guest an aligned lattice, and the VM no longer needs one. The param-9 collision (§
above) survives as an upstream bug report we owe, not as a dependency.

## The strategy this serves: two tiers that diverge, and a stock tier that converges on "agent only"

Settled with the user 2026-08-15. This is not a plan to *drop* 16 KiB — it is a plan to stop 16 KiB
being load-bearing, so the two tiers can move in opposite directions on purpose:

- **The enhanced tier keeps the 16 KiB kernel and gets *more* custom over time**, not less. It is
  heading toward a fully-owned image — our kernel, bootloader, update mechanism, default compositor
  (see the LiminaOS work; the plan of record lives outside this repo, per the
  `limina-liminaos-prototype` memory). 16 KiB is one of many things we control there, and we keep it
  because it is better: exact 1:1 balloon reclaim, `mach_vm_remap` stitching with no hybrid path,
  and a coarse stage-2 mapping it can ask for by name (`ipa_granule = "16k"`) because it knows its
  own page size.
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
| **`limina-kernel-16k`** | 16 KiB pages (one config symbol — the fork branch carries zero patches as of v7.1.8) | for the *stock* tier: **nothing left** — a stock 4 KiB guest gets venus from the host's granule setting, and pays 4-8% on CPU-bound work for it. For the *enhanced* tier: nothing — we keep building it deliberately, for that 4-8%. |
| **mesa RPM** (8 venus/virgl/zink patches) | venus correctness + WSI + the CPU-write coherency fix | all 8 landing upstream and reaching a distro release. Four are already flagged **upstream-now** in `docs/upstreaming/ledger/mesa.md` (rows 0012, 0013, 0007, 0008); the rest need design work or a conversation. |
| **`guest/virtio-gpu-dkms`** | 16 KiB blob-offset lattice on stock 4 KiB guests | **retired 2026-08-27** by the 4 KiB stage-2 granule. Never wired into the payload; kept as a lab artifact only. |
| **`clipboard@limina`** shell extension | GNOME has no `ext-data-control`, so an unfocused agent cannot touch the clipboard | **M12 — `spice-vdagent`.** This row converges *better* than any other, because the guest component is already installed: `spice-vdagent` ships in the default Fedora Workstation set (verified in the dogfood guest's rpmdb, `0.23.0-1.fc43.aarch64`) and sits dormant purely for want of a named virtio-serial port. Waking it is **~40 lines of host-side limina code** — no guest component, and (contrary to the roadmap's original premise) no new libkrun device either, since `PortConfig::InOut` already announces `VIRTIO_CONSOLE_PORT_NAME`. Spike #1 was GREEN on an unmodified F43 guest with zero limina components. **LANDED 2026-08-15 (#37)**: the host-side broker ships, `limina-agent-session` yields the clipboard wherever a live `spice-vdagent` serves the session, and the extension was deleted — this row is closed, exactly as predicted. |

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

**Keep the 16 KiB kernel as *the* enhanced tier** — because it is the faster tier and because that
tier is getting *more* custom, not because anything requires it. An enhanced guest should carry
`ipa_granule = "16k"` in its definition, which is the only place the page size is now worth
declaring.

**Treat "a stock distro needs only the agent" as an explicit goal with a scoreboard.** The table
above is that scoreboard: each upstream landing removes a row. The venus/kernel rows are gone; what
is left is the mesa RPM, and the one upstream item that makes a venus failure survivable.

**Do not let the goal overstate itself.** A stock guest today gets venus, clipboard and the rest —
but it gets *degraded* venus resilience (a failure still takes the whole loader down) and it pays
the fine-granule tax. "Better and better as things land" remains the accurate framing.

**The adjacent question is now live rather than blocked: can the enhanced tier stop shipping a
custom kernel at all?** We build a multi-GB kernel for one config symbol, and nothing depends on
that symbol any more — it buys 4-8% on CPU-bound work plus exact balloon reclaim and hybrid-free
`mach_vm_remap` stitching. That is a real trade with real numbers on both sides now, and it should
be decided deliberately rather than by drift.

**Concrete:**

1. *(now, cheap)* File the Mesa param-9 collision upstream — it survived the design that found it.
2. *(now)* Send patch 0003 (venus stub instance) upstream; it is marked **upstream-now** in the
   ledger with a precedent MR cited, and it is what stops the next venus failure from being total.
3. *(when someone wants the maintenance win)* Cost out an enhanced tier on the stock kernel against
   the measured 4-8%, and decide.
