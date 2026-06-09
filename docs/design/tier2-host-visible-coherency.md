# Tier-2 #28 — host-visible blob coherency (and why venus renders black)

**Status: RESOLVED (2026-06-08).** Bug A (#31) and #28 are fixed by sharing MoltenVK's own
`vkMapMemory` pointer for HOST_VISIBLE memory instead of an imported shm. Read "The fix" below; the
findings sections under it are kept as the diagnostic record that led there.

## The fix (what shipped)

The defect: the old macOS host-visible path allocated a POSIX shm + Metal buffer
(`newBufferWithBytesNoCopy`), imported it as the `VkDeviceMemory`, and exported the **shm fd**. The
VMM then `mmap`'d that fd a **second** time for the guest — so the guest's CPU mapping and the GPU's
`MTLBuffer` view were two independent mappings that did not stay coherent across `hv_vm_map`. Guest
writes landed in one; the GPU read the other (zeros).

The fix (the krunkit/slp model, ported onto our virglrenderer 1.3.0 tree, branch
`limina/macos-blob-map-ptr`): for HOST_VISIBLE memory, let MoltenVK allocate its **own** Shared
`MTLBuffer`, `vkMapMemory` it, and share **MoltenVK's own pointer** with the VMM (`hv_vm_map`). One
mapping → guest CPU + host GPU coherent. Pieces:

- `virgl_context_blob` / `virgl_resource` gain a `map_ptr`; `resource_map` / `get_map_ptr` return it
  directly, `unmap` is a no-op, and `export_blob` bails (`-EINVAL`) for `map_ptr` resources (the
  `VkDeviceMemory` owns the mapping; `vkFreeMemory`'s implicit unmap releases it — an explicit
  `vkUnmapMemory` in release double-frees and SIGSEGVs at teardown).
- `vkr_device_memory`: HOST_VISIBLE no longer gets a shm carrier; `export_blob` `vkMapMemory`s the
  memory into an `OPAQUE_HANDLE` blob carrying `map_ptr` (`map_info = CACHED`, hardcoded — a `NONE`
  map_info makes `get_map_info` return `-EINVAL` → the guest sees `VK_ERROR_MEMORY_MAP_FAILED`). The
  #30 scanout carrier path is untouched.
- **The render-server proxy boundary** was the load-bearing surprise: it serialized blobs as an
  *fd* and rejected fd-less replies. `map_ptr` is now threaded through it (`render_protocol.h` reply
  struct + `render_context.c` server + `proxy_context.c` client, plus `vkr_renderer_create_resource`
  / `render_state_create_resource` signatures) and an fd-less reply is allowed. This is sound only
  because the render server is a **thread in the same process** (`ENABLE_SAME_PROCESS_RENDER_SERVER`),
  so the host VA is valid on the client side.

**Verification (the oracle):** a guest GLES full-screen quad (`spikes/venus-draw-probe/tri.c`, green
fragment shader over a blue clear) now fills the scanout IOSurface **green** — the guest-written
vertices reach the GPU. Pre-fix it was **blue** (clear only; degenerate geometry). Read host-side via
`iosdump.swift <id>` → uniform `(0,255,0)`. This is EXP-A, but driven by the *guest* writing the data.
Zero-copy, no transfer model.

## TL;DR (original findings)

Venus "renders black" (bug A, #31) is **not a rendering bug** — it is a **data bug**, and it *is* #28:
**the guest's writes to host-visible memory are not visible to the host GPU.** Every draw reads
all-zero vertices → degenerate geometry → no fragments; clears (fixed-function, no guest data) land,
so the screen shows only the clear. The same defect breaks the read direction (GPU→guest readback /
venus feedback), which is how #28 was first seen.

A standing question reframes the whole problem (see "The reframe"): **krunkit runs venus GPU compute on
macOS** (Red Hat's AI-inference work), which *requires* efficient host-visible sharing. So host-visible
coherency on macOS HVF **is achievable** — meaning #28 is most likely a **divergence in our stack**, not
a fundamental HVF limit. This downgrades the prior spike's pessimistic "stage-2 dead end" conclusion to
"unproven; re-check against a working reference."

## What is verified (instrumented host MoltenVK + raw-Vulkan probes; see `spikes/venus-draw-probe/`)

The instrument: `third_party/MoltenVK-src` built with `fprintf` probes (`mvk-instrument.patch`), loaded
into the worker via `VK_ICD_FILENAMES`. Rebuild/boot with `rebuild-mvk.sh` / `boot-mvkinst.sh`.

1. **The draw is perfect at the Metal level.** `[LIMINA-DRAW]`: `prim=triangle, vtxCount=6, instCount=1,
   cull=none, rasterization on, full scissor`. (A negative-height viewport is just zink's normal Y-flip,
   harmless with cull off.) ⇒ the points / rasterizer-discard / `depth_clip_enable` / primitive-restart
   theories are all **disproven by direct observation**, not argument.
2. **The vertex buffer the GPU fetches is entirely zero.** `[LIMINA-VTX]`: `storage=Shared`,
   `firstNonzeroByte=-1` across the whole 1 MB. The guest wrote a valid quad; the host sees zeros.
3. **Host CPU↔GPU is perfectly coherent (EXP-A).** With `LIMINA_FORCE_QUAD=1` the host CPU memcpy's the
   quad into that same buffer → the scanout renders a **full green screen** (read host-side via
   `iosdump.swift`). So everything host-side and downstream (fetch/raster/fragment/present) is fine; the
   defect is **only** the guest→host hop.
4. **Guest-side cache clean alone does NOT fix it (EXP-B, `vkcoh.c`).** Guest writes a host-visible
   buffer, `dc cvac`s it (clean-to-PoC, EL0-permitted), then the GPU copies it; reading the
   **GPU-written** intermediate (host-coherent per the prior spike's Finding 1 — a poison-free oracle)
   shows the GPU read **zero**, with and without the clean. Combined with the prior spike (guest
   *invalidate*-alone was dead for reads), **guest-side cache maintenance alone is insufficient in both
   directions.**

## The mechanism, as far as it is pinned (do not over-claim beyond this)

Host-visible memory (`third_party/virglrenderer/src/venus/vkr_metal_helpers.m`): host allocates an anon
shm (`os_create_anonymous_file` + `mmap(MAP_SHARED)`) and wraps it in a `newBufferWithBytesNoCopy(...,
MTLStorageModeShared)` buffer. libkrun `resource_map_blob` (`virtio_gpu.rs`) then `hv_vm_map`s that host
pointer into the guest's SHM window (`GpuAddMapping`), and returns `map_info = CACHED` so the guest maps
it Write-Back cacheable. venus (`vkr_physical_device.c`) passes MoltenVK's memory props through
**unfiltered**, so the guest sees `HOST_COHERENT` and zink never issues `vkFlushMappedMemoryRanges`.

Net: host CPU, host GPU, and the shm pages are mutually coherent; the **guest's `hv_vm_map`'d CACHED
view is not in the same coherency domain**, and cleaning the guest cache to PoC does not bridge it. What
is **not** pinned: whether it is (i) a coherency-*domain* / shareability gap on the same physical pages,
(ii) genuinely different physical backing, or (iii) a missing explicit sync that `HOST_COHERENT`
suppresses. Each implies a different fix; we have not run the experiment that distinguishes them.

## The reframe (the load-bearing open question)

krunkit ships venus GPU compute on macOS and it produces correct AI-inference results — which is
impossible unless guest writes to host-visible memory reach the GPU. The macOS-venus mechanism it uses
is the same family as ours (host shm + Shared `MTLBuffer` + `hv_vm_map` + CACHED guest mapping; Homebrew's
*plain* virglrenderer has no venus/metal symbols, so the working build is a custom one like ours). So a
working configuration exists, and **we differ from it somewhere.** Candidate divergences, in rough order
of suspicion — none yet tested:

1. **Our custom 16 KiB guest kernel.** The whole #28 title is "16 KiB `hv_vm_map`." If a working krunkit
   guest is 4 KiB-page, the guest virtio-gpu driver's blob mmap (pgprot / shareability / cacheability)
   at 16 KiB may differ. This is the first thing to check.
2. **Our virglrenderer 1.3.0 port.** The macOS-venus code (`vkr_metal_helpers.m`) was carried onto
   upstream 1.3.0; the working reference may set a different storage mode, memory-type filtering, or a
   sync step the port lost. Diff the host-visible path against the reference build.
3. **libkrun's `GpuAddMapping` / `hv_vm_map`** of the blob vs the reference.

## What this changes about the fix landscape

The prior spike (`spikes/mtl-shm-coherency/RESULTS.md`) concluded the guest mapping is fundamentally not
inner-shareable-coherent with the GPU and leaned toward a transfer/copy model (losing zero-copy). The
reframe says: **before accepting that, find our divergence from a working krunkit venus config** — the
clean zero-copy fix may simply be "map the blob the way the working reference does." Only if a like-for-
like working reference also fails on this machine does the deeper-lever / transfer-model discussion
reopen.

## Concrete next steps (grounding, not yet a fix)

1. **Establish a known-working reference on this host.** Get/build the virglrenderer + libkrun + guest
   kernel that krunkit uses for macOS venus compute, and run `vkcoh.c` (our coherency probe) inside it.
   - If coherent there → it's our divergence; bisect 16 KiB-kernel vs 1.3.0-port vs libkrun-map.
   - If it also fails → the deeper-lever / transfer-model discussion reopens with evidence.
2. **Check the 16 KiB angle directly:** compare the guest virtio-gpu driver's host-visible blob mmap
   (pgprot/shareability) on our 16 KiB kernel vs a 4 KiB guest.
3. Keep the disproven theories closed (points / rasterizer-discard / depth_clip / primitive-restart /
   "mutter copies / linear modifier"); they are settled by observation.

## Reproduce

- Instrument + boot: `spikes/venus-draw-probe/rebuild-mvk.sh` then `boot-mvkinst.sh` (multi-user).
- Draw probe: build `tri.c` in the guest (patched zink env, see `limina-fedora-access` memo), run, then
  `swift spikes/venus-draw-probe/iosdump.swift <scanout blob id>`.
- Coherency probe: `vkcoh.c` (raw Vulkan, `-lvulkan`); boot with `LIMINA_COPY_DUMP=1`; the 2nd `[LIMINA-COPY]`
  (GPU-written `mid`) is the reliable oracle.
- ⚠️ Do **not** enable `MTL_DEBUG_LAYER`/`MTL_SHADER_VALIDATION` — they abort the worker on a *separate*
  (#28-adjacent) `replaceRegion bytesPerRow not multiple of 4` defect in MoltenVK's linear-image host sync.
